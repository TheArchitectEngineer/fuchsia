// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashSet;
use std::pin::pin;
use std::sync::Arc;

use assert_matches::assert_matches;
use fidl::endpoints::{ControlHandle as _, ProtocolMarker, RequestStream, Responder as _};
use fidl_fuchsia_net_interfaces_admin::ProofOfInterfaceAuthorization;
use fnet_routes_ext::admin::{FidlRouteAdminIpExt, RouteSetRequest, RouteTableRequest};
use fnet_routes_ext::{FidlRouteIpExt, Responder as _};
use futures::channel::{mpsc, oneshot};
use futures::{Future, FutureExt as _, StreamExt as _, TryStreamExt as _};
use log::{debug, error, warn};
use net_types::ip::Ip;
use netstack3_core::device::DeviceId;
use netstack3_core::routes::AddableEntry;
use thiserror::Error;
use zx::{self as zx, AsHandleRef, HandleBased as _};
use {
    fidl_fuchsia_net_routes_admin as fnet_routes_admin,
    fidl_fuchsia_net_routes_ext as fnet_routes_ext, fuchsia_async as fasync,
};

use crate::bindings::devices::StaticCommonInfo;
use crate::bindings::routes::witness::TableId;
use crate::bindings::routes::{self, RouteWorkItem, WeakDeviceId};
use crate::bindings::util::{ScopeExt as _, TryFromFidlWithContext};
use crate::bindings::{BindingsCtx, Ctx, DeviceIdExt};

pub(crate) use crate::bindings::routes::rules_admin::serve_rule_table;

pub(crate) async fn serve_route_set<
    I: Ip + FidlRouteAdminIpExt + FidlRouteIpExt,
    R: RouteSet<I>,
    C: Future<Output = ()>,
>(
    stream: I::RouteSetRequestStream,
    route_set: &mut R,
    cancel_token: C,
) -> Result<(), fidl::Error> {
    let debug_name =
        <<I::RouteSetRequestStream as RequestStream>::Protocol as ProtocolMarker>::DEBUG_NAME;
    let mut cancel_token = pin!(cancel_token.fuse());
    let control_handle = stream.control_handle();
    let mut stream = stream.map_ok(I::into_route_set_request).into_stream().fuse();
    loop {
        futures::select_biased! {
            () = cancel_token => {
                control_handle.shutdown_with_epitaph(zx::Status::UNAVAILABLE);
                return Ok(());
            },
            request = stream.try_next() => match request? {
                Some(request) => {
                    route_set.handle_request(request).await.unwrap_or_else(|e| {
                        if !e.is_closed() {
                            error!("error handling {debug_name} request: {e:?}");
                        }
                    })
                },
                None => return Ok(()),
            },
        }
    }
}

pub(crate) fn spawn_user_route_set<
    I: Ip + FidlRouteAdminIpExt + FidlRouteIpExt,
    F: FnOnce() -> UserRouteSet<I>,
>(
    scope: &fasync::ScopeHandle,
    stream: I::RouteSetRequestStream,
    create_route_set: F,
) {
    let Some(guard) = scope.active_guard() else {
        warn!("aborted serving user route set because scope is finished");
        stream.control_handle().shutdown_with_epitaph(zx::Status::UNAVAILABLE);
        return;
    };
    let mut user_route_set = create_route_set();
    scope.spawn_request_stream_handler(stream, |stream| async move {
        // Stop serving and hold the guard until we've been able to close
        // the route set.
        let stop = guard.on_cancel();
        let result =
            serve_route_set::<I, UserRouteSet<I>, _>(stream, &mut user_route_set, stop).await;
        user_route_set.close().await;
        std::mem::drop(guard);
        result
    });
}

pub(crate) async fn serve_route_table_provider<I: Ip + FidlRouteIpExt + FidlRouteAdminIpExt>(
    stream: <I::RouteTableProviderMarker as fidl::endpoints::ProtocolMarker>::RequestStream,
    ctx: Ctx,
) -> Result<(), fidl::Error> {
    let mut stream = pin!(stream.map_ok(I::into_route_table_provider_request));

    while let Some(req) = stream.try_next().await? {
        match req {
            fnet_routes_ext::admin::RouteTableProviderRequest::NewRouteTable {
                provider,
                options: fnet_routes_ext::admin::RouteTableOptions { name, _marker: _ },
                control_handle,
            } => {
                let route_table = match ctx.bindings_ctx().routes.add_table::<I>(name).await {
                    Ok((table_id, route_work_sink, token)) => {
                        UserRouteTable::new(ctx.clone(), token, table_id, route_work_sink)
                    }
                    Err(routes::TableIdOverflowsError) => {
                        control_handle.shutdown_with_epitaph(zx::Status::NO_SPACE);
                        break;
                    }
                };
                let stream = provider.into_stream();

                fasync::Scope::current().spawn_request_stream_handler(stream, |stream| {
                    serve_route_table::<I, UserRouteTable<I>>(stream, route_table)
                });
            }
            fnet_routes_ext::admin::RouteTableProviderRequest::GetInterfaceLocalTable {
                credential: _,
                responder,
            } => {
                responder
                    .send(Err(fnet_routes_admin::GetInterfaceLocalTableError::NoLocalRouteTable))?;
            }
        }
    }
    Ok(())
}

pub(crate) async fn serve_route_table<
    I: Ip + FidlRouteAdminIpExt + FidlRouteIpExt,
    R: RouteTable<I>,
>(
    mut stream: <I as FidlRouteAdminIpExt>::RouteTableRequestStream,
    mut route_table: R,
) -> Result<(), fidl::Error> {
    let route_table_ref = &mut route_table;
    let serve = || async move {
        while let Some(request) = stream.try_next().await? {
            match I::into_route_table_request(request) {
                RouteTableRequest::NewRouteSet { route_set, control_handle: _ } => {
                    let set_request_stream = route_set.into_stream();
                    route_table_ref.serve_user_route_set(set_request_stream);
                }
                RouteTableRequest::GetTableId { responder } => {
                    responder.send(route_table_ref.id().into())?;
                }
                RouteTableRequest::Detach { control_handle: _ } => {
                    route_table_ref.detach();
                }
                RouteTableRequest::Remove { responder } => {
                    return Ok(Some(responder));
                }
                RouteTableRequest::GetAuthorizationForRouteTable { responder } => {
                    responder.send(fnet_routes_admin::GrantForRouteTableAuthorization {
                        table_id: route_table_ref.id().into(),
                        token: route_table_ref.token(),
                    })?;
                }
            }
        }
        Ok(None)
    };

    let (result, remove_responder) = match serve().await {
        Ok(responder) => (Ok(()), responder),
        Err(e) => (Err(e), None),
    };

    if route_table.detached() && remove_responder.is_none() {
        debug!(
            "RouteTable protocol for {:?} is shutting down, but the table is detached",
            route_table.id()
        );
        return result;
    }
    // Remove the route table.
    let removed = route_table.remove().await;
    match remove_responder {
        Some(responder) => {
            let fidl_result = match removed {
                Ok(()) | Err(TableRemoveError::Removed) => Ok(()),
                Err(TableRemoveError::InvalidOp) => {
                    Err(fnet_routes_admin::BaseRouteTableRemoveError::InvalidOpOnMainTable)
                }
            };
            responder.send(fidl_result)
        }
        None => {
            match removed {
                Ok(()) | Err(TableRemoveError::Removed) => {}
                // Tables that do not support removing should be `detached` by
                // default.
                Err(e @ TableRemoveError::InvalidOp) => {
                    panic!("unexpected error removing non-detached table: {e:?}");
                }
            }
            result
        }
    }
}

#[derive(Debug)]
pub(crate) enum TableRemoveError {
    Removed,
    InvalidOp,
}

/// The common trait for operations on route table.
///
/// Allows abstracting differences between netstack and user owned tables.
pub(crate) trait RouteTable<I: FidlRouteAdminIpExt + FidlRouteIpExt>: Send + Sync {
    /// Gets the table ID.
    fn id(&self) -> TableId<I>;
    /// Gets the token for authorization.
    fn token(&self) -> zx::Event;
    /// Removes this route table from the system.
    async fn remove(self) -> Result<(), TableRemoveError>;
    /// Detaches the lifetime of the route table from this protocol.
    fn detach(&mut self);
    /// Returns whether the table was detached.
    fn detached(&self) -> bool;
    /// Serves the user route set.
    fn serve_user_route_set(&self, stream: I::RouteSetRequestStream);
}

pub(crate) struct MainRouteTable {
    ctx: Ctx,
}

impl MainRouteTable {
    pub(crate) fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }
}

impl<I: FidlRouteAdminIpExt + FidlRouteIpExt> RouteTable<I> for MainRouteTable {
    fn id(&self) -> TableId<I> {
        routes::main_table_id::<I>()
    }
    fn token(&self) -> zx::Event {
        self.ctx
            .bindings_ctx()
            .routes
            .main_table_token::<I>()
            .duplicate_handle(zx::Rights::TRANSFER | zx::Rights::DUPLICATE)
            .expect("failed to duplicate")
    }
    async fn remove(self) -> Result<(), TableRemoveError> {
        Err(TableRemoveError::InvalidOp)
    }
    fn detach(&mut self) {
        // Nothing to do - main tables are always detached.
    }
    fn detached(&self) -> bool {
        true
    }
    fn serve_user_route_set(&self, stream: I::RouteSetRequestStream) {
        spawn_user_route_set::<I, _>(&fasync::Scope::current(), stream, || {
            UserRouteSet::from_main_table(self.ctx.clone())
        })
    }
}

pub(crate) struct UserRouteTable<I: Ip> {
    ctx: Ctx,
    token: Arc<zx::Event>,
    table_id: TableId<I>,
    route_work_sink: mpsc::UnboundedSender<RouteWorkItem<I>>,
    route_set_scope: fasync::ScopeHandle,
    detached: bool,
}

impl<I: Ip> UserRouteTable<I> {
    fn new(
        ctx: Ctx,
        token: Arc<zx::Event>,
        table_id: TableId<I>,
        route_work_sink: mpsc::UnboundedSender<RouteWorkItem<I>>,
    ) -> Self {
        Self {
            ctx,
            token,
            table_id,
            route_work_sink,
            // Use a detached child for route set so we can just drop
            // `UserRouteTable` and let the parent scope handle joining the
            // route sets.
            route_set_scope: fasync::Scope::current().new_detached_child(format!("{table_id:?}")),
            detached: false,
        }
    }
}

impl<I: FidlRouteAdminIpExt + FidlRouteIpExt> RouteTable<I> for UserRouteTable<I> {
    fn id(&self) -> TableId<I> {
        self.table_id
    }
    fn token(&self) -> zx::Event {
        self.token
            .duplicate_handle(zx::Rights::TRANSFER | zx::Rights::DUPLICATE)
            .expect("failed to duplicate")
    }
    async fn remove(self) -> Result<(), TableRemoveError> {
        let Self { ctx: _, token: _, table_id, route_work_sink, route_set_scope, detached: _ } =
            self;
        let (responder, receiver) = oneshot::channel();
        let work_item = RouteWorkItem {
            change: routes::Change::RemoveTable(table_id),
            responder: Some(responder),
        };
        match route_work_sink.unbounded_send(work_item) {
            Ok(()) => {
                let result = receiver.await.expect("responder should not be dropped");
                assert_matches!(
                    result,
                    Ok(routes::ChangeOutcome::Changed | routes::ChangeOutcome::NoChange)
                );
                route_set_scope.cancel().await;
                Ok(())
            }
            Err(e) => {
                let _: mpsc::TrySendError<_> = e;
                Err(TableRemoveError::Removed)
            }
        }
    }
    fn detach(&mut self) {
        self.detached = true;
    }
    fn detached(&self) -> bool {
        self.detached
    }
    fn serve_user_route_set(&self, stream: I::RouteSetRequestStream) {
        spawn_user_route_set::<I, _>(&self.route_set_scope, stream, || {
            UserRouteSet::new(self.ctx.clone(), self.table_id, self.route_work_sink.clone())
        })
    }
}

#[derive(Debug)]
pub(crate) struct UserRouteSetId<I: Ip> {
    table_id: TableId<I>,
}

pub(crate) type WeakUserRouteSet<I> = netstack3_core::sync::WeakRc<UserRouteSetId<I>>;
pub(crate) type StrongUserRouteSet<I> = netstack3_core::sync::StrongRc<UserRouteSetId<I>>;

impl<I: Ip> UserRouteSetId<I> {
    #[cfg(test)]
    pub(crate) fn new(table_id: TableId<I>) -> Self {
        Self { table_id }
    }

    pub(super) fn table(&self) -> TableId<I> {
        self.table_id
    }
}

#[must_use = "UserRouteSets must explicitly have `.close()` called on them before dropping them"]
pub(crate) struct UserRouteSet<I: Ip> {
    ctx: Ctx,
    set: Option<netstack3_core::sync::PrimaryRc<UserRouteSetId<I>>>,
    route_work_sink: mpsc::UnboundedSender<RouteWorkItem<I>>,
    authorization_set: HashSet<WeakDeviceId>,
}

impl<I: Ip> Drop for UserRouteSet<I> {
    fn drop(&mut self) {
        if self.set.is_some() {
            panic!("UserRouteSet must not be dropped without calling close()");
        }
    }
}

impl<I: FidlRouteAdminIpExt> UserRouteSet<I> {
    #[cfg_attr(feature = "instrumented", track_caller)]
    pub(crate) fn new(
        ctx: Ctx,
        table: TableId<I>,
        route_work_sink: mpsc::UnboundedSender<RouteWorkItem<I>>,
    ) -> Self {
        let set = netstack3_core::sync::PrimaryRc::new(UserRouteSetId { table_id: table });
        Self { ctx, set: Some(set), authorization_set: HashSet::new(), route_work_sink }
    }

    pub(crate) fn from_main_table(ctx: Ctx) -> Self {
        let route_work_sink = ctx.bindings_ctx().routes.main_table_route_work_sink::<I>().clone();
        Self::new(ctx, routes::main_table_id::<I>(), route_work_sink)
    }

    fn weak_set_id(&self) -> netstack3_core::sync::WeakRc<UserRouteSetId<I>> {
        netstack3_core::sync::PrimaryRc::downgrade(
            self.set.as_ref().expect("close() can't have been called because it takes ownership"),
        )
    }

    pub(crate) async fn close(mut self) {
        fn consume_outcome(result: Result<routes::ChangeOutcome, routes::ChangeError>) {
            match result {
                Ok(outcome) => match outcome {
                    routes::ChangeOutcome::Changed | routes::ChangeOutcome::NoChange => {
                        // We don't care what the outcome was as long as it succeeded.
                    }
                },
                Err(err) => match err {
                    routes::ChangeError::TableRemoved => {
                        // We don't care if the backing route table has been
                        // closed; we're already closing the route set.
                    }
                    routes::ChangeError::DeviceRemoved => {
                        unreachable!("closing a route set should not require upgrading a DeviceId")
                    }
                    routes::ChangeError::SetRemoved => {
                        unreachable!(
                            "SetRemoved should not be observable while closing a route set, \
                            as `RouteSet::close()` takes ownership of `self` and thus can't be \
                            called twice on the same RouteSet"
                        )
                    }
                },
            }
        }

        consume_outcome(
            self.apply_route_change(routes::Change::RemoveSet(self.weak_set_id())).await,
        );

        let UserRouteSet { ctx: _, set, authorization_set: _, route_work_sink: _ } = &mut self;
        let UserRouteSetId { table_id: _ } = netstack3_core::sync::PrimaryRc::unwrap(
            set.take().expect("close() can't be called twice"),
        );
    }
}

impl<I: Ip + FidlRouteAdminIpExt> RouteSet<I> for UserRouteSet<I> {
    fn set(&self) -> routes::SetMembership<netstack3_core::sync::WeakRc<UserRouteSetId<I>>> {
        routes::SetMembership::User(self.weak_set_id())
    }

    fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    fn authorization_set(&self) -> &HashSet<WeakDeviceId> {
        &self.authorization_set
    }

    fn authorization_set_mut(&mut self) -> &mut HashSet<WeakDeviceId> {
        &mut self.authorization_set
    }

    fn route_work_sink(&self) -> &mpsc::UnboundedSender<RouteWorkItem<I>> {
        &self.route_work_sink
    }
}

pub(crate) struct GlobalRouteSet {
    ctx: Ctx,
    authorization_set: HashSet<WeakDeviceId>,
}

impl GlobalRouteSet {
    #[cfg_attr(feature = "instrumented", track_caller)]
    pub(crate) fn new(ctx: Ctx) -> Self {
        Self { ctx, authorization_set: HashSet::new() }
    }
}

impl<I: FidlRouteAdminIpExt> RouteSet<I> for GlobalRouteSet {
    fn set(
        &self,
    ) -> routes::SetMembership<netstack3_core::sync::WeakRc<routes::admin::UserRouteSetId<I>>> {
        routes::SetMembership::Global
    }

    fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    fn authorization_set(&self) -> &HashSet<WeakDeviceId> {
        &self.authorization_set
    }

    fn authorization_set_mut(&mut self) -> &mut HashSet<WeakDeviceId> {
        &mut self.authorization_set
    }

    fn route_work_sink(&self) -> &mpsc::UnboundedSender<RouteWorkItem<I>> {
        // TODO(https://fxbug.dev/339567592): GlobalRouteSet should be aware of
        // the route table as well.
        &self.ctx.bindings_ctx().routes.main_table_route_work_sink::<I>()
    }
}

#[derive(Error, Debug)]
pub(crate) enum ModifyTableError {
    #[error("backing route table is removed")]
    TableRemoved,
    #[error("route set error: {0:?}")]
    RouteSetError(fnet_routes_admin::RouteSetError),
    #[error("fidl error: {0:?}")]
    Fidl(#[from] fidl::Error),
}

pub(crate) trait RouteSet<I: FidlRouteAdminIpExt>: Send + Sync {
    fn set(&self) -> routes::SetMembership<netstack3_core::sync::WeakRc<UserRouteSetId<I>>>;
    fn ctx(&self) -> &Ctx;
    fn authorization_set(&self) -> &HashSet<WeakDeviceId>;
    fn authorization_set_mut(&mut self) -> &mut HashSet<WeakDeviceId>;
    fn route_work_sink(&self) -> &mpsc::UnboundedSender<RouteWorkItem<I>>;

    async fn handle_request(&mut self, request: RouteSetRequest<I>) -> Result<(), fidl::Error> {
        debug!("RouteSet::handle_request {request:?}");

        match request {
            RouteSetRequest::AddRoute { route, responder } => {
                let route = match route {
                    Ok(route) => route,
                    Err(e) => {
                        return responder.send(Err(e.into()));
                    }
                };

                match self.add_fidl_route(route).await {
                    Ok(modified) => responder.send(Ok(modified)),
                    Err(ModifyTableError::Fidl(err)) => Err(err),
                    Err(ModifyTableError::RouteSetError(err)) => responder.send(Err(err)),
                    Err(ModifyTableError::TableRemoved) => {
                        responder.control_handle().shutdown_with_epitaph(zx::Status::UNAVAILABLE);
                        Ok(())
                    }
                }
            }
            RouteSetRequest::RemoveRoute { route, responder } => {
                let route = match route {
                    Ok(route) => route,
                    Err(e) => {
                        return responder.send(Err(e.into()));
                    }
                };

                match self.remove_fidl_route(route).await {
                    Ok(modified) => responder.send(Ok(modified)),
                    Err(ModifyTableError::Fidl(err)) => Err(err),
                    Err(ModifyTableError::RouteSetError(err)) => responder.send(Err(err)),
                    Err(ModifyTableError::TableRemoved) => {
                        responder.control_handle().shutdown_with_epitaph(zx::Status::UNAVAILABLE);
                        Ok(())
                    }
                }
            }
            RouteSetRequest::AuthenticateForInterface { credential, responder } => {
                responder.send(self.authenticate_for_interface(credential))
            }
        }
    }

    async fn apply_route_op(
        &self,
        op: routes::RouteOp<I::Addr>,
    ) -> Result<routes::ChangeOutcome, routes::ChangeError> {
        self.apply_route_change(routes::Change::RouteOp(op, self.set())).await
    }

    async fn apply_route_change(
        &self,
        change: routes::Change<I>,
    ) -> Result<routes::ChangeOutcome, routes::ChangeError> {
        let sender = self.route_work_sink();
        let (responder, receiver) = oneshot::channel();
        let work_item = RouteWorkItem { change, responder: Some(responder) };
        match sender.unbounded_send(work_item) {
            Ok(()) => receiver.await.expect("responder should not be dropped"),
            Err(e) => {
                let _: mpsc::TrySendError<_> = e;
                Err(routes::ChangeError::TableRemoved)
            }
        }
    }

    async fn add_fidl_route(
        &self,
        route: fnet_routes_ext::Route<I>,
    ) -> Result<bool, ModifyTableError> {
        let addable_entry = try_to_addable_entry::<I>(self.ctx().bindings_ctx(), route)
            .map_err(ModifyTableError::RouteSetError)?
            .map_device_id(|d| d.downgrade());

        if !self.authorization_set().contains(&addable_entry.device) {
            return Err(ModifyTableError::RouteSetError(
                fnet_routes_admin::RouteSetError::Unauthenticated,
            ));
        }

        let result = self.apply_route_op(routes::RouteOp::Add(addable_entry)).await;

        match result {
            Ok(outcome) => match outcome {
                routes::ChangeOutcome::NoChange => Ok(false),
                routes::ChangeOutcome::Changed => Ok(true),
            },
            Err(err) => match err {
                routes::ChangeError::DeviceRemoved => Err(
                    ModifyTableError::RouteSetError(fnet_routes_admin::RouteSetError::PreviouslyAuthenticatedInterfaceNoLongerExists),
                ),
                routes::ChangeError::TableRemoved => Err(
                    ModifyTableError::TableRemoved
                ),
                routes::ChangeError::SetRemoved => unreachable!(
                    "SetRemoved should not be observable while holding a route set, \
                    as `RouteSet::close()` takes ownership of `self`"
                ),
            },
        }
    }

    async fn remove_fidl_route(
        &self,
        route: fnet_routes_ext::Route<I>,
    ) -> Result<bool, ModifyTableError> {
        let AddableEntry { subnet, device, gateway, metric } =
            try_to_addable_entry::<I>(self.ctx().bindings_ctx(), route)
                .map_err(ModifyTableError::RouteSetError)?
                .map_device_id(|d| d.downgrade());

        if !self.authorization_set().contains(&device) {
            return Err(ModifyTableError::RouteSetError(
                fnet_routes_admin::RouteSetError::Unauthenticated,
            ));
        }

        let result = self
            .apply_route_op(routes::RouteOp::RemoveMatching {
                subnet,
                device,
                gateway: routes::Matcher::Exact(gateway),
                metric: routes::Matcher::Exact(metric),
            })
            .await;

        match result {
            Ok(outcome) => match outcome {
                routes::ChangeOutcome::NoChange => Ok(false),
                routes::ChangeOutcome::Changed => Ok(true),
            },
            Err(err) => match err {
                routes::ChangeError::DeviceRemoved => Err(
                    ModifyTableError::RouteSetError(fnet_routes_admin::RouteSetError::PreviouslyAuthenticatedInterfaceNoLongerExists),
                ),
                routes::ChangeError::TableRemoved => Err(
                    ModifyTableError::TableRemoved
                ),
                routes::ChangeError::SetRemoved => unreachable!(
                    "SetRemoved should not be observable while holding a route set, \
                    as `RouteSet::close()` takes ownership of `self`"
                ),
            },
        }
    }

    fn authenticate_for_interface(
        &mut self,
        client_credential: ProofOfInterfaceAuthorization,
    ) -> Result<(), fnet_routes_admin::AuthenticateForInterfaceError> {
        let bindings_id = client_credential
            .interface_id
            .try_into()
            .map_err(|_| fnet_routes_admin::AuthenticateForInterfaceError::InvalidAuthentication)?;

        let core_id =
            self.ctx().bindings_ctx().devices.get_core_id(bindings_id).ok_or_else(|| {
                warn!("authentication interface {bindings_id} does not exist");
                fnet_routes_admin::AuthenticateForInterfaceError::InvalidAuthentication
            })?;

        let external_state = core_id.external_state();
        let StaticCommonInfo { authorization_token: netstack_token } =
            external_state.static_common_info();

        let netstack_koid = netstack_token
            .basic_info()
            .expect("failed to get basic info for netstack-owned token")
            .koid;

        let client_koid = client_credential
            .token
            .basic_info()
            .map_err(|e| {
                error!("failed to get basic info for client-provided token: {}", e);
                fnet_routes_admin::AuthenticateForInterfaceError::InvalidAuthentication
            })?
            .koid;

        if netstack_koid == client_koid {
            let authorization_set = self.authorization_set_mut();

            // Prune any devices that no longer exist.  Since we store
            // weak references, we only need to check whether any given
            // reference can be upgraded.
            authorization_set.retain(|k| k.upgrade().is_some());

            // Insert after pruning the map to avoid a needless call to upgrade.
            let _ = authorization_set.insert(core_id.downgrade());

            Ok(())
        } else {
            Err(fnet_routes_admin::AuthenticateForInterfaceError::InvalidAuthentication)
        }
    }
}

fn try_to_addable_entry<I: Ip>(
    bindings_ctx: &crate::bindings::BindingsCtx,
    route: fnet_routes_ext::Route<I>,
) -> Result<AddableEntry<I::Addr, DeviceId<BindingsCtx>>, fnet_routes_admin::RouteSetError> {
    AddableEntry::try_from_fidl_with_ctx(bindings_ctx, route).map_err(|err| match err {
        crate::bindings::util::AddableEntryFromRoutesExtError::DeviceNotFound => {
            fnet_routes_admin::RouteSetError::PreviouslyAuthenticatedInterfaceNoLongerExists
        }
        crate::bindings::util::AddableEntryFromRoutesExtError::UnknownAction => {
            fnet_routes_admin::RouteSetError::UnsupportedAction
        }
    })
}

// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use fidl_fuchsia_developer_ffx as ffx;
use futures::StreamExt;
use std::convert::Infallible;
use std::time::{Duration, Instant};
use timeout::timeout;

#[cfg(feature = "fdomain")]
use {
    fdomain_client::fidl::{DiscoverableProtocolMarker, Proxy},
    fdomain_fuchsia_developer_remotecontrol::{
        ConnectCapabilityError, RemoteControlMarker, RemoteControlProxy,
    },
    fdomain_fuchsia_kernel as proto_fuchsia_kernel,
    fdomain_fuchsia_sys2::{
        ConfigOverrideMarker, ConfigOverrideProxy, LifecycleControllerMarker,
        LifecycleControllerProxy, RealmQueryMarker, RealmQueryProxy, RouteValidatorMarker,
        RouteValidatorProxy,
    },
};

#[cfg(not(feature = "fdomain"))]
use {
    fidl::endpoints::{DiscoverableProtocolMarker, ProxyHasDomain},
    fidl_fuchsia_developer_remotecontrol::{
        ConnectCapabilityError, IdentifyHostError, IdentifyHostResponse, RemoteControlMarker,
        RemoteControlProxy,
    },
    fidl_fuchsia_kernel as proto_fuchsia_kernel,
    fidl_fuchsia_overnet_protocol::NodeId,
    fidl_fuchsia_sys2::{
        ConfigOverrideMarker, ConfigOverrideProxy, LifecycleControllerMarker,
        LifecycleControllerProxy, RealmQueryMarker, RealmQueryProxy, RouteValidatorMarker,
        RouteValidatorProxy,
    },
    std::hash::{Hash, Hasher},
    std::sync::Arc,
    timeout::TimeoutError,
};

#[cfg(feature = "fdomain")]
pub use fdomain_fuchsia_sys2::OpenDirType;

#[cfg(not(feature = "fdomain"))]
pub use fidl_fuchsia_sys2::OpenDirType;

pub mod toolbox;

/// Note that this is only used for backwards compatibility. All new usages should prefer using the
/// toolbox moniker instead.
const REMOTE_CONTROL_MONIKER: &str = "core/remote-control";

#[cfg(not(feature = "fdomain"))]
const IDENTIFY_HOST_TIMEOUT_MILLIS: u64 = 10000;

#[cfg(not(feature = "fdomain"))]
#[derive(Debug, Clone)]
pub struct RcsConnection {
    pub node: Arc<overnet_core::Router>,
    pub proxy: RemoteControlProxy,
    pub overnet_id: NodeId,
}

#[cfg(not(feature = "fdomain"))]
impl Hash for RcsConnection {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        self.overnet_id.id.hash(state)
    }
}

#[cfg(not(feature = "fdomain"))]
impl PartialEq for RcsConnection {
    fn eq(&self, other: &Self) -> bool {
        self.overnet_id == other.overnet_id
    }
}

#[cfg(not(feature = "fdomain"))]
impl Eq for RcsConnection {}

#[cfg(not(feature = "fdomain"))]
impl RcsConnection {
    pub fn new(node: Arc<overnet_core::Router>, id: &mut NodeId) -> Result<Self> {
        let (s, p) = fidl::Channel::create();
        let _result = RcsConnection::connect_to_service(Arc::clone(&node), id, s)?;
        let proxy = RemoteControlProxy::new(fidl::AsyncChannel::from_channel(p));

        Ok(Self { node, proxy, overnet_id: id.clone() })
    }

    pub fn copy_to_channel(&mut self, channel: fidl::Channel) -> Result<()> {
        RcsConnection::connect_to_service(Arc::clone(&self.node), &mut self.overnet_id, channel)
    }

    fn connect_to_service(
        node: Arc<overnet_core::Router>,
        overnet_id: &mut NodeId,
        channel: fidl::Channel,
    ) -> Result<()> {
        let overnet_id = (*overnet_id).into();
        // TODO(b/302394849): If this method were async we could return the
        // error instead of just logging it. This task used to be managed by
        // Hoist where we couldn't get to it, but now we have it right here
        // where it would be easy to factor out.
        fuchsia_async::Task::spawn(async move {
            if let Err(e) = node
                .connect_to_service(overnet_id, RemoteControlMarker::PROTOCOL_NAME, channel)
                .await
            {
                log::warn!("Error connecting to Rcs: {}", e)
            }
        })
        .detach();
        Ok(())
    }

    // Primarily For testing.
    pub fn new_with_proxy(
        node: Arc<overnet_core::Router>,
        proxy: RemoteControlProxy,
        id: &NodeId,
    ) -> Self {
        Self { node, proxy, overnet_id: id.clone() }
    }

    pub async fn identify_host(&self) -> Result<IdentifyHostResponse, RcsConnectionError> {
        log::debug!("Requesting host identity from overnet id {}", self.overnet_id.id);
        let identify_result = timeout(
            Duration::from_millis(IDENTIFY_HOST_TIMEOUT_MILLIS),
            self.proxy.identify_host(),
        )
        .await
        .map_err(|e| RcsConnectionError::ConnectionTimeoutError(e))?;

        let identify = match identify_result {
            Ok(res) => match res {
                Ok(target) => target,
                Err(e) => return Err(RcsConnectionError::RemoteControlError(e)),
            },
            Err(e) => return Err(RcsConnectionError::FidlConnectionError(e)),
        };

        Ok(identify)
    }
}

#[cfg(not(feature = "fdomain"))]
#[derive(Debug)]
pub enum RcsConnectionError {
    /// There is something wrong with the FIDL connection.
    FidlConnectionError(fidl::Error),
    /// There was a timeout trying to communicate with RCS.
    ConnectionTimeoutError(TimeoutError),
    /// There is an error from within Rcs itself.
    RemoteControlError(IdentifyHostError),

    /// There is an error with the output from Rcs.
    TargetError(anyhow::Error),
}

#[cfg(not(feature = "fdomain"))]
impl std::fmt::Display for RcsConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RcsConnectionError::FidlConnectionError(ferr) => {
                write!(f, "fidl connection error: {}", ferr)
            }
            RcsConnectionError::ConnectionTimeoutError(_) => write!(f, "timeout error"),
            RcsConnectionError::RemoteControlError(ierr) => write!(f, "internal error: {:?}", ierr),
            RcsConnectionError::TargetError(error) => write!(f, "general error: {}", error),
        }
    }
}

pub const RCS_KNOCK_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(thiserror::Error, Debug)]
pub enum KnockRcsError {
    #[error("FIDL error {0:?}")]
    FidlError(#[from] fidl::Error),
    #[cfg(feature = "fdomain")]
    #[error("FDomain error {0:?}")]
    FDomainError(#[from] fdomain_client::Error),
    #[error("Creating FIDL channel: {0:?}")]
    ChannelError(#[from] fidl::handle::Status),
    #[error("Connecting to RCS {0:?}")]
    RcsConnectCapabilityError(ConnectCapabilityError),
    #[error("Could not knock service from RCS")]
    FailedToKnock,
}

impl From<Infallible> for KnockRcsError {
    fn from(_value: Infallible) -> Self {
        unreachable!()
    }
}

/// Attempts to "knock" RCS.
///
/// This can be used to verify whether it is up and running, or as a control flow to ensure that
/// RCS is up and running before continuing time-sensitive operations.
// TODO(b/339266778): Use non-FIDL error type.
pub async fn knock_rcs(rcs_proxy: &RemoteControlProxy) -> Result<(), ffx::TargetConnectionError> {
    knock_rcs_impl(rcs_proxy).await.map_err(|e| match e {
        KnockRcsError::FidlError(e) => {
            log::warn!("FIDL error: {:?}", e);
            ffx::TargetConnectionError::FidlCommunicationError
        }
        #[cfg(feature = "fdomain")]
        KnockRcsError::FDomainError(e) => {
            log::warn!("FDomain error: {:?}", e);
            ffx::TargetConnectionError::FidlCommunicationError
        }
        KnockRcsError::ChannelError(e) => {
            log::warn!("RCS connect channel err: {:?}", e);
            ffx::TargetConnectionError::FidlCommunicationError
        }
        KnockRcsError::RcsConnectCapabilityError(c) => {
            log::warn!("RCS failed connecting to itself for knocking: {:?}", c);
            ffx::TargetConnectionError::RcsConnectionError
        }
        KnockRcsError::FailedToKnock => ffx::TargetConnectionError::FailedToKnockService,
    })
}

#[cfg(not(feature = "fdomain"))]
type KnockClientType = fidl::client::Client<fidl::encoding::DefaultFuchsiaResourceDialect>;

#[cfg(feature = "fdomain")]
type KnockClientType = fidl::client::Client<fdomain_client::fidl::FDomainResourceDialect>;

async fn connect_to_rcs(
    rcs_proxy: &RemoteControlProxy,
    moniker: &str,
    capability_set: OpenDirType,
    capability_name: &str,
) -> Result<KnockClientType, KnockRcsError> {
    let rcs_client = rcs_proxy.domain();
    // Try to connect via fuchsia.developer.remotecontrol/RemoteControl.ConnectCapability.
    let (client, server) = rcs_client.create_channel();
    #[cfg(not(feature = "fdomain"))]
    let client = fuchsia_async::Channel::from_channel(client);
    if let Ok(response) =
        rcs_proxy.connect_capability(moniker, capability_set, capability_name, server).await
    {
        response.map_err(|e| KnockRcsError::RcsConnectCapabilityError(e))?;
        return Ok(KnockClientType::new(client, "knock_client"));
    }
    // Fallback to fuchsia.developer.remotecontrol/RemoteControl.DeprecatedOpenCapability.
    // This can be removed once we drop support for API level 27.
    let (client, server) = rcs_proxy.domain().create_channel();
    #[cfg(not(feature = "fdomain"))]
    let client = fuchsia_async::Channel::from_channel(client);
    rcs_proxy
        .deprecated_open_capability(
            moniker,
            capability_set,
            capability_name,
            server,
            Default::default(),
        )
        .await?
        .map_err(|e| KnockRcsError::RcsConnectCapabilityError(e))?;
    return Ok(KnockClientType::new(client, "knock_client"));
}

async fn knock_rcs_impl(rcs_proxy: &RemoteControlProxy) -> Result<(), KnockRcsError> {
    let knock_client = match connect_to_rcs(
        rcs_proxy,
        toolbox::MONIKER,
        OpenDirType::NamespaceDir,
        &format!("svc/{}", RemoteControlMarker::PROTOCOL_NAME),
    )
    .await
    {
        Ok(client) => client,
        Err(KnockRcsError::RcsConnectCapabilityError(_)) => {
            // Fallback to the legacy moniker if toolbox doesn't contain the capability.
            connect_to_rcs(
                rcs_proxy,
                REMOTE_CONTROL_MONIKER,
                OpenDirType::ExposedDir,
                RemoteControlMarker::PROTOCOL_NAME,
            )
            .await?
        }
        Err(e) => return Err(e),
    };

    let mut event_receiver = knock_client.take_event_receiver();
    let res = timeout(RCS_KNOCK_TIMEOUT, event_receiver.next()).await;
    match res {
        // no events are expected -- the only reason we'll get an event is if
        // channel closes. So the only valid response here is a timeout.
        Err(_) => Ok(()),
        Ok(r) => r.ok_or(KnockRcsError::FailedToKnock).map(drop),
    }
}

#[cfg(not(feature = "fdomain"))]
pub trait ProtocolMarker: fidl::endpoints::ProtocolMarker {}

#[cfg(feature = "fdomain")]
pub trait ProtocolMarker: fdomain_client::fidl::ProtocolMarker {}

#[cfg(not(feature = "fdomain"))]
impl<T> ProtocolMarker for T where T: fidl::endpoints::ProtocolMarker {}

#[cfg(feature = "fdomain")]
impl<T> ProtocolMarker for T where T: fdomain_client::fidl::ProtocolMarker {}

pub async fn open_with_timeout_at<T: ProtocolMarker>(
    dur: Duration,
    moniker: &str,
    capability_set: OpenDirType,
    capability_name: &str,
    rcs_proxy: &RemoteControlProxy,
) -> Result<T::Proxy> {
    let connect_capability_fut = async move {
        // Try to connect via fuchsia.developer.remotecontrol/RemoteControl.ConnectCapability.
        let (proxy, server) = rcs_proxy.domain().create_proxy::<T>();
        if let Ok(Ok(())) = rcs_proxy
            .connect_capability(moniker, capability_set, capability_name, server.into_channel())
            .await
        {
            return Ok(Ok(proxy));
        }
        // Fallback to fuchsia.developer.remotecontrol/RemoteControl.DeprecatedOpenCapability.
        // This can be removed once we drop support for API level 27.
        let (proxy, server) = rcs_proxy.domain().create_proxy::<T>();
        rcs_proxy
            .deprecated_open_capability(
                moniker,
                capability_set,
                capability_name,
                server.into_channel(),
                Default::default(),
            )
            .await
            .map(|result| result.map(|_| proxy))
    };
    if let Ok(result) = timeout::timeout(dur, connect_capability_fut).await {
        let fidl_result = result.map_err(|e| anyhow::anyhow!(e))?;
        return fidl_result.map_err(|e| {
                    match e {
                        ConnectCapabilityError::NoMatchingCapabilities => {
                            errors::ffx_error!(format!(
"The plugin service did not match any capabilities on the target for moniker '{moniker}' and
capability '{capability_name}'.

It is possible that the expected component is either not built into the system image, or that the
package server has not been setup.

For users, ensure your Fuchsia device is registered with ffx. To do this you can run:

$ ffx target repository register -r $IMAGE_TYPE --alias fuchsia.com

For plugin developers, it may be possible that the moniker you're attempting to connect to is
incorrect.
You can use `ffx component explore '<moniker>'` to explore the component topology
of your target device to fix this moniker if this is the case.

If you believe you have encountered a bug after walking through the above please report it at
https://fxbug.dev/new/ffx+User+Bug")).into()
                        }
                        _ => {
                            anyhow::anyhow!(
                                format!("This service dependency exists but connecting to it failed with error {e:?}. Moniker: {moniker}. Capability name: {capability_name}")
                            )
                        }
                    }
                });
    } else {
        return Err(errors::ffx_error!("Timed out connecting to capability: '{capability_name}'
with moniker: '{moniker}'.
This is likely due to a sudden shutdown or disconnect of the target.
If you have encountered what you think is a bug, Please report it at https://fxbug.dev/new/ffx+User+Bug

To diagnose the issue, use `ffx doctor`.").into());
    }
}

pub async fn connect_with_timeout_at<T: ProtocolMarker>(
    dur: Duration,
    moniker: &str,
    capability_name: &str,
    rcs_proxy: &RemoteControlProxy,
) -> Result<T::Proxy> {
    open_with_timeout_at::<T>(dur, moniker, OpenDirType::ExposedDir, capability_name, rcs_proxy)
        .await
}

pub async fn connect_with_timeout<P: ProtocolMarker + DiscoverableProtocolMarker>(
    dur: Duration,
    moniker: &str,
    rcs_proxy: &RemoteControlProxy,
) -> Result<P::Proxy> {
    open_with_timeout_at::<P>(dur, moniker, OpenDirType::ExposedDir, P::PROTOCOL_NAME, rcs_proxy)
        .await
}

pub async fn connect_to_protocol<P: DiscoverableProtocolMarker>(
    dur: Duration,
    moniker: &str,
    rcs_proxy: &RemoteControlProxy,
) -> Result<P::Proxy> {
    connect_with_timeout::<P>(dur, moniker, rcs_proxy).await
}

pub async fn open_with_timeout<P: DiscoverableProtocolMarker>(
    dur: Duration,
    moniker: &str,
    capability_set: OpenDirType,
    rcs_proxy: &RemoteControlProxy,
) -> Result<P::Proxy> {
    open_with_timeout_at::<P>(dur, moniker, capability_set, P::PROTOCOL_NAME, rcs_proxy).await
}

async fn get_cf_root_from_namespace<M: DiscoverableProtocolMarker>(
    rcs_proxy: &RemoteControlProxy,
    timeout: Duration,
) -> Result<M::Proxy> {
    let start_time = Instant::now();
    let res = open_with_timeout_at::<M>(
        timeout,
        toolbox::MONIKER,
        OpenDirType::NamespaceDir,
        &format!("svc/{}.root", M::PROTOCOL_NAME),
        rcs_proxy,
    )
    .await;
    // Fallback to the legacy remote control moniker if toolbox doesn't contain the capability.
    match res {
        Ok(proxy) => Ok(proxy),
        Err(_) => {
            let timeout = timeout.saturating_sub(Instant::now() - start_time);
            open_with_timeout_at::<M>(
                timeout,
                REMOTE_CONTROL_MONIKER,
                OpenDirType::NamespaceDir,
                &format!("svc/{}.root", M::PROTOCOL_NAME),
                rcs_proxy,
            )
            .await
        }
    }
}

pub async fn kernel_stats(
    rcs_proxy: &RemoteControlProxy,
    timeout: Duration,
) -> Result<proto_fuchsia_kernel::StatsProxy> {
    let start_time = Instant::now();
    let res = open_with_timeout_at::<proto_fuchsia_kernel::StatsMarker>(
        timeout,
        toolbox::MONIKER,
        OpenDirType::NamespaceDir,
        &format!("svc/{}", proto_fuchsia_kernel::StatsMarker::PROTOCOL_NAME),
        rcs_proxy,
    )
    .await;
    // Fallback to the legacy remote control moniker if toolbox doesn't contain the capability.
    match res {
        Ok(proxy) => Ok(proxy),
        Err(_) => {
            let timeout = timeout.saturating_sub(Instant::now() - start_time);
            open_with_timeout_at::<proto_fuchsia_kernel::StatsMarker>(
                timeout,
                REMOTE_CONTROL_MONIKER,
                OpenDirType::NamespaceDir,
                &format!("svc/{}", proto_fuchsia_kernel::StatsMarker::PROTOCOL_NAME),
                rcs_proxy,
            )
            .await
        }
    }
}

pub async fn root_config_override(
    rcs_proxy: &RemoteControlProxy,
    timeout: Duration,
) -> Result<ConfigOverrideProxy> {
    get_cf_root_from_namespace::<ConfigOverrideMarker>(rcs_proxy, timeout).await
}

pub async fn root_realm_query(
    rcs_proxy: &RemoteControlProxy,
    timeout: Duration,
) -> Result<RealmQueryProxy> {
    get_cf_root_from_namespace::<RealmQueryMarker>(rcs_proxy, timeout).await
}

pub async fn root_lifecycle_controller(
    rcs_proxy: &RemoteControlProxy,
    timeout: Duration,
) -> Result<LifecycleControllerProxy> {
    get_cf_root_from_namespace::<LifecycleControllerMarker>(rcs_proxy, timeout).await
}

pub async fn root_route_validator(
    rcs_proxy: &RemoteControlProxy,
    timeout: Duration,
) -> Result<RouteValidatorProxy> {
    get_cf_root_from_namespace::<RouteValidatorMarker>(rcs_proxy, timeout).await
}

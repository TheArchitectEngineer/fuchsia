// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::target_handle::TargetHandle;
use addr::{TargetAddr, TargetIpAddr};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use discovery::query::target_addr_info_to_socketaddr;
use emulator_instance::targets as emulator_targets;
use emulator_targets::EmulatorTargetAction;
use ffx_config::EnvironmentContext;
use ffx_daemon_events::TargetConnectionState;
use ffx_daemon_target::target::{
    self, Target, TargetProtocol, TargetTransport, TargetUpdateBuilder,
};
use ffx_daemon_target::target_collection::{TargetCollection, TargetUpdateFilter};
use ffx_stream_util::TryStreamUtilExt;
use ffx_target::{FastbootInterface, TargetInfoQuery};
use fidl::endpoints::ProtocolMarker;
use fidl_fuchsia_developer_ffx::{self as ffx, TargetAddrInfo};
#[cfg(not(target_os = "macos"))]
use fidl_fuchsia_developer_remotecontrol as fidl_rcs;
use fidl_fuchsia_developer_remotecontrol::RemoteControlMarker;
#[cfg(not(target_os = "macos"))]
use fuchsia_async::{DurationExt, TimeoutExt};
#[cfg(test)]
use futures::channel::oneshot::Sender;
use futures::TryStreamExt;
#[cfg(not(target_os = "macos"))]
use futures::{AsyncReadExt, FutureExt, StreamExt};
use protocols::prelude::*;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
#[cfg(not(target_os = "macos"))]
use std::time::Duration;
use tasks::TaskManager;

#[cfg(not(target_os = "macos"))]
use usb_vsock_host::UsbVsockHostEvent;

mod reboot;
mod target_handle;

#[ffx_protocol(ffx::MdnsMarker, ffx::FastbootTargetStreamMarker)]
pub struct TargetCollectionProtocol {
    tasks: TaskManager,

    // An online cache of configured target entries (the non-discoverable targets represented in the
    // ffx configuration).
    // The cache can be updated by calls to AddTarget and RemoveTarget.
    // With manual_targets, we have access to the targets.manual field of the configuration (a
    // vector of strings). Each target is defined by an IP address and a port.
    manual_targets: Rc<dyn manual_targets::ManualTargets>,

    // Only used in tests.
    // If is Some, will send signal after manual targets have been successfully loaded
    #[cfg(test)]
    manual_targets_loaded_signal: Option<Sender<()>>,

    context: EnvironmentContext,
}

impl Default for TargetCollectionProtocol {
    fn default() -> Self {
        #[cfg(not(test))]
        let manual_targets = manual_targets::Config::default();
        #[cfg(test)]
        let manual_targets = manual_targets::Mock::default();

        Self {
            tasks: Default::default(),
            manual_targets: Rc::new(manual_targets),
            #[cfg(test)]
            manual_targets_loaded_signal: None,
            context: ffx_config::global_env_context().unwrap(),
        }
    }
}

async fn target_is_fastboot_tcp(addr: SocketAddr) -> bool {
    log::info!("Checking if target at addr: {addr:?} in fastboot over tcp");
    let tclone = Target::new_with_fastboot_addrs(
        Option::<String>::None,
        Option::<String>::None,
        [addr].iter().map(|x| From::from(*x)).collect(),
        FastbootInterface::Tcp,
    );

    match tclone.is_fastboot_tcp().await {
        Ok(true) => {
            log::info!("Target is running TCP fastboot");
            true
        }
        Ok(false) => {
            log::info!("Target not running TCP fastboot");
            false
        }
        Err(e) => {
            // Since we don't know if this target supports fastboot, this should
            // be an info message, not an error
            log::info!("Got error connecting to target over TCP: {:?}", e);
            false
        }
    }
}

async fn add_manual_target(
    manual_targets: Rc<dyn manual_targets::ManualTargets>,
    tc: &TargetCollection,
    addr: SocketAddr,
    overnet_node: &Arc<overnet_core::Router>,
) -> Rc<Target> {
    log::debug!("Adding manual targets, addr: {addr:?}");

    // When adding a manual target we need to test if the target behind the
    // address is running in fastboot over tcp or not
    let is_fastboot_tcp = target_is_fastboot_tcp(addr).await;

    log::debug!("Is manual target in Fastboot over TCP: {}", is_fastboot_tcp);

    let mut update = TargetUpdateBuilder::new()
        .manual_target()
        .net_addresses(std::slice::from_ref(&addr))
        .discovered(
            match is_fastboot_tcp {
                true => TargetProtocol::Fastboot,
                false => TargetProtocol::Ssh,
            },
            TargetTransport::Network,
        );

    if addr.port() != 0 {
        update = update.ssh_port(Some(addr.port()));
    }

    tc.update_target(
        &[TargetUpdateFilter::NetAddrs(std::slice::from_ref(&addr))],
        update.build(),
        true,
    );

    let _ = manual_targets.add(format!("{}", addr)).await.map_err(|e| {
        log::error!("Unable to persist manual target: {:?}", e);
    });

    let target = tc
        .query_single_enabled_target(&TargetInfoQuery::Addr(addr.into()))
        .expect("Query by address cannot be ambiguous")
        .expect("Could not find inserted manual target");

    if !is_fastboot_tcp {
        log::debug!("Running host pipe since target is not fastboot");
        target.run_host_pipe(overnet_node);
    }
    target
}

async fn remove_manual_target(
    manual_targets: Rc<dyn manual_targets::ManualTargets>,
    tc: &TargetCollection,
    target_id: String,
) -> bool {
    // TODO(dwayneslater): Move into TargetCollection, return false if multiple targets.
    if let Ok(Some(target)) = tc.query_single_enabled_target(&target_id.clone().into()) {
        // TODO(b/299141238): This code won't work if the socket address format in the config does
        // not match the format Rust outputs. Which means a manual target cannot be removed without
        // editing the config.
        let ssh_port = target.ssh_port();
        for addr in target.manual_addrs() {
            let Ok(sockaddr) = TargetIpAddr::try_from(addr) else {
                continue;
            };
            let mut sockaddr: SocketAddr = sockaddr.into();
            ssh_port.map(|p| sockaddr.set_port(p));
            let _ = manual_targets.remove(format!("{}", sockaddr)).await.map_err(|e| {
                log::error!("Unable to persist target removal: {}", e);
            });
            log::debug!("Removed {:#?} from manual target collection", sockaddr)
        }
    }
    tc.remove_target(target_id)
}

impl TargetCollectionProtocol {
    async fn load_manual_targets(
        cx: &Context,
        manual_targets: Rc<dyn manual_targets::ManualTargets>,
    ) -> Result<()> {
        // The FFX config value for a manual target contains a target ID (typically the IP:PORT
        // combo) and a timeout (which is now ignored, but we for backwards compatibility we still
        // allow it).
        for (unparsed_addr, _val) in manual_targets.get_or_default().await {
            let (addr, scope, port) = match netext::parse_address_parts(unparsed_addr.as_str()) {
                Ok(res) => res,
                Err(e) => {
                    log::error!("Skipping load of manual target address due to parsing error '{unparsed_addr}': {e}");
                    continue;
                }
            };
            let scope_id = if let Some(scope) = scope {
                match netext::get_verified_scope_id(scope) {
                    Ok(res) => res,
                    Err(e) => {
                        log::error!("Scope load of manual address '{unparsed_addr}', which had a scope ID of '{scope}', which was not verifiable: {e}");
                        continue;
                    }
                }
            } else {
                0
            };
            let port = port.unwrap_or(0);
            let sa = match addr {
                IpAddr::V4(i) => std::net::SocketAddr::V4(SocketAddrV4::new(i, port)),
                IpAddr::V6(i) => std::net::SocketAddr::V6(SocketAddrV6::new(i, port, 0, scope_id)),
            };

            let tc = cx.get_target_collection().await?;
            let overnet_node = cx.overnet_node()?;
            log::info!("Adding manual target with address: {:?}", sa);
            add_manual_target(manual_targets.clone(), &tc, sa, &overnet_node).await;
        }
        Ok(())
    }

    // Discovery is turned off, so  we're not going to discover
    // anything.  But since the query provided us with an address/
    // serial#, we can try to connect to it.
    async fn add_and_use_target(
        &self,
        target_collection: &Rc<TargetCollection>,
        node: &Arc<overnet_core::Router>,
        query: TargetInfoQuery,
    ) -> Result<Rc<Target>, ffx::OpenTargetError> {
        let addrs;
        let serial: String;
        let (update, filter) = match query {
            TargetInfoQuery::Addr(addr) => {
                addrs = [addr];
                // Set the addresses, note that is transient, note that it is
                // a network target, and enable it
                let mut update = TargetUpdateBuilder::new()
                    .net_addresses(&addrs)
                    .transient_target()
                    .discovered(TargetProtocol::Ssh, TargetTransport::Network)
                    // Call this "manual" so we don't expire the address if we don't see an mDNS update.
                    .manual_target()
                    .enable();
                let port = addr.port();
                if port != 0 {
                    update = update.ssh_port(Some(port));
                }

                let filter = [TargetUpdateFilter::NetAddrs(&addrs)];
                (update, filter)
            }
            TargetInfoQuery::Serial(ref sn) => {
                // We're not going to discover anything.  But since
                // the query provided us with a serial number, we can try
                // to connect to it.
                serial = sn.clone();
                let update = TargetUpdateBuilder::new()
                    .identity(target::Identity::from_serial(sn))
                    // This is only explicit when the client has discovered this device
                    .discovered(TargetProtocol::Fastboot, TargetTransport::Usb)
                    .transient_target()
                    .enable();
                let filter = [TargetUpdateFilter::Serial(&serial)];
                (update, filter)
            }
            _ => unreachable!(),
        };
        target_collection.update_target(&filter, update.build(), true);
        target_collection.try_to_reconnect_target(&filter, &node);
        // Creating and connecting to the target doesn't actually _give_ us
        // the target, so we now need to search for the one we just created
        target_collection
            .query_single_enabled_target(&query)
            .map_err(|_| ffx::OpenTargetError::QueryAmbiguous)
            .and_then(|target| match target {
                None => {
                    log::error!("Couldn't find target we just created");
                    Err(ffx::OpenTargetError::TargetNotFound)
                }
                Some(t) => Ok(t),
            })
    }
}

#[async_trait(?Send)]
impl FidlProtocol for TargetCollectionProtocol {
    type Protocol = ffx::TargetCollectionMarker;
    type StreamHandler = FidlStreamHandler<Self>;

    async fn handle(&self, cx: &Context, req: ffx::TargetCollectionRequest) -> Result<()> {
        log::debug!("handling request {req:?}");
        let target_collection = cx.get_target_collection().await?;
        match req {
            ffx::TargetCollectionRequest::ListTargets { reader, query, .. } => {
                let reader = reader.into_proxy();
                let query = match query.string_matcher.clone() {
                    Some(query) if !query.is_empty() => Some(TargetInfoQuery::from(query)),
                    _ => None,
                };

                // TODO(b/297896647): Use `discover_targets` to run discovery & stream discovered
                // targets. Wait for `reader.as_channel().on_closed()` to cancel discovery when no
                // longer reading. Add FIDL parameter to control discovery streaming.

                let targets = target_collection.targets(query.as_ref());

                // This was chosen arbitrarily. It's possible to determine a
                // better chunk size using some FIDL constant math.
                const TARGET_CHUNK_SIZE: usize = 20;
                let mut iter = targets.chunks(TARGET_CHUNK_SIZE);
                loop {
                    let chunk = iter.next().unwrap_or(&[]);
                    reader.next(chunk).await?;
                    if chunk.is_empty() {
                        break;
                    }
                }
                Ok(())
            }
            ffx::TargetCollectionRequest::OpenTarget { query, responder, target_handle } => {
                log::trace!("Open Target {query:?}");

                let query = TargetInfoQuery::from(query.string_matcher.clone());
                log::debug!("Open Target parsed query: {query:?}");

                let node = cx.overnet_node()?;
                // Get a previously used target first, otherwise fall back to discovery + use.
                let result = match target_collection.query_single_connected_target(&query) {
                    Ok(Some(target)) => Ok(target),
                    Ok(None) => {
                        match query {
                            // If we have enough information in the request to
                            // try to connect to the target, just add the target
                            // automatically.
                            TargetInfoQuery::Addr(_) | TargetInfoQuery::Serial(_) => {
                                self.add_and_use_target(&target_collection, &node, query).await
                            }
                            _ => {
                                // If we don't have enough information, but
                                // discovery is enabled, then try to discover
                                // the target based on the query. Otherwise,
                                // fail with TargetNotFound.
                                let can_discover =
                                    ffx_target::is_discovery_enabled(&self.context).await;
                                if can_discover {
                                    target_collection
                                        // OpenTarget is called on behalf of
                                        // the user.
                                        .discover_target(&query)
                                        .await
                                        .map_err(|_| ffx::OpenTargetError::QueryAmbiguous)
                                        .map(|t| {
                                            target_collection.use_target(t, "OpenTarget request")
                                        })
                                } else {
                                    log::warn!("OpenTarget(query:?): daemon discovery is turned off, so client should only be sending already-resolved addresses (Addr or Serial)");
                                    Err(ffx::OpenTargetError::TargetNotFound)
                                }
                            }
                        }
                    }
                    Err(()) => Err(ffx::OpenTargetError::QueryAmbiguous),
                };

                let target = match result {
                    Ok(target) => target,
                    Err(e) => {
                        log::debug!("OpenTarget: got err {e:?}");
                        return responder.send(Err(e)).map_err(Into::into);
                    }
                };

                log::trace!("Found target: {target:?}");
                self.tasks.spawn(TargetHandle::new(
                    target,
                    cx.clone(),
                    target_handle,
                    target_collection.clone(),
                )?);
                responder.send(Ok(())).map_err(Into::into)
            }
            ffx::TargetCollectionRequest::AddTarget {
                ip, config, add_target_responder, ..
            } => {
                let add_target_responder = add_target_responder.into_proxy();
                let ip = TargetAddr::from(ip);
                let Ok(ip) = ip.try_into() else {
                    return add_target_responder
                        .error(&ffx::AddTargetError {
                            connection_error: None,
                            connection_error_logs: Some(vec!["Wrong address type!".to_owned()]),
                            ..Default::default()
                        })
                        .map_err(Into::into);
                };
                let addr = target_addr_info_to_socketaddr(ip);
                let node = cx.overnet_node()?;
                let do_add_target = || {
                    add_manual_target(self.manual_targets.clone(), &target_collection, addr, &node)
                };
                match config.verify_connection {
                    Some(true) => {}
                    _ => {
                        let _ = do_add_target().await;
                        return add_target_responder.success().map_err(Into::into);
                    }
                };
                // The drop guard is here for the impatient user: if the user closes their channel
                // prematurely (before this operation either succeeds or fails), then they will
                // risk adding a manual target that can never be connected to, and then have to
                // manually remove the target themselves.
                struct DropGuard(
                    Option<(
                        Rc<dyn manual_targets::ManualTargets>,
                        Rc<TargetCollection>,
                        SocketAddr,
                    )>,
                );
                impl Drop for DropGuard {
                    fn drop(&mut self) {
                        match self.0.take() {
                            Some((mt, tc, addr)) => fuchsia_async::Task::local(async move {
                                remove_manual_target(mt, &tc, addr.to_string()).await;
                            })
                            .detach(),
                            None => {}
                        }
                    }
                }
                let mut drop_guard = DropGuard(Some((
                    self.manual_targets.clone(),
                    target_collection.clone(),
                    addr.clone(),
                )));
                let target = do_add_target().await;
                // If the target is in fastboot then skip rcs
                match target.get_connection_state() {
                    TargetConnectionState::Fastboot(_) => {
                        log::info!("skipping rcs verfication as the target is in fastboot ");
                        let _ = drop_guard.0.take();
                        return add_target_responder.success().map_err(Into::into);
                    }
                    _ => {
                        log::error!(
                            "target connection state was: {:?}",
                            target.get_connection_state()
                        );
                    }
                };
                let rcs = target_handle::wait_for_rcs(&target, &cx).await?;
                match rcs {
                    Ok(mut rcs) => {
                        let (rcs_proxy, server) =
                            fidl::endpoints::create_proxy::<RemoteControlMarker>();
                        rcs.copy_to_channel(server.into_channel())?;
                        match rcs::knock_rcs(&rcs_proxy).await {
                            Ok(_) => {
                                let _ = drop_guard.0.take();
                            }
                            Err(e) => {
                                return add_target_responder
                                    .error(&ffx::AddTargetError {
                                        connection_error: Some(e),
                                        connection_error_logs: match target
                                            .host_pipe_log_buffer()
                                            .lines()
                                        {
                                            vec if vec.is_empty() => None,
                                            v => Some(v),
                                        },
                                        ..Default::default()
                                    })
                                    .map_err(Into::into)
                            }
                        }
                    }
                    Err(e) => {
                        let logs = target.host_pipe_log_buffer().lines();
                        let _ = remove_manual_target(
                            self.manual_targets.clone(),
                            &target_collection,
                            addr.to_string(),
                        )
                        .await;
                        let _ = drop_guard.0.take();
                        return add_target_responder
                            .error(&ffx::AddTargetError {
                                connection_error: Some(e),
                                connection_error_logs: Some(logs),
                                ..Default::default()
                            })
                            .map_err(Into::into);
                    }
                }
                add_target_responder.success().map_err(Into::into)
            }
            ffx::TargetCollectionRequest::RemoveTarget { target_id, responder } => {
                let result = remove_manual_target(
                    self.manual_targets.clone(),
                    &target_collection,
                    target_id,
                )
                .await;
                responder.send(result).map_err(Into::into)
            }
        }
    }

    async fn serve<'a>(
        &'a self,
        cx: &'a Context,
        stream: <Self::Protocol as ProtocolMarker>::RequestStream,
    ) -> Result<()> {
        // Necessary to avoid hanging forever when a client drops a connection
        // during a call to OpenTarget.
        stream
            .map_err(|err| anyhow!("{}", err))
            .try_for_each_concurrent_while_connected(None, |req| self.handle(cx, req))
            .await
    }

    async fn stop(&mut self, _cx: &Context) -> Result<()> {
        drop(self.tasks.drain());
        Ok(())
    }

    async fn start(&mut self, cx: &Context) -> Result<()> {
        let node = cx.overnet_node()?;
        let load_manual_cx = cx.clone();
        let manual_targets_collection = self.manual_targets.clone();
        #[cfg(test)]
        let signal = if self.manual_targets_loaded_signal.is_some() {
            Some(self.manual_targets_loaded_signal.take().unwrap())
        } else {
            None
        };
        self.tasks.spawn(async move {
            log::debug!("Loading previously configured manual targets");
            if let Err(e) = TargetCollectionProtocol::load_manual_targets(
                &load_manual_cx,
                manual_targets_collection,
            )
            .await
            {
                log::warn!("Got error loading manual targets: {}", e);
            }
            #[cfg(test)]
            if let Some(s) = signal {
                log::debug!("Sending signal that manual target loading is complete");
                let _ = s.send(());
            }
        });
        let mdns = self.open_mdns_proxy(cx).await?;
        let fastboot = self.open_fastboot_target_stream_proxy(cx).await?;
        let tc = cx.get_target_collection().await?;
        let tc_clone = tc.clone();
        let node_clone = Arc::clone(&node);
        self.tasks.spawn(async move {
            while let Ok(Some(e)) = mdns.get_next_event().await {
                match *e {
                    ffx::MdnsEventType::TargetFound(t)
                    | ffx::MdnsEventType::TargetRediscovered(t) => {
                        // For backwards compatibility.
                        // Immediately mark the target as used then run the host pipe.
                        let autoconnect = if let Some(ctx) = ffx_config::global_env_context() {
                            !ffx_config::is_mdns_autoconnect_disabled(&ctx)
                        } else {
                            true
                        };
                        handle_discovered_target(&tc_clone, t, &node_clone, autoconnect);
                    }
                    _ => {}
                }
            }
        });
        self.tasks.spawn(async move {
            while let Ok(target) = fastboot.get_next().await {
                handle_fastboot_target(&tc, target);
            }
        });

        let tc2 = cx.get_target_collection().await?;
        let context = self.context.clone();
        let node_clone = Arc::clone(&node);
        self.tasks.spawn(async move {
            let instance_root: PathBuf = match context.get(emulator_instance::EMU_INSTANCE_ROOT_DIR)
            {
                Ok(dir) => dir,
                Err(e) => {
                    log::error!("Could not read emulator instance root configuration: {e:?}");
                    return;
                }
            };

            let mut watcher = match emulator_targets::start_emulator_watching(instance_root) {
                Ok(w) => w,
                Err(e) => {
                    log::error!("Could not create emulator watcher: {e:?}");
                    return;
                }
            };

            let _ = watcher
                .check_all_instances()
                .await
                .map_err(|e| log::error!("Error checking emulator instances: {e:?}"));
            log::trace!("Starting processing emulator instance events");
            loop {
                if let Some(emu_target_action) = watcher.emulator_target_detected().await {
                    match emu_target_action {
                        EmulatorTargetAction::Add(emu_target) => {
                            // Let's always connect to emulators -- otherwise, why would someone start an emulator?
                            handle_discovered_target(&tc2, emu_target, &node_clone, true);
                        }
                        EmulatorTargetAction::Remove(emu_target) => {
                            if let Some(id) = emu_target.nodename {
                                if tc2.remove_target(id.clone()) {
                                    log::info!(
                                        "Successfully removed emulator instance ['{}']",
                                        &id
                                    );
                                } else {
                                    log::error!("Unable to remove emulator instance ['{}']", &id);
                                };
                            }
                        }
                    };
                }
            }
        });

        #[cfg(not(target_os = "macos"))]
        if let Some(mut usb_events) = Target::init_usb_vsock_host() {
            let tc = cx.get_target_collection().await?;
            self.tasks.spawn(async move {
                while let Some(event) = usb_events.next().await {
                    match event {
                        UsbVsockHostEvent::AddedCid(cid) => {
                            if let Err(error) = handle_usb_target(cid, &tc, &node).await {
                                log::warn!(cid, error:?; "Could not connect to USB target");
                            }
                        }
                        UsbVsockHostEvent::RemovedCid(cid) => {
                            tc.remove_address(TargetAddr::UsbCtx(cid));
                        }
                    }
                }

                log::error!("USB Discovery shut down");
            });
        }

        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
const VSOCK_IDENTIFY_PORT: u32 = 201;

#[cfg(not(target_os = "macos"))]
async fn handle_usb_target(
    cid: u32,
    tc: &Rc<TargetCollection>,
    overnet_node: &Arc<overnet_core::Router>,
) -> Result<()> {
    let host = Target::get_usb_vsock_host().ok_or_else(|| anyhow!("USB not initialized"))?;

    handle_usb_target_impl(cid, tc, overnet_node, host).await
}

#[cfg(not(target_os = "macos"))]
async fn handle_usb_target_impl(
    cid: u32,
    tc: &Rc<TargetCollection>,
    overnet_node: &Arc<overnet_core::Router>,
    host: Arc<usb_vsock_host::UsbVsockHost<fuchsia_async::Socket>>,
) -> Result<()> {
    let (socket, other_end) = fuchsia_async::emulated_handle::Socket::create_stream();
    let other_end = fuchsia_async::Socket::from_socket(other_end);
    let _state = host
        .connect(
            cid.try_into().map_err(|_| anyhow!("Tried to get target info from USB CID 0"))?,
            VSOCK_IDENTIFY_PORT,
            other_end,
        )
        .await?;

    let mut buf = Vec::new();
    let mut socket = fuchsia_async::Socket::from_socket(socket);
    let _buf_len = socket
        .read_to_end(&mut buf)
        .map(|x| x.map_err(anyhow::Error::from))
        .on_timeout(Duration::from_secs(5).after_now(), || {
            Err(anyhow!("Timed out waiting for USB target info"))
        })
        .await?;

    let (header, bytes) = fidl::encoding::decode_transaction_header(&buf)?;
    let body = fidl_message::decode_response_flexible_result::<
        fidl_rcs::IdentifyHostResponse,
        fidl_rcs::IdentifyHostError,
    >(header, bytes)?;

    let identify = match body {
        fidl_message::MaybeUnknown::Known(x) => {
            x.map_err(|e| anyhow!("Could not identify USB host: {e:?}"))?
        }
        fidl_message::MaybeUnknown::Unknown => {
            return Err(anyhow!("Got unknown identify host response"))
        }
    };

    let (update, addrs) = TargetUpdateBuilder::from_rcs_identify_no_connection(&identify);
    let update =
        update.usb_cid(cid).enable().discovered(TargetProtocol::Vsock, TargetTransport::Usb);
    let filter = if let Some(ref name) = identify.nodename {
        &[TargetUpdateFilter::NetAddrs(&addrs), TargetUpdateFilter::LegacyNodeName(name)][..]
    } else {
        &[TargetUpdateFilter::NetAddrs(&addrs)][..]
    };

    tc.update_target(filter, update.build(), true);

    tc.try_to_reconnect_target(filter, overnet_node);

    Ok(())
}

// USB fastboot
fn handle_fastboot_target(tc: &Rc<TargetCollection>, target: ffx::FastbootTarget) {
    if let Some(serial) = target.serial {
        log::debug!("Found new target via fastboot: {}", serial);

        let update = TargetUpdateBuilder::new()
            .discovered(TargetProtocol::Fastboot, TargetTransport::Usb)
            .identity(target::Identity::from_serial(serial.clone()))
            .build();
        tc.update_target(&[TargetUpdateFilter::Serial(&serial)], update, true);
    } else if let Some(addrs) = target.addresses {
        log::debug!("Found a new fastboot over network target {:?}.", addrs);

        let mut nadders = vec![];
        for addr in addrs {
            nadders.push(SocketAddr::from(TargetIpAddr::from(addr)));
        }
        let update = TargetUpdateBuilder::new()
            .discovered(TargetProtocol::Fastboot, TargetTransport::Network)
            .net_addresses(&nadders)
            .build();
        tc.update_target(&[TargetUpdateFilter::NetAddrs(&nadders)], update, true);
    } else {
        log::warn!("Got a fastboot target without serial or addresses: {:?}", target);
    }
}

// mDNS/Emulator Fastboot & RCS
fn handle_discovered_target(
    tc: &Rc<TargetCollection>,
    t: ffx::TargetInfo,
    overnet_node: &Arc<overnet_core::Router>,
    autoconnect: bool,
) {
    log::debug!("Discovered target {t:?}");

    if t.fastboot_interface.is_some() {
        log::debug!(
            "Found new fastboot target via mdns: {}. Address: {:?}",
            t.nodename.as_deref().unwrap_or(ffx_target::UNKNOWN_TARGET_NAME),
            t.addresses
        );
    } else {
        log::debug!(
            "Found new target via mdns or file watcher: {}",
            t.nodename.as_deref().unwrap_or(ffx_target::UNKNOWN_TARGET_NAME),
        );
    }

    let identity = t.nodename.as_deref().map(target::Identity::from_name);

    let addrs = t
        .addresses
        .iter()
        .flatten()
        .filter_map(|a| TargetIpAddr::try_from(a).ok().map(Into::into))
        .collect::<Vec<_>>();

    let vsock_addr = t.addresses.iter().flatten().find_map(|x| {
        if let TargetAddrInfo::Vsock(x) = x {
            Some(*x)
        } else {
            None
        }
    });

    let mut update = TargetUpdateBuilder::new().net_addresses(&addrs);

    if let Some(vsock_addr) = vsock_addr {
        update = update.vsock_cid(vsock_addr.cid);
    }

    if autoconnect {
        update = update.enable();
    }

    if let Some(identity) = identity {
        update = update.identity(identity);
    }

    update = match t.fastboot_interface {
        Some(interface) => update.discovered(
            TargetProtocol::Fastboot,
            match interface {
                ffx::FastbootInterface::Tcp => TargetTransport::Network,
                ffx::FastbootInterface::Udp => TargetTransport::NetworkUdp,
                _ => panic!("Discovered non-network fastboot interface over mDNS, {interface:?}"),
            },
        ),
        None => {
            if vsock_addr.is_some() {
                update.discovered(TargetProtocol::Vsock, TargetTransport::Network)
            } else {
                update.discovered(TargetProtocol::Ssh, TargetTransport::Network)
            }
        }
    };

    if let Some(ffx::TargetIpAddrInfo::IpPort(ssh_address)) = t.ssh_address {
        update = update.ssh_port(Some(ssh_address.port));
    }

    let filter = if let Some(ref name) = t.nodename {
        &[TargetUpdateFilter::NetAddrs(&addrs), TargetUpdateFilter::LegacyNodeName(name)][..]
    } else {
        &[TargetUpdateFilter::NetAddrs(&addrs)][..]
    };

    tc.update_target(filter, update.build(), true);

    tc.try_to_reconnect_target(filter, overnet_node);
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use async_channel::{Receiver, Sender};
    use ffx_config::{query, ConfigLevel};
    use fidl_fuchsia_developer_ffx::TargetQuery;
    use fidl_fuchsia_net::{IpAddress, Ipv6Address};
    use futures::channel::oneshot::channel;
    use futures::AsyncWriteExt;
    use protocols::testing::FakeDaemonBuilder;
    use serde_json::{json, Map, Value};
    use std::cell::RefCell;
    use std::path::Path;
    use std::str::FromStr;
    use std::time::Instant;
    use tempfile::tempdir;
    use timeout::timeout;

    #[fuchsia::test]
    async fn test_handle_mdns_non_fastboot() {
        let local_node = overnet_core::Router::new(None).unwrap();
        let t = Target::new_named("this-is-a-thing");
        let tc = Rc::new(TargetCollection::new());
        tc.merge_insert(t.clone());
        let before_update = Instant::now();

        handle_discovered_target(
            &tc,
            ffx::TargetInfo { nodename: Some(t.nodename().unwrap()), ..Default::default() },
            &local_node,
            false,
        );
        assert!(!t.is_host_pipe_running());
        assert_matches!(t.get_connection_state(), TargetConnectionState::Mdns(t) if t > before_update);
    }

    #[fuchsia::test]
    async fn test_handle_mdns_fastboot() {
        let local_node = overnet_core::Router::new(None).unwrap();
        let t = Target::new_named("this-is-a-thing");
        let tc = Rc::new(TargetCollection::new());
        tc.merge_insert(t.clone());
        let before_update = Instant::now();

        handle_discovered_target(
            &tc,
            ffx::TargetInfo {
                nodename: Some(t.nodename().unwrap()),
                target_state: Some(ffx::TargetState::Fastboot),
                fastboot_interface: Some(ffx::FastbootInterface::Tcp),
                ..Default::default()
            },
            &local_node,
            false,
        );
        assert!(!t.is_host_pipe_running());
        assert_matches!(t.get_connection_state(), TargetConnectionState::Fastboot(t) if t > before_update);
    }

    struct TestMdns {
        /// Lets the test know that a call to `GetNextEvent` has started. This
        /// is just a hack to avoid using timers for races. This is dependent
        /// on the executor running in a single thread.
        call_started: Sender<()>,
        next_event: Receiver<ffx::MdnsEventType>,
    }

    impl Default for TestMdns {
        fn default() -> Self {
            unimplemented!()
        }
    }

    #[async_trait(?Send)]
    impl FidlProtocol for TestMdns {
        type Protocol = ffx::MdnsMarker;
        type StreamHandler = FidlStreamHandler<Self>;

        async fn handle(&self, _cx: &Context, req: ffx::MdnsRequest) -> Result<()> {
            match req {
                ffx::MdnsRequest::GetNextEvent { responder } => {
                    self.call_started.send(()).await.unwrap();
                    responder.send(self.next_event.recv().await.ok().as_ref()).map_err(Into::into)
                }
                _ => panic!("unsupported"),
            }
        }
    }

    async fn list_targets(
        query: Option<&str>,
        tc: &ffx::TargetCollectionProxy,
    ) -> Vec<ffx::TargetInfo> {
        let (reader, server) =
            fidl::endpoints::create_endpoints::<ffx::TargetCollectionReaderMarker>();
        tc.list_targets(
            &ffx::TargetQuery { string_matcher: query.map(|s| s.to_owned()), ..Default::default() },
            reader,
        )
        .unwrap();
        let mut res = Vec::new();
        let mut stream = server.into_stream();
        while let Ok(Some(ffx::TargetCollectionReaderRequest::Next { entry, responder })) =
            stream.try_next().await
        {
            responder.send().unwrap();
            if entry.len() > 0 {
                res.extend(entry);
            } else {
                break;
            }
        }
        res
    }

    #[derive(Default)]
    struct FakeFastboot {}

    #[async_trait(?Send)]
    impl FidlProtocol for FakeFastboot {
        type Protocol = ffx::FastbootTargetStreamMarker;
        type StreamHandler = FidlStreamHandler<Self>;

        async fn handle(
            &self,
            _cx: &Context,
            _req: ffx::FastbootTargetStreamRequest,
        ) -> Result<()> {
            futures::future::pending::<()>().await;
            Ok(())
        }
    }

    async fn init_test_config(_env: &ffx_config::TestEnv, temp_dir: &Path) {
        query(emulator_instance::EMU_INSTANCE_ROOT_DIR)
            .level(Some(ConfigLevel::User))
            .set(json!(temp_dir.display().to_string()))
            .await
            .unwrap();
    }

    #[fuchsia::test]
    async fn test_protocol_integration() {
        let env = ffx_config::test_init().await.unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        init_test_config(&env, temp.path()).await;

        // Disable mDNS autoconnect to prevent RCS connection attempts in this test.
        env.context
            .query("discovery.mdns.autoconnect")
            .level(Some(ConfigLevel::User))
            .set(json!(false))
            .await
            .unwrap();

        const NAME: &'static str = "foo";
        const NAME2: &'static str = "bar";
        const NAME3: &'static str = "baz";
        const NON_MATCHING_NAME: &'static str = "mlorp";
        let (call_started_sender, call_started_receiver) = async_channel::unbounded::<()>();
        let (target_sender, r) = async_channel::unbounded::<ffx::MdnsEventType>();
        let mdns_protocol =
            Rc::new(RefCell::new(TestMdns { call_started: call_started_sender, next_event: r }));
        let fake_daemon = FakeDaemonBuilder::new()
            .inject_fidl_protocol(mdns_protocol)
            .register_fidl_protocol::<FakeFastboot>()
            .register_fidl_protocol::<TargetCollectionProtocol>()
            .build();
        let tc = fake_daemon.open_proxy::<ffx::TargetCollectionMarker>().await;
        let res = list_targets(None, &tc).await;
        assert_eq!(res.len(), 0);
        call_started_receiver.recv().await.unwrap();
        target_sender
            .send(ffx::MdnsEventType::TargetFound(ffx::TargetInfo {
                nodename: Some(NAME.to_owned()),
                ..Default::default()
            }))
            .await
            .unwrap();
        target_sender
            .send(ffx::MdnsEventType::TargetFound(ffx::TargetInfo {
                nodename: Some(NAME2.to_owned()),
                ..Default::default()
            }))
            .await
            .unwrap();
        target_sender
            .send(ffx::MdnsEventType::TargetFound(ffx::TargetInfo {
                nodename: Some(NAME3.to_owned()),
                ..Default::default()
            }))
            .await
            .unwrap();
        call_started_receiver.recv().await.unwrap();
        let res = list_targets(None, &tc).await;
        assert_eq!(res.len(), 3, "received: {:?}", res);
        assert!(res.iter().any(|t| t.nodename.as_ref().unwrap() == NAME));
        assert!(res.iter().any(|t| t.nodename.as_ref().unwrap() == NAME2));
        assert!(res.iter().any(|t| t.nodename.as_ref().unwrap() == NAME3));

        let res = list_targets(Some(NON_MATCHING_NAME), &tc).await;
        assert_eq!(res.len(), 0, "received: {:?}", res);

        let res = list_targets(Some(NAME), &tc).await;
        assert_eq!(res.len(), 1, "received: {:?}", res);
        assert_eq!(res[0].nodename.as_ref().unwrap(), NAME);

        let res = list_targets(Some(NAME2), &tc).await;
        assert_eq!(res.len(), 1, "received: {:?}", res);
        assert_eq!(res[0].nodename.as_ref().unwrap(), NAME2);

        let res = list_targets(Some(NAME3), &tc).await;
        assert_eq!(res.len(), 1, "received: {:?}", res);
        assert_eq!(res[0].nodename.as_ref().unwrap(), NAME3);

        // Regression test for b/308490757:
        // Targets with a long compatibility message fail to send across FIDL boundary.
        let compatibility = ffx::CompatibilityInfo {
            state: ffx::CompatibilityState::Unsupported,
            platform_abi: 1234,
            message: r"Somehow, some way, this target is incompatible.
                To convey this information, this exceptionally long message contains information,
                some of which is unrelated to the problem.

                Did you know: They did surgery on a grape."
                .into(),
        };

        {
            let tc = fake_daemon.get_target_collection().await.unwrap();

            tc.update_target(
                &[TargetUpdateFilter::LegacyNodeName(NAME)],
                TargetUpdateBuilder::new()
                    .rcs_compatibility(Some(compatibility.clone().into()))
                    .build(),
                false,
            );
        }

        match &*list_targets(Some(NAME), &tc).await {
            [target] if target.nodename.as_deref() == Some(NAME) => {
                assert_eq!(target.compatibility, Some(compatibility));
            }
            list => panic!("Expected single target '{NAME}', got {list:?}"),
        }
    }

    #[fuchsia::test]
    async fn test_handle_fastboot_target_no_serial() {
        let tc = Rc::new(TargetCollection::new());
        handle_fastboot_target(&tc, ffx::FastbootTarget::default());
        assert_eq!(tc.targets(None).len(), 0, "target collection should remain empty");
    }

    #[fuchsia::test]
    async fn test_handle_fastboot_target() {
        let tc = Rc::new(TargetCollection::new());
        handle_fastboot_target(
            &tc,
            ffx::FastbootTarget { serial: Some("12345".to_string()), ..Default::default() },
        );
        assert_eq!(tc.targets(None)[0].serial_number.as_deref(), Some("12345"));
    }

    #[cfg(not(target_os = "macos"))]
    #[fuchsia::test]
    async fn test_handle_usb_target() {
        use usb_vsock_host as usbv;
        const NODE_NAME: &str = "Teletechternacon";
        let tc = Rc::new(TargetCollection::new());
        let tc_clone = Rc::clone(&tc);
        let local_node = overnet_core::Router::new(None).unwrap();

        let usbv::TestConnection {
            cid,
            host,
            connection,
            mut incoming_requests,
            abort_transfer: _,
            event_receiver: _,
            scope,
        } = usbv::TestConnection::new();

        let handled = scope.compute_local(async move {
            handle_usb_target_impl(cid, &tc_clone, &local_node, host).await
        });

        let identify_request = incoming_requests.next().await.unwrap();

        let usb_vsock::Address { device_cid, host_cid, device_port, host_port: _ } =
            identify_request.address();
        assert_eq!(cid, *device_cid);
        assert_eq!(2, *host_cid);
        assert_eq!(VSOCK_IDENTIFY_PORT, *device_port);

        let (identify_sock, other_end) = fuchsia_async::emulated_handle::Socket::create_stream();
        let other_end = fuchsia_async::Socket::from_socket(other_end);
        let _conn_state = connection.accept(identify_request, other_end).await.unwrap();

        let mut identify_sock = fuchsia_async::Socket::from_socket(identify_sock);
        let header = fidl::encoding::TransactionHeader::new(
            0,
            0x6035e1ab368deee1,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let identify = fidl_message::encode_response_result::<
            fidl_rcs::IdentifyHostResponse,
            fidl_rcs::IdentifyHostError,
        >(
            header,
            Ok(fidl_rcs::IdentifyHostResponse {
                nodename: Some(NODE_NAME.into()),
                ..Default::default()
            }),
        )
        .unwrap();
        identify_sock.write_all(&identify).await.unwrap();
        std::mem::drop(identify_sock);

        handled.await.unwrap();

        assert!(tc
            .targets(None)
            .into_iter()
            .any(|target| { target.nodename == Some(NODE_NAME.into()) }));
    }

    fn make_target_add_fut(
        server: fidl::endpoints::ServerEnd<ffx::AddTargetResponder_Marker>,
    ) -> impl std::future::Future<Output = Result<(), ffx::AddTargetError>> {
        async {
            let mut stream = server.into_stream();
            if let Ok(Some(req)) = stream.try_next().await {
                match req {
                    ffx::AddTargetResponder_Request::Success { .. } => {
                        return Ok(());
                    }
                    ffx::AddTargetResponder_Request::Error { err, .. } => {
                        return Err(err);
                    }
                }
            } else {
                panic!("connection lost to stream. This should not be reachable");
            }
        }
    }

    #[derive(Default)]
    struct FakeMdns {}

    #[async_trait(?Send)]
    impl FidlProtocol for FakeMdns {
        type Protocol = ffx::MdnsMarker;
        type StreamHandler = FidlStreamHandler<Self>;

        async fn handle(&self, _cx: &Context, _req: ffx::MdnsRequest) -> Result<()> {
            futures::future::pending::<()>().await;
            Ok(())
        }
    }

    #[fuchsia::test]
    async fn test_persisted_manual_target_remove() {
        let env = ffx_config::test_init().await.unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        init_test_config(&env, temp.path()).await;

        let (manual_targets_loaded_sender, manual_targets_loaded_receiver) = channel::<()>();
        let tc_impl = Rc::new(RefCell::new(TargetCollectionProtocol::default()));
        tc_impl.borrow_mut().manual_targets_loaded_signal.replace(manual_targets_loaded_sender);
        let fake_daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeMdns>()
            .register_fidl_protocol::<FakeFastboot>()
            .inject_fidl_protocol(tc_impl.clone())
            .build();
        tc_impl.borrow().manual_targets.add("127.0.0.1:8022".to_string()).await.unwrap();

        let proxy = fake_daemon.open_proxy::<ffx::TargetCollectionMarker>().await;
        let res = list_targets(None, &proxy).await;
        // List targets will be unstable as the manual targets have not yet loaded
        // we can be sure, however that there should be less than 2 at this
        // point as we have only added one manual target thus far
        assert!(res.len() < 2);
        // Wait here... listing targets initializes the proxy which calls `start` on the target collection
        // need to wait for it to load the manual targets
        manual_targets_loaded_receiver.await.unwrap();
        let res = list_targets(None, &proxy).await;
        assert_eq!(1, res.len());
        assert!(proxy.remove_target("127.0.0.1:8022").await.unwrap());
        assert_eq!(0, list_targets(None, &proxy).await.len());
        assert_eq!(
            tc_impl.borrow().manual_targets.get_or_default().await,
            Map::<String, Value>::new()
        );
    }

    #[fuchsia::test]
    async fn test_add_target() {
        let env = ffx_config::test_init().await.unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        init_test_config(&env, temp.path()).await;

        let fake_daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeMdns>()
            .register_fidl_protocol::<FakeFastboot>()
            .register_fidl_protocol::<TargetCollectionProtocol>()
            .build();
        let target_addr = TargetIpAddr::from_str("[::1]:0").unwrap();
        let proxy = fake_daemon.open_proxy::<ffx::TargetCollectionMarker>().await;
        let (client, server) =
            fidl::endpoints::create_endpoints::<ffx::AddTargetResponder_Marker>();
        let target_add_fut = make_target_add_fut(server);
        proxy.add_target(&target_addr.into(), &ffx::AddTargetConfig::default(), client).unwrap();
        target_add_fut.await.unwrap();
        let target_collection = Context::new(fake_daemon).get_target_collection().await.unwrap();
        let target = target_collection
            .query_single_enabled_target(&TargetInfoQuery::Addr(target_addr.into()))
            .unwrap()
            .expect("Target not found");
        assert_eq!(target.addrs().len(), 1);
        assert_eq!(target.addrs().into_iter().next(), Some(target_addr.into()));
    }

    #[fuchsia::test]
    async fn test_add_target_with_port() {
        let env = ffx_config::test_init().await.unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        init_test_config(&env, temp.path()).await;

        let fake_daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeMdns>()
            .register_fidl_protocol::<FakeFastboot>()
            .register_fidl_protocol::<TargetCollectionProtocol>()
            .build();
        let target_addr = TargetIpAddr::from_str("[::1]:8022").unwrap();
        let proxy = fake_daemon.open_proxy::<ffx::TargetCollectionMarker>().await;
        let (client, server) =
            fidl::endpoints::create_endpoints::<ffx::AddTargetResponder_Marker>();
        let target_add_fut = make_target_add_fut(server);
        proxy.add_target(&target_addr.into(), &ffx::AddTargetConfig::default(), client).unwrap();
        target_add_fut.await.unwrap();
        let target_collection = Context::new(fake_daemon).get_target_collection().await.unwrap();
        let target = target_collection
            .query_single_enabled_target(&TargetInfoQuery::Addr(target_addr.into()))
            .unwrap()
            .expect("Target not found");
        assert_eq!(target.addrs().len(), 1);
        assert_eq!(target.addrs().into_iter().next(), Some(target_addr.into()));
    }

    #[fuchsia::test]
    async fn test_persisted_manual_target_add() {
        let env = ffx_config::test_init().await.unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        init_test_config(&env, temp.path()).await;

        let tc_impl = Rc::new(RefCell::new(TargetCollectionProtocol::default()));
        let fake_daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeMdns>()
            .register_fidl_protocol::<FakeFastboot>()
            .inject_fidl_protocol(tc_impl.clone())
            .build();
        let (client, server) =
            fidl::endpoints::create_endpoints::<ffx::AddTargetResponder_Marker>();
        let target_add_fut = make_target_add_fut(server);
        let proxy = fake_daemon.open_proxy::<ffx::TargetCollectionMarker>().await;
        proxy
            .add_target(
                &ffx::TargetAddrInfo::IpPort(ffx::TargetIpPort {
                    ip: IpAddress::Ipv6(Ipv6Address {
                        addr: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                    }),
                    port: 8022,
                    scope_id: 1,
                }),
                &ffx::AddTargetConfig::default(),
                client,
            )
            .unwrap();
        target_add_fut.await.unwrap();
        let target_collection = Context::new(fake_daemon).get_target_collection().await.unwrap();
        assert_eq!(1, target_collection.targets(None).len());
        let mut map = Map::<String, Value>::new();
        map.insert("[fe80::1%1]:8022".to_string(), Value::Null);
        assert_eq!(tc_impl.borrow().manual_targets.get().await.unwrap(), json!(map));
    }

    #[fuchsia::test]
    async fn test_persisted_manual_target_load() {
        let env = ffx_config::test_init().await.unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        init_test_config(&env, temp.path()).await;

        let tc_impl = Rc::new(RefCell::new(TargetCollectionProtocol::default()));
        let fake_daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeMdns>()
            .register_fidl_protocol::<FakeFastboot>()
            .inject_fidl_protocol(tc_impl.clone())
            .build();
        tc_impl.borrow().manual_targets.add("127.0.0.1:8022".to_string()).await.unwrap();

        let cx = Context::new(fake_daemon);
        let target_collection = cx.get_target_collection().await.unwrap();
        // This happens in FidlProtocol::start(), but we want to avoid binding the
        // network sockets in unit tests, thus not calling start.
        let manual_targets_collection = tc_impl.borrow().manual_targets.clone();
        TargetCollectionProtocol::load_manual_targets(&cx, manual_targets_collection)
            .await
            .expect("Problem loading manual targets");

        let target = target_collection
            .query_single_enabled_target(&"127.0.0.1:8022".into())
            .unwrap()
            .expect("Could not find target");
        assert_eq!(target.ssh_address(), Some("127.0.0.1:8022".parse::<SocketAddr>().unwrap()));
    }

    #[fuchsia::test]
    async fn test_dynamic_open_target() {
        // Without doing an "add", we can still do an "open" if the target is specified
        // with IP:port
        let _env = ffx_config::test_init().await.unwrap();

        let tc_impl = Rc::new(RefCell::new(TargetCollectionProtocol::default()));
        let fake_daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeMdns>()
            .register_fidl_protocol::<FakeFastboot>()
            .inject_fidl_protocol(tc_impl.clone())
            .build();
        let (_client, server) = fidl::endpoints::create_endpoints::<ffx::TargetMarker>();
        let target_query = TargetQuery {
            string_matcher: Some("127.0.0.1:12345".to_string()),
            ..TargetQuery::default()
        };
        let proxy = fake_daemon.open_proxy::<ffx::TargetCollectionMarker>().await;
        // When this _doesn't_ work, we end up trying to discover the
        // device, a process which never completes
        timeout(Duration::from_secs(1), async {
            proxy.open_target(&target_query, server).await.unwrap().unwrap();
        })
        .await
        .unwrap();
        let target_collection = Context::new(fake_daemon).get_target_collection().await.unwrap();
        assert_eq!(1, target_collection.targets(None).len());
    }
}

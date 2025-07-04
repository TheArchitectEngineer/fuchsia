// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use addr::TargetIpAddr;
use anyhow::{anyhow, Result};
use async_utils::async_once::Once;
use ffx_daemon_events::TargetConnectionState;
use ffx_daemon_target::target::Target;
use ffx_daemon_target::zedboot::{reboot, reboot_to_bootloader, reboot_to_recovery};
use ffx_fastboot_connection_factory::{
    ConnectionFactory, FastbootConnectionFactory, FastbootConnectionKind,
};
use ffx_fastboot_interface::fastboot_interface::RebootEvent;
use ffx_ssh::ssh::build_ssh_command;
use ffx_target::FastbootInterface;
use fidl::endpoints::DiscoverableProtocolMarker as _;
use fidl::Error;
use fidl_fuchsia_developer_ffx::{TargetRebootError, TargetRebootResponder, TargetRebootState};
use fidl_fuchsia_developer_remotecontrol::RemoteControlProxy;
use fidl_fuchsia_hardware_power_statecontrol::{
    AdminMarker, AdminProxy, RebootOptions, RebootReason2,
};
use fidl_fuchsia_sys2 as fsys;
use fuchsia_async::TimeoutExt;
use futures::TryFutureExt;
use std::net::SocketAddr;
use std::process::Command;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};

// TODO(125639): Remove when Power Manager stabilizes
/// Configuration flag which enables using `dm` over ssh to reboot the target
/// when it is in product mode
const USE_SSH_FOR_REBOOT_FROM_PRODUCT: &'static str = "product.reboot.use_dm";

const ADMIN_MONIKER: &'static str = "/bootstrap/shutdown_shim";

pub(crate) struct RebootController {
    target: Rc<Target>,
    remote_proxy: Once<RemoteControlProxy>,
    admin_proxy: Once<AdminProxy>,
    overnet_node: Arc<overnet_core::Router>,
    fastboot_connection_builder: Box<dyn FastbootConnectionFactory>,
}

#[derive(thiserror::Error, Debug, Clone)]
enum FastbootConnectionError {
    #[error("Passed Target Name was empty. Target name needs to be a non-empty string to support Target rediscovery")]
    EmptyTargetName,
}

impl RebootController {
    pub(crate) fn new(target: Rc<Target>, overnet_node: Arc<overnet_core::Router>) -> Self {
        Self {
            target,
            remote_proxy: Once::new(),
            admin_proxy: Once::new(),
            overnet_node,
            fastboot_connection_builder: Box::new(ConnectionFactory {}),
        }
    }

    async fn get_remote_proxy(&self) -> Result<RemoteControlProxy> {
        // TODO(awdavies): Factor out init_remote_proxy from the target, OR
        // move the impl(s) here that rely on remote control to use init_remote_proxy
        // instead.
        self.remote_proxy
            .get_or_try_init(self.target.init_remote_proxy(&self.overnet_node))
            .await
            .map(|proxy| proxy.clone())
    }

    fn nodename(&self) -> Result<String> {
        let target_name = self.target.nodename_str();
        if target_name.is_empty() {
            return Err(FastbootConnectionError::EmptyTargetName.into());
        }
        Ok(target_name)
    }

    async fn get_admin_proxy(&self) -> Result<AdminProxy> {
        self.admin_proxy.get_or_try_init(self.init_admin_proxy()).await.map(|p| p.clone())
    }

    async fn init_admin_proxy(&self) -> Result<AdminProxy> {
        let rcs_proxy = self.get_remote_proxy().await?;
        // Try to connect via fuchsia.developer.remotecontrol/RemoteControl.ConnectCapability.
        let (proxy, server) = fidl::endpoints::create_proxy::<AdminMarker>();
        if let Ok(response) = rcs_proxy
            .connect_capability(
                ADMIN_MONIKER,
                fsys::OpenDirType::ExposedDir,
                AdminMarker::PROTOCOL_NAME,
                server.into_channel(),
            )
            .await
        {
            response.map_err(|e| anyhow!("could not get admin proxy: {e:?}"))?;
            return Ok(proxy);
        }
        // Fallback to fuchsia.developer.remotecontrol/RemoteControl.DeprecatedOpenCapability.
        // This can be removed once we drop support for API level 27.
        let (proxy, server) = fidl::endpoints::create_proxy::<AdminMarker>();
        rcs_proxy
            .deprecated_open_capability(
                ADMIN_MONIKER,
                fsys::OpenDirType::ExposedDir,
                AdminMarker::PROTOCOL_NAME,
                server.into_channel(),
                Default::default(),
            )
            .await?
            .map_err(|_| anyhow!("could not get admin proxy"))?;
        return Ok(proxy);
    }

    pub(crate) async fn reboot(
        &self,
        state: TargetRebootState,
        responder: TargetRebootResponder,
    ) -> Result<()> {
        match self.target.get_connection_state() {
            TargetConnectionState::Fastboot(_) => match state {
                TargetRebootState::Product => {
                    let mut fastboot_interface = match self
                        .target
                        .fastboot_interface()
                        .ok_or_else(|| anyhow!("No fastboot interface"))?
                    {
                        FastbootInterface::Tcp => {
                            let address: SocketAddr = self
                                .target
                                .fastboot_address()
                                .ok_or_else(|| anyhow!("No fastboot address"))?
                                .0
                                .into();
                            let target_name = self.nodename()?;
                            self.fastboot_connection_builder
                                .build_interface(FastbootConnectionKind::Tcp(target_name, address))
                                .await?
                        }
                        FastbootInterface::Udp => {
                            let address: SocketAddr = self
                                .target
                                .fastboot_address()
                                .ok_or_else(|| anyhow!("No fastboot address"))?
                                .0
                                .into();
                            let target_name = self.nodename()?;
                            self.fastboot_connection_builder
                                .build_interface(FastbootConnectionKind::Udp(target_name, address))
                                .await?
                        }
                        FastbootInterface::Usb => {
                            let serial =
                                self.target.serial().ok_or_else(|| anyhow!("No serial number"))?;
                            self.fastboot_connection_builder
                                .build_interface(FastbootConnectionKind::Usb(serial))
                                .await?
                        }
                    };

                    match fastboot_interface.reboot().await {
                        Ok(_) => responder.send(Ok(())).map_err(Into::into),
                        Err(e) => {
                            log::error!("Fastboot communication error: {:?}", e);
                            responder
                                .send(Err(TargetRebootError::FastbootCommunication))
                                .map_err(Into::into)
                        }
                    }
                }
                TargetRebootState::Bootloader => {
                    let (reboot_client, _reboot_server): (
                        Sender<RebootEvent>,
                        Receiver<RebootEvent>,
                    ) = mpsc::channel(1);

                    let mut fastboot_interface = match self
                        .target
                        .fastboot_interface()
                        .ok_or_else(|| anyhow!("No fastboot interface"))?
                    {
                        FastbootInterface::Tcp => {
                            let address: SocketAddr = self
                                .target
                                .fastboot_address()
                                .ok_or_else(|| anyhow!("No fastboot address"))?
                                .0
                                .into();
                            let target_name = self.nodename()?;
                            self.fastboot_connection_builder
                                .build_interface(FastbootConnectionKind::Tcp(target_name, address))
                                .await?
                        }
                        FastbootInterface::Udp => {
                            let address: SocketAddr = self
                                .target
                                .fastboot_address()
                                .ok_or_else(|| anyhow!("No fastboot address"))?
                                .0
                                .into();
                            let target_name = self.nodename()?;
                            self.fastboot_connection_builder
                                .build_interface(FastbootConnectionKind::Udp(target_name, address))
                                .await?
                        }
                        FastbootInterface::Usb => {
                            let serial =
                                self.target.serial().ok_or_else(|| anyhow!("No serial number"))?;
                            self.fastboot_connection_builder
                                .build_interface(FastbootConnectionKind::Usb(serial))
                                .await?
                        }
                    };

                    match fastboot_interface.reboot_bootloader(reboot_client).await {
                        Ok(_) => responder.send(Ok(())).map_err(Into::into),
                        Err(e) => {
                            log::error!("Fastboot communication error: {:?}", e);
                            responder
                                .send(Err(TargetRebootError::FastbootCommunication))
                                .map_err(Into::into)
                        }
                    }
                }
                TargetRebootState::Recovery => {
                    responder.send(Err(TargetRebootError::FastbootToRecovery)).map_err(Into::into)
                }
            },
            TargetConnectionState::Zedboot(_) => {
                let response = if let Some(addr) = self.target.netsvc_address() {
                    match state {
                        TargetRebootState::Product => reboot(addr).await.map(|_| ()).map_err(|e| {
                            log::error!("zedboot reboot failed {:?}", e);
                            TargetRebootError::NetsvcCommunication
                        }),
                        TargetRebootState::Bootloader => {
                            reboot_to_bootloader(addr).await.map(|_| ()).map_err(|e| {
                                log::error!("zedboot reboot to bootloader failed {:?}", e);
                                TargetRebootError::NetsvcCommunication
                            })
                        }
                        TargetRebootState::Recovery => {
                            reboot_to_recovery(addr).await.map(|_| ()).map_err(|e| {
                                log::error!("zedboot reboot to recovery failed {:?}", e);
                                TargetRebootError::NetsvcCommunication
                            })
                        }
                    }
                } else {
                    Err(TargetRebootError::NetsvcAddressNotFound)
                };
                responder.send(response).map_err(Into::into)
            }
            // Everything else use AdminProxy
            _ => {
                //TODO(125639): Remove when Power Manager stabilizes
                let use_ssh_for_reboot: bool =
                    ffx_config::get(USE_SSH_FOR_REBOOT_FROM_PRODUCT).unwrap_or(false);

                if use_ssh_for_reboot {
                    let res = run_ssh_command(Rc::downgrade(&self.target), state).await;
                    match res {
                        Ok(_) => responder.send(Ok(())).map_err(Into::into),
                        Err(e) => {
                            log::error!("Target communication error when rebooting: {:?}", e);
                            responder
                                .send(Err(TargetRebootError::TargetCommunication))
                                .map_err(Into::into)
                        }
                    }
                } else {
                    let admin_proxy = match self
                        .get_admin_proxy()
                        .map_err(|e| {
                            log::warn!("error getting admin proxy: {}", e);
                            TargetRebootError::TargetCommunication
                        })
                        .on_timeout(Duration::from_secs(5), || {
                            log::warn!("timed out getting admin proxy");
                            Err(TargetRebootError::TargetCommunication)
                        })
                        .await
                    {
                        Ok(a) => a,
                        Err(e) => {
                            responder.send(Err(e))?;
                            return Err(anyhow!("failed to get admin proxy"));
                        }
                    };
                    match state {
                        TargetRebootState::Product => {
                            match admin_proxy
                                .perform_reboot(&RebootOptions {
                                    reasons: Some(vec![RebootReason2::UserRequest]),
                                    ..Default::default()
                                })
                                .await
                            {
                                Ok(_) => responder.send(Ok(())).map_err(Into::into),
                                Err(e) => {
                                    handle_fidl_connection_err(e, responder).map_err(Into::into)
                                }
                            }
                        }
                        TargetRebootState::Bootloader => {
                            match admin_proxy.reboot_to_bootloader().await {
                                Ok(_) => responder.send(Ok(())).map_err(Into::into),
                                Err(e) => {
                                    handle_fidl_connection_err(e, responder).map_err(Into::into)
                                }
                            }
                        }
                        TargetRebootState::Recovery => match admin_proxy.reboot_to_recovery().await
                        {
                            Ok(_) => responder.send(Ok(())).map_err(Into::into),
                            Err(e) => handle_fidl_connection_err(e, responder).map_err(Into::into),
                        },
                    }
                }
            }
        }
    }
}

pub(crate) fn handle_fidl_connection_err(e: Error, responder: TargetRebootResponder) -> Result<()> {
    match e {
        Error::ClientChannelClosed { protocol_name, .. } => {
            // Changing this to an info from warn since reboot has succeeded The assumption that
            // reboot has succeeded is correct since we received a ClientChannelClosed
            // successfully. So let's just make the message clearer to the user.
            //
            // Check the 'protocol_name' and if it is 'fuchsia.hardware.power.statecontrol.Admin'
            // then we can be more confident that target reboot/shutdown has succeeded.
            if protocol_name == "fuchsia.hardware.power.statecontrol.Admin" {
                log::info!("Target reboot succeeded.");
            } else {
                log::info!("Assuming target reboot succeeded. Client received a PEER_CLOSED from '{protocol_name}'");
            }
            log::debug!("{:?}", e);
            responder.send(Ok(()))?;
        }
        _ => {
            log::error!("Target communication error: {:?}", e);
            responder.send(Err(TargetRebootError::TargetCommunication))?;
        }
    }
    Ok(())
}

async fn run_ssh_command(target: Weak<Target>, state: TargetRebootState) -> Result<()> {
    let t =
        target.upgrade().ok_or_else(|| anyhow!("Could not upgrade Target to build ssh command"))?;
    let addr = t.ssh_address().ok_or_else(|| anyhow!("Could not get ssh address for target"))?;
    let mut cmd = build_ssh_command_local(addr.into(), state).await?;
    log::debug!("About to run command on target to reboot: {:?}", cmd);
    let ssh = cmd.spawn()?;
    let output = ssh.wait_with_output()?;
    match output.status.success() {
        true => Ok(()),
        _ => {
            // Exit code 255 indicates that the ssh connection was suddenly dropped
            // assume this is correct behaviour and return
            if let Some(255) = output.status.code() {
                Ok(())
            } else {
                let stdout = output.stdout;
                log::error!(
                    "Error rebooting. Error code: {:?}. Output from ssh command: {:?}",
                    output.status.code(),
                    stdout
                );
                Err(anyhow!("Error using `dm` command to reboot to bootloader. Check Daemon Logs"))
            }
        }
    }
}

async fn build_ssh_command_local(
    addr: TargetIpAddr,
    desired_state: TargetRebootState,
) -> Result<Command> {
    let device_command = match desired_state {
        TargetRebootState::Bootloader => vec!["dm", "reboot-bootloader"],
        TargetRebootState::Recovery => vec!["dm", "reboot-recovery"],
        TargetRebootState::Product => vec!["dm", "reboot"],
    };
    Ok(build_ssh_command(netext::ScopedSocketAddr::from_socket_addr(addr.into())?, device_command)?)
}

// END BLOCK

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use assert_matches::assert_matches;
    use ffx_fastboot_connection_factory::test::setup_connection_factory;
    use fidl::endpoints::{create_proxy_and_stream, RequestStream};
    use fidl_fuchsia_developer_ffx::{TargetMarker, TargetProxy, TargetRequest};
    use fidl_fuchsia_developer_remotecontrol::{RemoteControlMarker, RemoteControlRequest};
    use fidl_fuchsia_hardware_power_statecontrol::{AdminRequest, AdminRequestStream};
    use futures::TryStreamExt;
    use std::time::Instant;

    fn setup_admin(chan: fidl::Channel) -> Result<()> {
        let mut stream = AdminRequestStream::from_channel(fidl::AsyncChannel::from_channel(chan));
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    AdminRequest::PerformReboot {
                        options: RebootOptions { reasons: Some(reasons), .. },
                        responder,
                    } => {
                        assert_matches!(&reasons[..], [RebootReason2::UserRequest]);
                        responder.send(Ok(())).unwrap();
                    }
                    AdminRequest::RebootToBootloader { responder } => {
                        responder.send(Ok(())).unwrap();
                    }
                    AdminRequest::RebootToRecovery { responder } => {
                        responder.send(Ok(())).unwrap();
                    }
                    _ => assert!(false),
                }
            }
        })
        .detach();
        Ok(())
    }

    async fn setup_remote() -> RemoteControlProxy {
        let (proxy, mut stream) = fidl::endpoints::create_proxy_and_stream::<RemoteControlMarker>();
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    RemoteControlRequest::ConnectCapability {
                        server_channel, responder, ..
                    } => {
                        setup_admin(server_channel).unwrap();
                        responder.send(Ok(())).unwrap();
                    }
                    _ => panic!("Unhandled request: {req:?}"),
                }
            }
        })
        .detach();
        proxy
    }

    async fn setup_inner(target: Rc<Target>) -> (Rc<Target>, TargetProxy) {
        let overnet_node = overnet_core::Router::new(None).unwrap();
        let remote_proxy = Once::new();
        let _ = remote_proxy.get_or_init(setup_remote()).await;
        let admin_proxy = Once::new();
        let (_, connection_builder) = setup_connection_factory();
        let rc = RebootController {
            target: target.clone(),
            remote_proxy,
            admin_proxy,
            overnet_node,
            fastboot_connection_builder: Box::new(connection_builder),
        };
        let (proxy, mut stream) = create_proxy_and_stream::<TargetMarker>();
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    TargetRequest::Reboot { state, responder } => {
                        rc.reboot(state, responder).await.unwrap();
                    }
                    r => panic!("received unexpected request {:?}", r),
                }
            }
        })
        .detach();
        (target, proxy)
    }

    async fn setup() -> (Rc<Target>, TargetProxy) {
        let target = Target::new_named("scooby-dooby-doo");
        setup_inner(target).await
    }

    async fn setup_usb() -> (Rc<Target>, TargetProxy) {
        let target = Target::new_for_usb("1DISTHISAREALSERIAL");
        setup_inner(target).await
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_reboot_product() -> Result<()> {
        let (_, proxy) = setup().await;
        proxy
            .reboot(TargetRebootState::Product)
            .await?
            .map_err(|e| anyhow!("error rebooting: {:?}", e))
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_reboot_recovery() -> Result<()> {
        let (_, proxy) = setup().await;
        proxy
            .reboot(TargetRebootState::Recovery)
            .await?
            .map_err(|e| anyhow!("error rebooting: {:?}", e))
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_reboot_bootloader() -> Result<()> {
        let (_, proxy) = setup().await;
        proxy
            .reboot(TargetRebootState::Bootloader)
            .await?
            .map_err(|e| anyhow!("error rebooting: {:?}", e))
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_fastboot_reboot_product() -> Result<()> {
        let (target, proxy) = setup_usb().await;
        target.set_state(TargetConnectionState::Fastboot(Instant::now()));
        proxy
            .reboot(TargetRebootState::Product)
            .await?
            .map_err(|e| anyhow!("error rebooting: {:?}", e))
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_fastboot_reboot_recovery() -> Result<()> {
        let (target, proxy) = setup_usb().await;
        target.set_state(TargetConnectionState::Fastboot(Instant::now()));
        assert!(proxy.reboot(TargetRebootState::Recovery).await?.is_err());
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_fastboot_reboot_bootloader() -> Result<()> {
        let (target, proxy) = setup_usb().await;
        target.set_state(TargetConnectionState::Fastboot(Instant::now()));
        proxy
            .reboot(TargetRebootState::Bootloader)
            .await?
            .map_err(|e| anyhow!("error rebooting: {:?}", e))
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_zedboot_reboot_bootloader() -> Result<()> {
        let (target, proxy) = setup().await;
        target.set_state(TargetConnectionState::Zedboot(Instant::now()));
        assert!(proxy.reboot(TargetRebootState::Bootloader).await?.is_err());
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_zedboot_reboot_recovery() -> Result<()> {
        let (target, proxy) = setup().await;
        target.set_state(TargetConnectionState::Zedboot(Instant::now()));
        assert!(proxy.reboot(TargetRebootState::Recovery).await?.is_err());
        Ok(())
    }
}

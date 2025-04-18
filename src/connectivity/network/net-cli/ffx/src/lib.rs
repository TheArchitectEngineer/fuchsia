// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_writer::MachineWriter;
use fho::{user_error, FfxMain, FfxTool};
use fidl::endpoints::{DiscoverableProtocolMarker, ProtocolMarker};
use std::ops::Deref as _;
use target_holders::RemoteControlProxyHolder;
use {
    fidl_fuchsia_developer_remotecontrol as fremotecontrol, fidl_fuchsia_net_debug as fdebug,
    fidl_fuchsia_net_dhcp as fdhcp, fidl_fuchsia_net_filter as ffilter,
    fidl_fuchsia_net_filter_deprecated as ffilter_deprecated,
    fidl_fuchsia_net_interfaces as finterfaces,
    fidl_fuchsia_net_interfaces_admin as finterfaces_admin, fidl_fuchsia_net_name as fname,
    fidl_fuchsia_net_neighbor as fneighbor, fidl_fuchsia_net_root as froot,
    fidl_fuchsia_net_routes as froutes, fidl_fuchsia_net_stack as fstack,
    fidl_fuchsia_net_stackmigrationdeprecated as fnet_migration, fidl_fuchsia_sys2 as fsys,
};

const NETSTACK_MONIKER_SUFFIX: &str = "/netstack";
const DHCPD_MONIKER_SUFFIX: &str = "/dhcpd";
const DNS_MONIKER_SUFFIX: &str = "/dns-resolver";
const NETWORK_REALM: &str = "/core/network";
const MIGRATION_CONTROLLER_SUFFIX: &str = "/netstack-migration";

struct FfxConnector<'a> {
    remote_control: fremotecontrol::RemoteControlProxy,
    realm: &'a str,
}

impl FfxConnector<'_> {
    async fn remotecontrol_connect<S: DiscoverableProtocolMarker>(
        &self,
        moniker_suffix: &str,
    ) -> Result<S::Proxy, anyhow::Error> {
        let Self { remote_control, realm } = &self;
        let mut moniker = format!("{}{}", realm, moniker_suffix);
        // TODO: remove once all clients of this tool are passing monikers instead of selectors for
        // `realm`.
        if !moniker.starts_with("/") {
            moniker = moniker.replace("\\:", ":");
            moniker = format!("/{moniker}");
        }
        let (proxy, server_end) = fidl::endpoints::create_proxy::<S>();
        remote_control
            .connect_capability(
                &moniker,
                fsys::OpenDirType::ExposedDir,
                S::PROTOCOL_NAME,
                server_end.into_channel(),
            )
            .await?
            .map_err(|e| {
                anyhow::anyhow!("failed to connect to {} at {}: {:?}", S::PROTOCOL_NAME, moniker, e)
            })?;
        Ok(proxy)
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<fdebug::InterfacesMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<fdebug::InterfacesMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<fdebug::InterfacesMarker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<froot::InterfacesMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<froot::InterfacesMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<froot::InterfacesMarker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<froot::FilterMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<froot::FilterMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<froot::FilterMarker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<fdhcp::Server_Marker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<fdhcp::Server_Marker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<fdhcp::Server_Marker>(DHCPD_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<ffilter_deprecated::FilterMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<ffilter_deprecated::FilterMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<ffilter_deprecated::FilterMarker>(NETSTACK_MONIKER_SUFFIX)
            .await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<ffilter::StateMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<ffilter::StateMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<ffilter::StateMarker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<finterfaces::StateMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<finterfaces::StateMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<finterfaces::StateMarker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<finterfaces_admin::InstallerMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<finterfaces_admin::InstallerMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<finterfaces_admin::InstallerMarker>(NETSTACK_MONIKER_SUFFIX)
            .await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<fneighbor::ControllerMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<fneighbor::ControllerMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<fneighbor::ControllerMarker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<fneighbor::ViewMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<fneighbor::ViewMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<fneighbor::ViewMarker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<fstack::LogMarker> for FfxConnector<'_> {
    async fn connect(&self) -> Result<<fstack::LogMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<fstack::LogMarker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<fstack::StackMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<fstack::StackMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<fstack::StackMarker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<froutes::StateV4Marker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<froutes::StateV4Marker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<froutes::StateV4Marker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<froutes::StateV6Marker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<froutes::StateV6Marker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<froutes::StateV6Marker>(NETSTACK_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<fname::LookupMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<fname::LookupMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<fname::LookupMarker>(DNS_MONIKER_SUFFIX).await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<fnet_migration::ControlMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<fnet_migration::ControlMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<fnet_migration::ControlMarker>(MIGRATION_CONTROLLER_SUFFIX)
            .await
    }
}

#[async_trait::async_trait]
impl net_cli::ServiceConnector<fnet_migration::StateMarker> for FfxConnector<'_> {
    async fn connect(
        &self,
    ) -> Result<<fnet_migration::StateMarker as ProtocolMarker>::Proxy, anyhow::Error> {
        self.remotecontrol_connect::<fnet_migration::StateMarker>(MIGRATION_CONTROLLER_SUFFIX).await
    }
}

#[derive(FfxTool)]
pub struct NetTool {
    #[command]
    pub cmd: ffx_net_args::Command,
    pub remote_control: RemoteControlProxyHolder,
}

#[async_trait::async_trait(?Send)]
impl FfxMain for NetTool {
    type Writer = MachineWriter<serde_json::Value>;
    async fn main(self, writer: <Self as fho::FfxMain>::Writer) -> fho::Result<()> {
        self.net(writer).await
    }
}

fho::embedded_plugin!(NetTool);

impl NetTool {
    async fn net(&self, writer: <Self as fho::FfxMain>::Writer) -> fho::Result<()> {
        let realm = self.cmd.realm.as_deref().unwrap_or(NETWORK_REALM);
        let res = net_cli::do_root(
            writer.into(),
            net_cli::Command { cmd: self.cmd.cmd.clone() },
            &FfxConnector { remote_control: self.remote_control.deref().clone(), realm },
        )
        .await
        .map_err(|e| match net_cli::underlying_user_facing_error(&e) {
            Some(net_cli::UserFacingError { msg }) => {
                user_error!(msg)
            }
            None => e.into(),
        });
        res.into()
    }
}

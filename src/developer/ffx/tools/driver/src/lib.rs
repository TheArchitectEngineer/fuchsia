// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{Context, Result};
use component_debug::capability;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool};
use fidl::endpoints::{DiscoverableProtocolMarker, ProtocolMarker};
use target_holders::RemoteControlProxyHolder;
use {
    fidl_fuchsia_developer_remotecontrol as rc, fidl_fuchsia_driver_development as fdd,
    fidl_fuchsia_driver_registrar as fdr, fidl_fuchsia_sys2 as fsys,
    fidl_fuchsia_test_manager as ftm,
};

mod args;

struct DriverConnector {
    remote_control: fho::Result<RemoteControlProxyHolder>,
}

struct CapabilityOptions {
    capability_name: &'static str,
    default_capability_name_for_query: &'static str,
}

struct DiscoverableCapabilityOptions<P> {
    _phantom: std::marker::PhantomData<P>,
}

// #[derive(Default)] imposes a spurious P: Default bound.
impl<P> Default for DiscoverableCapabilityOptions<P> {
    fn default() -> Self {
        Self { _phantom: Default::default() }
    }
}

impl<P: DiscoverableProtocolMarker> Into<CapabilityOptions> for DiscoverableCapabilityOptions<P> {
    fn into(self) -> CapabilityOptions {
        CapabilityOptions {
            capability_name: P::PROTOCOL_NAME,
            default_capability_name_for_query: P::PROTOCOL_NAME,
        }
    }
}

// Gets monikers for components that expose a capability matching the given |query|.
// This moniker is eventually converted into a selector and is used to connecting to
// the capability.
async fn find_components_with_capability(
    query_proxy: &fsys::RealmQueryProxy,
    query: &str,
) -> Result<Vec<String>> {
    Ok(capability::get_all_route_segments(query.to_string(), &query_proxy)
        .await?
        .iter()
        .filter_map(|segment| {
            if let capability::RouteSegment::ExposeBy { moniker, .. } = segment {
                Some(moniker.to_string())
            } else {
                None
            }
        })
        .collect())
}

/// Find the components that expose a given capability, and let the user
/// request which component they would like to connect to.
async fn user_choose_selector(
    query_proxy: &fsys::RealmQueryProxy,
    capability: &str,
) -> Result<String> {
    let capabilities = find_components_with_capability(query_proxy, capability).await?;
    println!("Please choose which component to connect to:");
    for (i, component) in capabilities.iter().enumerate() {
        println!("    {}: {}", i, component)
    }

    let mut line_editor = rustyline::Editor::<()>::new();
    loop {
        let line = line_editor.readline("$ ")?;
        let choice = line.trim().parse::<usize>();
        if choice.is_err() {
            println!("Error: please choose a value.");
            continue;
        }
        let choice = choice.unwrap();
        if choice >= capabilities.len() {
            println!("Error: please choose a correct value.");
            continue;
        }
        // We have to escape colons in the capability name to distinguish them from the
        // syntactically meaningful colons in the ':expose:" string.
        return Ok(capabilities[choice].clone());
    }
}

impl DriverConnector {
    fn new(remote_control: fho::Result<RemoteControlProxyHolder>) -> Self {
        Self { remote_control }
    }

    async fn get_component_with_capability<S: ProtocolMarker>(
        &self,
        moniker: &str,
        capability_options: impl Into<CapabilityOptions>,
        select: bool,
    ) -> Result<S::Proxy> {
        async fn remotecontrol_connect<S: ProtocolMarker>(
            remote_control: &rc::RemoteControlProxy,
            moniker: &str,
            capability: &str,
        ) -> Result<S::Proxy> {
            // Try to connect via fuchsia.developer.remotecontrol/RemoteControl.ConnectCapability.
            let (proxy, server) = fidl::endpoints::create_proxy::<S>();
            if let Ok(response) = remote_control
                .connect_capability(
                    moniker,
                    fsys::OpenDirType::ExposedDir,
                    capability,
                    server.into_channel(),
                )
                .await
            {
                response.map_err(|e| {
                    anyhow::anyhow!(
                        "failed to connect to {} at {} as {}: {:?}",
                        S::DEBUG_NAME,
                        moniker,
                        capability,
                        e
                    )
                })?;
                return Ok(proxy);
            }
            // Fallback to fuchsia.developer.remotecontrol/RemoteControl.DeprecatedOpenCapability.
            // This can be removed once we drop support for API level 27.
            let (proxy, server) = fidl::endpoints::create_proxy::<S>();
            remote_control
                .deprecated_open_capability(
                    moniker,
                    fsys::OpenDirType::ExposedDir,
                    capability,
                    server.into_channel(),
                    Default::default(),
                )
                .await?
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to connect to {} at {} as {}: {:?}",
                        S::DEBUG_NAME,
                        moniker,
                        capability,
                        e
                    )
                })?;
            return Ok(proxy);
        }

        let CapabilityOptions { capability_name, default_capability_name_for_query } =
            capability_options.into();

        let Ok(ref remote_control) = self.remote_control else {
            anyhow::bail!("{}", self.remote_control.as_ref().unwrap_err());
        };
        let (moniker, capability): (String, &str) = match select {
            true => {
                let query_proxy =
                    rcs::root_realm_query(remote_control, std::time::Duration::from_secs(15))
                        .await
                        .context("opening query")?;
                (user_choose_selector(&query_proxy, capability_name).await?, capability_name)
            }
            false => (moniker.to_string(), default_capability_name_for_query),
        };
        remotecontrol_connect::<S>(&remote_control, &moniker, &capability).await
    }
}

#[async_trait::async_trait]
impl driver_connector::DriverConnector for DriverConnector {
    async fn get_driver_development_proxy(&self, select: bool) -> Result<fdd::ManagerProxy> {
        self.get_component_with_capability::<fdd::ManagerMarker>(
            "/bootstrap/driver_manager",
            DiscoverableCapabilityOptions::<fdd::ManagerMarker>::default(),
            select,
        )
        .await
        .context("Failed to get driver development component")
    }

    async fn get_driver_registrar_proxy(&self, select: bool) -> Result<fdr::DriverRegistrarProxy> {
        self.get_component_with_capability::<fdr::DriverRegistrarMarker>(
            "/bootstrap/driver_index",
            DiscoverableCapabilityOptions::<fdr::DriverRegistrarMarker>::default(),
            select,
        )
        .await
        .context("Failed to get driver registrar component")
    }

    async fn get_suite_runner_proxy(&self) -> Result<ftm::SuiteRunnerProxy> {
        self.get_component_with_capability::<ftm::SuiteRunnerMarker>(
            "/core/test_manager",
            DiscoverableCapabilityOptions::<ftm::SuiteRunnerMarker>::default(),
            false,
        )
        .await
        .context("Failed to get SuiteRunner component")
    }
}

#[derive(FfxTool)]
pub struct DriverTool {
    remote_control: fho::Result<RemoteControlProxyHolder>,
    #[command]
    cmd: args::DriverCommand,
}

#[async_trait::async_trait(?Send)]
impl FfxMain for DriverTool {
    type Writer = SimpleWriter;

    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        driver_tools::driver(
            self.cmd.into(),
            DriverConnector::new(self.remote_control),
            &mut writer,
        )
        .await
        .map_err(Into::into)
    }
}

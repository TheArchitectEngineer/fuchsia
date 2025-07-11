// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assert_matches::assert_matches;
use fidl_fuchsia_net::{IpAddress, SocketAddress};
use fidl_fuchsia_net_policy_socketproxy::{
    DnsServerList, FuchsiaNetworkInfo, FuchsiaNetworksProxy, FuchsiaNetworksRequest,
    FuchsiaNetworksRequestStream, Network, NetworkDnsServers, NetworkInfo,
    NetworkRegistryAddResult, NetworkRegistryRemoveResult, NetworkRegistrySetDefaultResult,
    NetworkRegistryUpdateResult, StarnixNetworkInfo, StarnixNetworksProxy,
};
use fidl_fuchsia_posix_socket::OptionalUint32;
use futures::{FutureExt as _, StreamExt as _};
use socket_proxy::NetworkRegistryError;
use std::future::Future;

fn dns_server_list(id: u32) -> DnsServerList {
    DnsServerList { source_network_id: Some(id), addresses: Some(vec![]), ..Default::default() }
}

fn starnix_network_info(mark: u32) -> NetworkInfo {
    NetworkInfo::Starnix(StarnixNetworkInfo {
        mark: Some(mark),
        handle: Some(0),
        ..Default::default()
    })
}

fn starnix_network(network_id: u32) -> Network {
    Network {
        network_id: Some(network_id),
        info: Some(starnix_network_info(network_id)),
        dns_servers: Some(Default::default()),
        ..Default::default()
    }
}

fn fuchsia_network(network_id: u32) -> Network {
    Network {
        network_id: Some(network_id),
        info: Some(NetworkInfo::Fuchsia(FuchsiaNetworkInfo { ..Default::default() })),
        dns_servers: Some(Default::default()),
        ..Default::default()
    }
}

pub trait ToNetwork {
    fn to_network(self, registry: RegistryType) -> Network;
}

pub trait ToDnsServerList {
    fn to_dns_server_list(self) -> DnsServerList;
}

impl ToNetwork for u32 {
    fn to_network(self, registry: RegistryType) -> Network {
        match registry {
            RegistryType::Starnix => starnix_network(self),
            RegistryType::Fuchsia => fuchsia_network(self),
        }
    }
}

impl ToDnsServerList for u32 {
    fn to_dns_server_list(self) -> DnsServerList {
        dns_server_list(self)
    }
}

pub enum RegistryType {
    Starnix,
    Fuchsia,
}

impl ToNetwork for (u32, Vec<IpAddress>) {
    fn to_network(self, registry: RegistryType) -> Network {
        let (v4, v6) = self.1.iter().fold((Vec::new(), Vec::new()), |(mut v4s, mut v6s), s| {
            match s {
                IpAddress::Ipv4(v4) => v4s.push(*v4),
                IpAddress::Ipv6(v6) => v6s.push(*v6),
            }
            (v4s, v6s)
        });
        let base = match registry {
            RegistryType::Starnix => starnix_network(self.0),
            RegistryType::Fuchsia => fuchsia_network(self.0),
        };
        Network {
            dns_servers: Some(NetworkDnsServers {
                v4: Some(v4),
                v6: Some(v6),
                ..Default::default()
            }),
            ..base
        }
    }
}

impl ToDnsServerList for (u32, Vec<SocketAddress>) {
    fn to_dns_server_list(self) -> DnsServerList {
        DnsServerList { addresses: Some(self.1), ..dns_server_list(self.0) }
    }
}

impl<N: ToNetwork + Clone> ToNetwork for &N {
    fn to_network(self, registry: RegistryType) -> Network {
        self.clone().to_network(registry)
    }
}

impl<D: ToDnsServerList + Clone> ToDnsServerList for &D {
    fn to_dns_server_list(self) -> DnsServerList {
        self.clone().to_dns_server_list()
    }
}

pub trait NetworkRegistry {
    fn set_default(
        &self,
        network_id: &OptionalUint32,
    ) -> impl Future<Output = Result<NetworkRegistrySetDefaultResult, fidl::Error>>;
    fn add(
        &self,
        network: &Network,
    ) -> impl Future<Output = Result<NetworkRegistryAddResult, fidl::Error>>;
    fn update(
        &self,
        network: &Network,
    ) -> impl Future<Output = Result<NetworkRegistryUpdateResult, fidl::Error>>;
    fn remove(
        &self,
        network_id: u32,
    ) -> impl Future<Output = Result<NetworkRegistryRemoveResult, fidl::Error>>;
}

macro_rules! impl_network_registry {
    ($($ty:ty),*) => {
        $(
            impl NetworkRegistry for $ty {
                fn set_default(
                    &self,
                    network_id: &OptionalUint32,
                ) -> impl Future<Output = Result<NetworkRegistrySetDefaultResult, fidl::Error>> {
                    self.set_default(network_id)
                }

                fn add(
                    &self,
                    network: &Network,
                ) -> impl Future<Output = Result<NetworkRegistryAddResult, fidl::Error>> {
                    self.add(network)
                }

                fn update(
                    &self,
                    network: &Network,
                ) -> impl Future<Output = Result<NetworkRegistryUpdateResult, fidl::Error>> {
                    self.update(network)
                }

                fn remove(
                    &self,
                    network_id: u32,
                ) -> impl Future<Output = Result<NetworkRegistryRemoveResult, fidl::Error>> {
                    self.remove(network_id)
                }
            }
        )*
    };
    ($($ty:ty),*,) => { impl_network_registry!($($ty),*); };
}

impl_network_registry!(StarnixNetworksProxy, FuchsiaNetworksProxy);

pub async fn respond_to_socketproxy(
    socket_proxy_req_stream: &mut FuchsiaNetworksRequestStream,
    result: Result<(), NetworkRegistryError>,
) {
    socket_proxy_req_stream
        .next()
        .map(|req| match req.expect("request stream ended").expect("receive request") {
            FuchsiaNetworksRequest::SetDefault { network_id: _, responder } => {
                let res = result.map_err(|e| {
                    assert_matches!(e, NetworkRegistryError::SetDefault(err) => {
                        return err;
                    });
                });
                responder.send(res).expect("respond to SetDefault");
            }
            FuchsiaNetworksRequest::Add { network: _, responder } => {
                let res = result.map_err(|e| {
                    assert_matches!(e, NetworkRegistryError::Add(err) => {
                        return err;
                    });
                });
                responder.send(res).expect("respond to Add");
            }
            FuchsiaNetworksRequest::Update { network: _, responder: _ } => {
                unreachable!("not called in tests");
            }
            FuchsiaNetworksRequest::Remove { network_id: _, responder } => {
                let res = result.map_err(|e| {
                    assert_matches!(e, NetworkRegistryError::Remove(err) => {
                        return err;
                    });
                });
                responder.send(res).expect("respond to Remove");
            }
            FuchsiaNetworksRequest::CheckPresence { responder: _ } => {
                unreachable!("not called in tests");
            }
        })
        .await;
}

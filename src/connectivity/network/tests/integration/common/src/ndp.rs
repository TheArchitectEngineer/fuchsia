// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helpers for tests involving the Neighbor Discovery Protocol.

use crate::constants;
use anyhow::Context as _;
use assert_matches::assert_matches;
use fuchsia_async::{DurationExt as _, TimeoutExt as _};
use futures::{future, FutureExt as _, Stream, StreamExt as _, TryStreamExt as _};
use net_types::ip::Ip as _;
use net_types::Witness as _;
use packet::serialize::{InnerPacketBuilder, Serializer};
use packet::Buf;
use packet_formats::ethernet::{
    EtherType, EthernetFrameBuilder, EthernetFrameLengthCheck, ETHERNET_MIN_BODY_LEN_NO_TAG,
};
use packet_formats::icmp::ndp::options::{NdpOption, NdpOptionBuilder};
use packet_formats::icmp::ndp::{
    NeighborAdvertisement, NeighborSolicitation, OptionSequenceBuilder, RouterAdvertisement,
    RouterSolicitation,
};
use packet_formats::icmp::{IcmpMessage, IcmpPacketBuilder, IcmpZeroCode};
use packet_formats::ip::Ipv6Proto;
use packet_formats::ipv6::Ipv6PacketBuilder;
use packet_formats::testutil::parse_icmp_packet_in_ip_packet_in_ethernet_frame;
use std::fmt::Debug;

/// As per [RFC 4861] sections 4.1-4.5, NDP packets MUST have a hop limit of 255.
///
/// [RFC 4861]: https://tools.ietf.org/html/rfc4861
pub const MESSAGE_TTL: u8 = 255;

/// Create an NDP message with the provided parameters, including IPv6 and
/// Ethernet headers.
pub fn create_message<M: IcmpMessage<net_types::ip::Ipv6, Code = IcmpZeroCode> + Debug>(
    src_mac: net_types::ethernet::Mac,
    dst_mac: net_types::ethernet::Mac,
    src_ip: net_types::ip::Ipv6Addr,
    dst_ip: net_types::ip::Ipv6Addr,
    message: M,
    options: &[NdpOptionBuilder<'_>],
) -> crate::Result<Buf<Vec<u8>>> {
    Ok(OptionSequenceBuilder::new(options.iter())
        .into_serializer()
        .wrap_in(IcmpPacketBuilder::<_, _>::new(src_ip, dst_ip, IcmpZeroCode, message))
        .wrap_in(Ipv6PacketBuilder::new(src_ip, dst_ip, MESSAGE_TTL, Ipv6Proto::Icmpv6))
        .wrap_in(EthernetFrameBuilder::new(
            src_mac,
            dst_mac,
            EtherType::Ipv6,
            ETHERNET_MIN_BODY_LEN_NO_TAG,
        ))
        .serialize_vec_outer()
        .map_err(|e| anyhow::anyhow!("failed to serialize NDP packet: {:?}", e))?
        .unwrap_b())
}

/// Write an NDP message to the provided fake endpoint.
///
/// Given the source and destination MAC and IP addresses, NDP message and
/// options, the full NDP packet (including IPv6 and Ethernet headers) will be
/// transmitted to the fake endpoint's network.
pub async fn write_message<M: IcmpMessage<net_types::ip::Ipv6, Code = IcmpZeroCode> + Debug>(
    src_mac: net_types::ethernet::Mac,
    dst_mac: net_types::ethernet::Mac,
    src_ip: net_types::ip::Ipv6Addr,
    dst_ip: net_types::ip::Ipv6Addr,
    message: M,
    options: &[NdpOptionBuilder<'_>],
    ep: &netemul::TestFakeEndpoint<'_>,
) -> crate::Result {
    let ser = create_message(src_mac, dst_mac, src_ip, dst_ip, message, options)?;
    ep.write(ser.as_ref()).await.context("failed to write to fake endpoint")
}

/// Send Router Advertisement NDP message.
pub async fn send_ra<'a>(
    fake_ep: &netemul::TestFakeEndpoint<'a>,
    ra: RouterAdvertisement,
    options: &[NdpOptionBuilder<'_>],
    src_ip: net_types::ip::Ipv6Addr,
) -> crate::Result {
    write_message(
        constants::eth::MAC_ADDR,
        net_types::ethernet::Mac::from(
            &net_types::ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS,
        ),
        src_ip,
        net_types::ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS.get(),
        ra,
        options,
        fake_ep,
    )
    .await
}

/// Send Router Advertisement NDP message with router lifetime.
pub async fn send_ra_with_router_lifetime<'a>(
    fake_ep: &netemul::TestFakeEndpoint<'a>,
    lifetime: u16,
    options: &[NdpOptionBuilder<'_>],
    src_ip: net_types::ip::Ipv6Addr,
) -> crate::Result {
    let ra = RouterAdvertisement::new(
        0,        /* current_hop_limit */
        false,    /* managed_flag */
        false,    /* other_config_flag */
        lifetime, /* router_lifetime */
        0,        /* reachable_time */
        0,        /* retransmit_timer */
    );
    send_ra(fake_ep, ra, options, src_ip).await
}

/// A result type that can be used to evaluate the outcome of Duplicate Address
/// Detection (DAD).
pub type DadState = Result<
    fidl_fuchsia_net_interfaces::AddressAssignmentState,
    fidl_fuchsia_net_interfaces_ext::admin::AddressStateProviderError,
>;

/// Wait for and verify a NS message transmitted by netstack for DAD. Returns
/// the bytes of the NS message.
pub async fn expect_dad_neighbor_solicitation(fake_ep: &netemul::TestFakeEndpoint<'_>) -> Vec<u8> {
    let ret = fake_ep
        .frame_stream()
        .try_filter_map(|(data, dropped)| {
            assert_eq!(dropped, 0);
            future::ok(
                parse_icmp_packet_in_ip_packet_in_ethernet_frame::<
                    net_types::ip::Ipv6,
                    _,
                    NeighborSolicitation,
                    _,
                >(&data, EthernetFrameLengthCheck::NoCheck, |p| {
                    assert_matches!(
                        &p.body().iter().collect::<Vec<_>>()[..],
                        [NdpOption::Nonce(_)]
                    );
                })
                .map_or(
                    None,
                    |(_src_mac, dst_mac, src_ip, dst_ip, ttl, message, _code)| {
                        // If the NS is not for the address we just added, this is for some
                        // other address. We ignore it as it is not relevant to our test.
                        if message.target_address() != &constants::ipv6::LINK_LOCAL_ADDR {
                            return None;
                        }

                        Some((dst_mac, src_ip, dst_ip, ttl, data))
                    },
                ),
            )
        })
        .try_next()
        .map(|r| r.context("error getting OnData event"))
        .on_timeout(crate::ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT.after_now(), || {
            Err(anyhow::anyhow!(
                "timed out waiting for a neighbor solicitation targetting {}",
                constants::ipv6::LINK_LOCAL_ADDR
            ))
        })
        .await
        .unwrap()
        .expect("failed to get next OnData event");

    let (dst_mac, src_ip, dst_ip, ttl, data) = ret;
    let expected_dst = constants::ipv6::LINK_LOCAL_ADDR.to_solicited_node_address();
    assert_eq!(src_ip, net_types::ip::Ipv6::UNSPECIFIED_ADDRESS);
    assert_eq!(dst_ip, expected_dst.get());
    assert_eq!(dst_mac, net_types::ethernet::Mac::from(&expected_dst));
    assert_eq!(ttl, MESSAGE_TTL);

    data
}

/// Transmit a Neighbor Solicitation message simulating that a node is
/// performing DAD for `constants::ipv6::LINK_LOCAL_ADDR`.
pub async fn fail_dad_with_ns(fake_ep: &netemul::TestFakeEndpoint<'_>) {
    let snmc = constants::ipv6::LINK_LOCAL_ADDR.to_solicited_node_address();
    write_message(
        constants::eth::MAC_ADDR,
        net_types::ethernet::Mac::from(&snmc),
        net_types::ip::Ipv6::UNSPECIFIED_ADDRESS,
        snmc.get(),
        NeighborSolicitation::new(constants::ipv6::LINK_LOCAL_ADDR),
        &[],
        fake_ep,
    )
    .await
    .expect("failed to write NDP message");
}

/// Transmit a Neighbor Advertisement message simulating that a node owns
/// `constants::ipv6::LINK_LOCAL_ADDR`.
pub async fn fail_dad_with_na(fake_ep: &netemul::TestFakeEndpoint<'_>) {
    write_message(
        constants::eth::MAC_ADDR,
        net_types::ethernet::Mac::from(
            &net_types::ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS,
        ),
        constants::ipv6::LINK_LOCAL_ADDR,
        net_types::ip::Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS.get(),
        NeighborAdvertisement::new(
            false, /* router_flag */
            false, /* solicited_flag */
            false, /* override_flag */
            constants::ipv6::LINK_LOCAL_ADDR,
        ),
        &[NdpOptionBuilder::TargetLinkLayerAddress(&constants::eth::MAC_ADDR.bytes())],
        fake_ep,
    )
    .await
    .expect("failed to write NDP message");
}

async fn dad_state(
    state_stream: &mut (impl Stream<Item = DadState> + std::marker::Unpin),
) -> DadState {
    // The address state provider doesn't buffer events, so we might see the tentative state,
    // but we might not.
    let state = match state_stream.by_ref().next().await.expect("state stream not ended") {
        Ok(fidl_fuchsia_net_interfaces::AddressAssignmentState::Tentative) => {
            state_stream.by_ref().next().await.expect("state stream not ended")
        }
        state => state,
    };
    // Ensure errors are terminal.
    match state {
        Ok(_) => {}
        Err(_) => {
            assert_matches::assert_matches!(state_stream.by_ref().next().await, None)
        }
    }
    state
}

/// Assert that the address state provider event stream yields an address
/// removal error, indicating that DAD failed.
pub async fn assert_dad_failed(
    mut state_stream: (impl Stream<Item = DadState> + std::marker::Unpin),
) {
    assert_matches::assert_matches!(
        dad_state(&mut state_stream).await,
        Err(fidl_fuchsia_net_interfaces_ext::admin::AddressStateProviderError::AddressRemoved(
            fidl_fuchsia_net_interfaces_admin::AddressRemovalReason::DadFailed
        ))
    );
}

/// Assert that the address state provider event stream yields an address
/// assignment event, implying that DAD succeeded.
pub async fn assert_dad_success(
    state_stream: &mut (impl Stream<Item = DadState> + std::marker::Unpin),
) {
    assert_matches::assert_matches!(
        dad_state(state_stream).await,
        Ok(fidl_fuchsia_net_interfaces::AddressAssignmentState::Assigned)
    );
}

/// Wait for a router solicitation message.
pub async fn wait_for_router_solicitation(fake_ep: &netemul::TestFakeEndpoint<'_>) {
    let () = fake_ep
        .frame_stream()
        .try_filter_map(|(data, dropped)| {
            assert_eq!(dropped, 0);
            future::ok(
                parse_icmp_packet_in_ip_packet_in_ethernet_frame::<
                    net_types::ip::Ipv6,
                    _,
                    RouterSolicitation,
                    _,
                >(&data, EthernetFrameLengthCheck::NoCheck, |_| {})
                .map_or(None, |_| Some(())),
            )
        })
        .try_next()
        .map(|r| r.context("error getting OnData event"))
        .on_timeout(crate::ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT.after_now(), || {
            Err(anyhow::anyhow!("timed out waiting for RS packet"))
        })
        .await
        .unwrap()
        .expect("failed to get next OnData event");
}

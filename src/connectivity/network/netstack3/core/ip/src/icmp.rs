// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Internet Control Message Protocol (ICMP).

use alloc::boxed::Box;
use core::convert::TryInto as _;
use core::num::{NonZeroU16, NonZeroU8};

use lock_order::lock::{OrderedLockAccess, OrderedLockRef};
use log::{debug, error, trace};
use net_types::ip::{
    GenericOverIp, Ip, IpAddress, IpMarked, Ipv4, Ipv4Addr, Ipv4SourceAddr, Ipv6, Ipv6Addr,
    Ipv6SourceAddr, Mtu, SubnetError,
};
use net_types::{
    LinkLocalAddress, LinkLocalUnicastAddr, MulticastAddress, NonMulticastAddr, SpecifiedAddr,
    UnicastAddr, Witness,
};
use netstack3_base::socket::{AddrIsMappedError, SocketIpAddr, SocketIpAddrExt as _};
use netstack3_base::sync::Mutex;
use netstack3_base::{
    AnyDevice, Counter, CounterContext, DeviceIdContext, EitherDeviceId, FrameDestination,
    IcmpIpExt, Icmpv4ErrorCode, Icmpv6ErrorCode, InstantBindingsTypes, InstantContext,
    IpDeviceAddr, IpExt, Marks, RngContext, TokenBucket, TxMetadataBindingsTypes,
};
use netstack3_filter::{FilterIpExt, TransportPacketSerializer};
use packet::{
    BufferMut, InnerPacketBuilder as _, PacketBuilder as _, ParsablePacket as _, ParseBuffer,
    PartialSerializer, Serializer, TruncateDirection, TruncatingSerializer,
};
use packet_formats::icmp::ndp::options::{NdpOption, NdpOptionBuilder};
use packet_formats::icmp::ndp::{
    NdpPacket, NeighborAdvertisement, NeighborSolicitation, NonZeroNdpLifetime,
    OptionSequenceBuilder, RouterSolicitation,
};
use packet_formats::icmp::{
    peek_message_type, IcmpDestUnreachable, IcmpEchoRequest, IcmpMessage, IcmpMessageType,
    IcmpPacket, IcmpPacketBuilder, IcmpPacketRaw, IcmpParseArgs, IcmpTimeExceeded, IcmpZeroCode,
    Icmpv4DestUnreachableCode, Icmpv4Packet, Icmpv4ParameterProblem, Icmpv4ParameterProblemCode,
    Icmpv4TimeExceededCode, Icmpv6DestUnreachableCode, Icmpv6Packet, Icmpv6PacketTooBig,
    Icmpv6ParameterProblem, Icmpv6ParameterProblemCode, Icmpv6TimeExceededCode, MessageBody,
    OriginalPacket,
};
use packet_formats::ip::{DscpAndEcn, IpPacket, Ipv4Proto, Ipv6Proto};
use packet_formats::ipv4::{Ipv4FragmentType, Ipv4Header, Ipv4OnlyMeta};
use packet_formats::ipv6::{ExtHdrParseError, Ipv6Header};
use zerocopy::SplitByteSlice;

use crate::internal::base::{
    AddressStatus, IpDeviceIngressStateContext, IpLayerHandler, IpPacketDestination,
    IpSendFrameError, IpTransportContext, Ipv6PresentAddressStatus, NdpBindingsContext,
    RouterAdvertisementEvent, SendIpPacketMeta, TransportReceiveError, IPV6_DEFAULT_SUBNET,
};
use crate::internal::device::nud::{ConfirmationFlags, NudIpHandler};
use crate::internal::device::route_discovery::Ipv6DiscoveredRoute;
use crate::internal::device::{
    IpAddressState, IpDeviceHandler, Ipv6DeviceHandler, Ipv6LinkLayerAddr,
};
use crate::internal::local_delivery::{IpHeaderInfo, LocalDeliveryPacketInfo, ReceiveIpPacketMeta};
use crate::internal::path_mtu::PmtuHandler;
use crate::internal::socket::{
    DelegatedRouteResolutionOptions, DelegatedSendOptions, IpSocketHandler, OptionDelegationMarker,
    RouteResolutionOptions, SendOptions,
};

/// The IP packet hop limit for all NDP packets.
///
/// See [RFC 4861 section 4.1], [RFC 4861 section 4.2], [RFC 4861 section 4.2],
/// [RFC 4861 section 4.3], [RFC 4861 section 4.4], and [RFC 4861 section 4.5]
/// for more information.
///
/// [RFC 4861 section 4.1]: https://tools.ietf.org/html/rfc4861#section-4.1
/// [RFC 4861 section 4.2]: https://tools.ietf.org/html/rfc4861#section-4.2
/// [RFC 4861 section 4.3]: https://tools.ietf.org/html/rfc4861#section-4.3
/// [RFC 4861 section 4.4]: https://tools.ietf.org/html/rfc4861#section-4.4
/// [RFC 4861 section 4.5]: https://tools.ietf.org/html/rfc4861#section-4.5
pub const REQUIRED_NDP_IP_PACKET_HOP_LIMIT: u8 = 255;

/// The default number of ICMP error messages to send per second.
///
/// Beyond this rate, error messages will be silently dropped.
///
/// The current value (1000) was inspired by Netstack2 (gVisor).
// TODO(https://fxbug.dev/407541323): Consider tuning the ICMP rate limiting
// behavior to conform more closely to Linux.
pub const DEFAULT_ERRORS_PER_SECOND: u64 = 1000;
/// The IP layer's ICMP state.
#[derive(GenericOverIp)]
#[generic_over_ip(I, Ip)]
pub struct IcmpState<I: IpExt, BT: IcmpBindingsTypes> {
    error_send_bucket: Mutex<IpMarked<I, TokenBucket<BT::Instant>>>,
    /// ICMP transmit counters.
    pub tx_counters: IcmpTxCounters<I>,
    /// ICMP receive counters.
    pub rx_counters: IcmpRxCounters<I>,
}

impl<I, BT> OrderedLockAccess<IpMarked<I, TokenBucket<BT::Instant>>> for IcmpState<I, BT>
where
    I: IpExt,
    BT: IcmpBindingsTypes,
{
    type Lock = Mutex<IpMarked<I, TokenBucket<BT::Instant>>>;
    fn ordered_lock_access(&self) -> OrderedLockRef<'_, Self::Lock> {
        OrderedLockRef::new(&self.error_send_bucket)
    }
}

/// ICMP tx path counters.
pub type IcmpTxCounters<I> = IpMarked<I, IcmpTxCountersInner>;

/// ICMP tx path counters.
#[derive(Default)]
pub struct IcmpTxCountersInner {
    /// Count of reply messages sent.
    pub reply: Counter,
    /// Count of protocol unreachable messages sent.
    pub protocol_unreachable: Counter,
    /// Count of host/address unreachable messages sent.
    pub address_unreachable: Counter,
    /// Count of port unreachable messages sent.
    pub port_unreachable: Counter,
    /// Count of net unreachable messages sent.
    pub net_unreachable: Counter,
    /// Count of ttl expired messages sent.
    pub ttl_expired: Counter,
    /// Count of packet too big messages sent.
    pub packet_too_big: Counter,
    /// Count of parameter problem messages sent.
    pub parameter_problem: Counter,
    /// Count of destination unreachable messages sent.
    pub dest_unreachable: Counter,
    /// Count of error messages sent.
    pub error: Counter,
}

/// ICMP rx path counters.
pub type IcmpRxCounters<I> = IpMarked<I, IcmpRxCountersInner>;

/// ICMP rx path counters.
#[derive(Default)]
pub struct IcmpRxCountersInner {
    /// Count of error messages received.
    pub error: Counter,
    /// Count of error messages delivered to the transport layer.
    pub error_delivered_to_transport_layer: Counter,
    /// Count of error messages delivered to a socket.
    pub error_delivered_to_socket: Counter,
    /// Count of echo request messages received.
    pub echo_request: Counter,
    /// Count of echo reply messages received.
    pub echo_reply: Counter,
    /// Count of timestamp request messages received.
    pub timestamp_request: Counter,
    /// Count of destination unreachable messages received.
    pub dest_unreachable: Counter,
    /// Count of time exceeded messages received.
    pub time_exceeded: Counter,
    /// Count of parameter problem messages received.
    pub parameter_problem: Counter,
    /// Count of packet too big messages received.
    pub packet_too_big: Counter,
}

/// Receive NDP counters.
#[derive(Default)]
pub struct NdpRxCounters {
    /// Count of neighbor solicitation messages received.
    pub neighbor_solicitation: Counter,
    /// Count of neighbor advertisement messages received.
    pub neighbor_advertisement: Counter,
    /// Count of router advertisement messages received.
    pub router_advertisement: Counter,
    /// Count of router solicitation messages received.
    pub router_solicitation: Counter,
}

/// Transmit NDP counters.
#[derive(Default)]
pub struct NdpTxCounters {
    /// Count of neighbor advertisement messages sent.
    pub neighbor_advertisement: Counter,
    /// Count of neighbor solicitation messages sent.
    pub neighbor_solicitation: Counter,
}

/// Counters for NDP messages.
#[derive(Default)]
pub struct NdpCounters {
    /// Receive counters.
    pub rx: NdpRxCounters,
    /// Transmit counters.
    pub tx: NdpTxCounters,
}

/// A builder for ICMPv4 state.
#[derive(Copy, Clone)]
pub struct Icmpv4StateBuilder {
    send_timestamp_reply: bool,
    errors_per_second: u64,
}

impl Default for Icmpv4StateBuilder {
    fn default() -> Icmpv4StateBuilder {
        Icmpv4StateBuilder {
            send_timestamp_reply: false,
            errors_per_second: DEFAULT_ERRORS_PER_SECOND,
        }
    }
}

impl Icmpv4StateBuilder {
    /// Enable or disable replying to ICMPv4 Timestamp Request messages with
    /// Timestamp Reply messages (default: disabled).
    ///
    /// Enabling this can introduce a very minor vulnerability in which an
    /// attacker can learn the system clock's time, which in turn can aid in
    /// attacks against time-based authentication systems.
    pub fn send_timestamp_reply(&mut self, send_timestamp_reply: bool) -> &mut Self {
        self.send_timestamp_reply = send_timestamp_reply;
        self
    }

    /// Builds an [`Icmpv4State`].
    pub fn build<BT: IcmpBindingsTypes>(self) -> Icmpv4State<BT> {
        Icmpv4State {
            inner: IcmpState {
                error_send_bucket: Mutex::new(IpMarked::new(TokenBucket::new(
                    self.errors_per_second,
                ))),
                tx_counters: Default::default(),
                rx_counters: Default::default(),
            },
            send_timestamp_reply: self.send_timestamp_reply,
        }
    }
}

/// The state associated with the ICMPv4 protocol.
pub struct Icmpv4State<BT: IcmpBindingsTypes> {
    /// The inner common ICMP state.
    pub inner: IcmpState<Ipv4, BT>,
    /// Whether the stack is configured to send ICMP timestamp replies.
    pub send_timestamp_reply: bool,
}

impl<BT: IcmpBindingsTypes> AsRef<IcmpState<Ipv4, BT>> for Icmpv4State<BT> {
    fn as_ref(&self) -> &IcmpState<Ipv4, BT> {
        &self.inner
    }
}

impl<BT: IcmpBindingsTypes> AsMut<IcmpState<Ipv4, BT>> for Icmpv4State<BT> {
    fn as_mut(&mut self) -> &mut IcmpState<Ipv4, BT> {
        &mut self.inner
    }
}

/// A builder for ICMPv6 state.
#[derive(Copy, Clone)]
pub(crate) struct Icmpv6StateBuilder {
    errors_per_second: u64,
}

impl Default for Icmpv6StateBuilder {
    fn default() -> Icmpv6StateBuilder {
        Icmpv6StateBuilder { errors_per_second: DEFAULT_ERRORS_PER_SECOND }
    }
}

impl Icmpv6StateBuilder {
    pub(crate) fn build<BT: IcmpBindingsTypes>(self) -> Icmpv6State<BT> {
        Icmpv6State {
            inner: IcmpState {
                error_send_bucket: Mutex::new(IpMarked::new(TokenBucket::new(
                    self.errors_per_second,
                ))),
                tx_counters: Default::default(),
                rx_counters: Default::default(),
            },
            ndp_counters: Default::default(),
        }
    }
}

/// The state associated with the ICMPv6 protocol.
pub struct Icmpv6State<BT: IcmpBindingsTypes> {
    /// The inner common ICMP state.
    pub inner: IcmpState<Ipv6, BT>,
    /// Neighbor discovery protocol counters.
    pub ndp_counters: NdpCounters,
}

impl<BT: IcmpBindingsTypes> AsRef<IcmpState<Ipv6, BT>> for Icmpv6State<BT> {
    fn as_ref(&self) -> &IcmpState<Ipv6, BT> {
        &self.inner
    }
}

impl<BT: IcmpBindingsTypes> AsMut<IcmpState<Ipv6, BT>> for Icmpv6State<BT> {
    fn as_mut(&mut self) -> &mut IcmpState<Ipv6, BT> {
        &mut self.inner
    }
}

/// An extension trait providing ICMP handler properties.
pub trait IcmpHandlerIpExt: IpExt {
    type SourceAddress: Witness<Self::Addr>;
    type IcmpError;

    /// A try-conversion from [`Self::RecvSrcAddr`] to [`Self::SourceAddress`].
    fn received_source_as_icmp_source(src: Self::RecvSrcAddr) -> Option<Self::SourceAddress>;

    /// An IP-specific constructor for TtlExpired ICMP errors.
    fn new_ttl_expired<B: SplitByteSlice>(
        proto: Self::Proto,
        header_len: usize,
        meta: <Self::Packet<B> as IpPacket<B, Self>>::VersionSpecificMeta,
    ) -> Self::IcmpError;

    /// An IP-specific optional-constructor for MTU Exceeded ICMP errors.
    fn new_mtu_exceeded(proto: Self::Proto, header_len: usize, mtu: Mtu)
        -> Option<Self::IcmpError>;
}

impl IcmpHandlerIpExt for Ipv4 {
    type SourceAddress = NonMulticastAddr<SpecifiedAddr<Ipv4Addr>>;
    type IcmpError = Icmpv4Error;
    fn received_source_as_icmp_source(
        src: Ipv4SourceAddr,
    ) -> Option<NonMulticastAddr<SpecifiedAddr<Ipv4Addr>>> {
        match src {
            Ipv4SourceAddr::Specified(src) => Some(src),
            Ipv4SourceAddr::Unspecified => None,
        }
    }
    fn new_ttl_expired<B: SplitByteSlice>(
        proto: Ipv4Proto,
        header_len: usize,
        Ipv4OnlyMeta { id: _, fragment_type }: Ipv4OnlyMeta,
    ) -> Icmpv4Error {
        Icmpv4Error { kind: Icmpv4ErrorKind::TtlExpired { proto, fragment_type }, header_len }
    }
    fn new_mtu_exceeded(_proto: Ipv4Proto, _header_len: usize, _mtu: Mtu) -> Option<Icmpv4Error> {
        // NB: ICMPv4 has no representation of MTU exceeded errors.
        None
    }
}

impl IcmpHandlerIpExt for Ipv6 {
    type SourceAddress = UnicastAddr<Ipv6Addr>;
    type IcmpError = Icmpv6ErrorKind;
    fn received_source_as_icmp_source(src: Ipv6SourceAddr) -> Option<UnicastAddr<Ipv6Addr>> {
        match src {
            Ipv6SourceAddr::Unicast(src) => Some(src.get()),
            Ipv6SourceAddr::Unspecified => None,
        }
    }
    fn new_ttl_expired<B: SplitByteSlice>(
        proto: Ipv6Proto,
        header_len: usize,
        _meta: (),
    ) -> Icmpv6ErrorKind {
        Icmpv6ErrorKind::TtlExpired { proto, header_len }
    }
    fn new_mtu_exceeded(proto: Ipv6Proto, header_len: usize, mtu: Mtu) -> Option<Icmpv6ErrorKind> {
        Some(Icmpv6ErrorKind::PacketTooBig { proto, header_len, mtu })
    }
}

/// A kind of ICMPv4 error.
pub(crate) enum Icmpv4ErrorKind {
    ParameterProblem {
        code: Icmpv4ParameterProblemCode,
        pointer: u8,
        fragment_type: Ipv4FragmentType,
    },
    TtlExpired {
        proto: Ipv4Proto,
        fragment_type: Ipv4FragmentType,
    },
    NetUnreachable {
        proto: Ipv4Proto,
        fragment_type: Ipv4FragmentType,
    },
    ProtocolUnreachable,
    PortUnreachable,
}

/// An ICMPv4 error.
pub struct Icmpv4Error {
    pub(super) kind: Icmpv4ErrorKind,
    pub(super) header_len: usize,
}

/// A kind of ICMPv6 error.
pub enum Icmpv6ErrorKind {
    ParameterProblem { code: Icmpv6ParameterProblemCode, pointer: u32, allow_dst_multicast: bool },
    TtlExpired { proto: Ipv6Proto, header_len: usize },
    NetUnreachable { proto: Ipv6Proto, header_len: usize },
    PacketTooBig { proto: Ipv6Proto, header_len: usize, mtu: Mtu },
    ProtocolUnreachable { header_len: usize },
    PortUnreachable,
}

/// The handler exposed by ICMP.
pub trait IcmpErrorHandler<I: IcmpHandlerIpExt, BC>: DeviceIdContext<AnyDevice> {
    /// Sends an error message in response to an incoming packet.
    ///
    /// `src_ip` and `dst_ip` are the source and destination addresses of the
    /// incoming packet.
    fn send_icmp_error_message<B: BufferMut>(
        &mut self,
        bindings_ctx: &mut BC,
        device: &Self::DeviceId,
        frame_dst: Option<FrameDestination>,
        src_ip: I::SourceAddress,
        dst_ip: SpecifiedAddr<I::Addr>,
        original_packet: B,
        error: I::IcmpError,
        marks: &Marks,
    );
}

impl<
        BC: IcmpBindingsContext,
        CC: InnerIcmpv4Context<BC> + CounterContext<IcmpTxCounters<Ipv4>>,
    > IcmpErrorHandler<Ipv4, BC> for CC
{
    fn send_icmp_error_message<B: BufferMut>(
        &mut self,
        bindings_ctx: &mut BC,
        device: &CC::DeviceId,
        frame_dst: Option<FrameDestination>,
        src_ip: NonMulticastAddr<SpecifiedAddr<Ipv4Addr>>,
        dst_ip: SpecifiedAddr<Ipv4Addr>,
        original_packet: B,
        Icmpv4Error { kind, header_len }: Icmpv4Error,
        marks: &Marks,
    ) {
        let src_ip = SocketIpAddr::new_ipv4_specified(src_ip.get());
        let dst_ip = SocketIpAddr::new_ipv4_specified(dst_ip);
        match kind {
            Icmpv4ErrorKind::ParameterProblem { code, pointer, fragment_type } => {
                send_icmpv4_parameter_problem(
                    self,
                    bindings_ctx,
                    device,
                    frame_dst,
                    src_ip,
                    dst_ip,
                    code,
                    Icmpv4ParameterProblem::new(pointer),
                    original_packet,
                    header_len,
                    fragment_type,
                    marks,
                )
            }
            Icmpv4ErrorKind::TtlExpired { proto, fragment_type } => send_icmpv4_ttl_expired(
                self,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                proto,
                original_packet,
                header_len,
                fragment_type,
                marks,
            ),
            Icmpv4ErrorKind::NetUnreachable { proto, fragment_type } => {
                send_icmpv4_net_unreachable(
                    self,
                    bindings_ctx,
                    device,
                    frame_dst,
                    src_ip,
                    dst_ip,
                    proto,
                    original_packet,
                    header_len,
                    fragment_type,
                    marks,
                )
            }
            Icmpv4ErrorKind::ProtocolUnreachable => send_icmpv4_protocol_unreachable(
                self,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                original_packet,
                header_len,
                marks,
            ),
            Icmpv4ErrorKind::PortUnreachable => send_icmpv4_port_unreachable(
                self,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                original_packet,
                header_len,
                marks,
            ),
        }
    }
}

impl<
        BC: IcmpBindingsContext,
        CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
    > IcmpErrorHandler<Ipv6, BC> for CC
{
    fn send_icmp_error_message<B: BufferMut>(
        &mut self,
        bindings_ctx: &mut BC,
        device: &CC::DeviceId,
        frame_dst: Option<FrameDestination>,
        src_ip: UnicastAddr<Ipv6Addr>,
        dst_ip: SpecifiedAddr<Ipv6Addr>,
        original_packet: B,
        error: Icmpv6ErrorKind,
        marks: &Marks,
    ) {
        let src_ip: SocketIpAddr<Ipv6Addr> = match src_ip.into_specified().try_into() {
            Ok(addr) => addr,
            Err(AddrIsMappedError {}) => {
                trace!("send_icmpv6_error_message: src_ip is mapped");
                return;
            }
        };
        let dst_ip: SocketIpAddr<Ipv6Addr> = match dst_ip.try_into() {
            Ok(addr) => addr,
            Err(AddrIsMappedError {}) => {
                trace!("send_icmpv6_error_message: dst_ip is mapped");
                return;
            }
        };

        match error {
            Icmpv6ErrorKind::ParameterProblem { code, pointer, allow_dst_multicast } => {
                send_icmpv6_parameter_problem(
                    self,
                    bindings_ctx,
                    device,
                    frame_dst,
                    src_ip,
                    dst_ip,
                    code,
                    Icmpv6ParameterProblem::new(pointer),
                    original_packet,
                    allow_dst_multicast,
                    marks,
                )
            }
            Icmpv6ErrorKind::TtlExpired { proto, header_len } => send_icmpv6_ttl_expired(
                self,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                proto,
                original_packet,
                header_len,
                marks,
            ),
            Icmpv6ErrorKind::NetUnreachable { proto, header_len } => send_icmpv6_net_unreachable(
                self,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                proto,
                original_packet,
                header_len,
                marks,
            ),
            Icmpv6ErrorKind::PacketTooBig { proto, header_len, mtu } => send_icmpv6_packet_too_big(
                self,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                proto,
                mtu,
                original_packet,
                header_len,
                marks,
            ),
            Icmpv6ErrorKind::ProtocolUnreachable { header_len } => {
                send_icmpv6_protocol_unreachable(
                    self,
                    bindings_ctx,
                    device,
                    frame_dst,
                    src_ip,
                    dst_ip,
                    original_packet,
                    header_len,
                    marks,
                )
            }
            Icmpv6ErrorKind::PortUnreachable => send_icmpv6_port_unreachable(
                self,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                original_packet,
                marks,
            ),
        }
    }
}

/// A marker for all the contexts provided by bindings require by the ICMP
/// module.
pub trait IcmpBindingsContext: InstantContext + RngContext + IcmpBindingsTypes {}
impl<BC> IcmpBindingsContext for BC where
    BC: InstantContext + RngContext + IcmpBindingsTypes + IcmpBindingsTypes
{
}

/// A marker trait for all bindings types required by the ICMP module.
pub trait IcmpBindingsTypes: InstantBindingsTypes + TxMetadataBindingsTypes {}
impl<BT: InstantBindingsTypes + TxMetadataBindingsTypes> IcmpBindingsTypes for BT {}

/// Empty trait to work around coherence issues.
///
/// This serves only to convince the coherence checker that a particular blanket
/// trait implementation could only possibly conflict with other blanket impls
/// in this crate. It can be safely implemented for any type.
/// TODO(https://github.com/rust-lang/rust/issues/97811): Remove this once the
/// coherence checker doesn't require it.
pub trait IcmpStateContext {}

/// A marker trait to prevent integration from creating a recursive loop when
/// handling Echo sockets.
///
/// This is a requirement for [`InnerIcmpContext::EchoTransportContext`] which
/// disallows the integration layer from using [`IcmpIpTransportContext`] as the
/// associated type, which would create a recursive loop.
///
/// By *not implementing* this trait for [`IcmpIpTransporContext`] we prevent
/// the mistake.
pub trait EchoTransportContextMarker {}

/// The execution context shared by ICMP(v4) and ICMPv6 for the internal
/// operations of the IP stack.
pub trait InnerIcmpContext<I: IpExt + FilterIpExt, BC: IcmpBindingsTypes>:
    IpSocketHandler<I, BC>
{
    /// A type implementing [`IpTransportContext`] that handles ICMP Echo
    /// replies.
    type EchoTransportContext: IpTransportContext<I, BC, Self> + EchoTransportContextMarker;

    // TODO(joshlf): If we end up needing to respond to these messages with new
    // outbound packets, then perhaps it'd be worth passing the original buffer
    // so that it can be reused?
    //
    // NOTE(joshlf): We don't guarantee the packet body length here for two
    // reasons:
    // - It's possible that some IPv4 protocol does or will exist for which
    //   valid packets are less than 8 bytes in length. If we were to reject all
    //   packets with bodies of less than 8 bytes, we might silently discard
    //   legitimate error messages for such protocols.
    // - Even if we were to guarantee this, there's no good way to encode such a
    //   guarantee in the type system, and so the caller would have no recourse
    //   but to panic, and panics have a habit of becoming bugs or DoS
    //   vulnerabilities when invariants change.

    /// Receives an ICMP error message and demultiplexes it to a transport layer
    /// protocol.
    ///
    /// All arguments beginning with `original_` are fields from the IP packet
    /// that triggered the error. The `original_body` is provided here so that
    /// the error can be associated with a transport-layer socket. `device`
    /// identifies the device on which the packet was received.
    ///
    /// While ICMPv4 error messages are supposed to contain the first 8 bytes of
    /// the body of the offending packet, and ICMPv6 error messages are supposed
    /// to contain as much of the offending packet as possible without violating
    /// the IPv6 minimum MTU, the caller does NOT guarantee that either of these
    /// hold. It is `receive_icmp_error`'s responsibility to handle any length
    /// of `original_body`, and to perform any necessary validation.
    fn receive_icmp_error(
        &mut self,
        bindings_ctx: &mut BC,
        device: &Self::DeviceId,
        original_src_ip: Option<SpecifiedAddr<I::Addr>>,
        original_dst_ip: SpecifiedAddr<I::Addr>,
        original_proto: I::Proto,
        original_body: &[u8],
        err: I::ErrorCode,
    );

    /// Calls the function with a mutable reference to ICMP error send tocket
    /// bucket.
    fn with_error_send_bucket_mut<O, F: FnOnce(&mut TokenBucket<BC::Instant>) -> O>(
        &mut self,
        cb: F,
    ) -> O;
}

/// The execution context for ICMPv4.
///
/// `InnerIcmpv4Context` is a shorthand for a larger collection of traits.
pub trait InnerIcmpv4Context<BC: IcmpBindingsTypes>: InnerIcmpContext<Ipv4, BC> {
    /// Returns true if a timestamp reply may be sent.
    fn should_send_timestamp_reply(&self) -> bool;
}

/// The execution context for ICMPv6.
///
/// `InnerIcmpv6Context` is a shorthand for a larger collection of traits.
pub trait InnerIcmpv6Context<BC: IcmpBindingsTypes>: InnerIcmpContext<Ipv6, BC> {}
impl<BC: IcmpBindingsTypes, CC: InnerIcmpContext<Ipv6, BC>> InnerIcmpv6Context<BC> for CC {}

/// Attempt to send an ICMP or ICMPv6 error message, applying a rate limit.
///
/// `try_send_error!($core_ctx, $bindings_ctx, $e)` attempts to consume a token from the
/// token bucket at `$core_ctx.get_state_mut().error_send_bucket`. If it
/// succeeds, it invokes the expression `$e`, and otherwise does nothing. It
/// assumes that the type of `$e` is `Result<(), _>` and, in the case that the
/// rate limit is exceeded and it does not invoke `$e`, returns `Ok(())`.
///
/// [RFC 4443 Section 2.4] (f) requires that we MUST limit the rate of outbound
/// ICMPv6 error messages. To our knowledge, there is no similar requirement for
/// ICMPv4, but the same rationale applies, so we do it for ICMPv4 as well.
///
/// [RFC 4443 Section 2.4]: https://tools.ietf.org/html/rfc4443#section-2.4
macro_rules! try_send_error {
    ($core_ctx:expr, $bindings_ctx:expr, $e:expr) => {{
        let send = $core_ctx.with_error_send_bucket_mut(|error_send_bucket| {
            error_send_bucket.try_take($bindings_ctx)
        });

        if send {
            $core_ctx.counters().error.increment();
            $e
        } else {
            trace!("ip::icmp::try_send_error!: dropping rate-limited ICMP error message");
            Ok(())
        }
    }};
}

/// An implementation of [`IpTransportContext`] for ICMP.
pub enum IcmpIpTransportContext {}

fn receive_ip_transport_icmp_error<
    I: IpExt + FilterIpExt,
    CC: InnerIcmpContext<I, BC> + CounterContext<IcmpRxCounters<I>>,
    BC: IcmpBindingsContext,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    original_src_ip: Option<SpecifiedAddr<I::Addr>>,
    original_dst_ip: SpecifiedAddr<I::Addr>,
    original_body: &[u8],
    err: I::ErrorCode,
) {
    core_ctx.counters().error_delivered_to_transport_layer.increment();
    trace!("IcmpIpTransportContext::receive_icmp_error({:?})", err);

    let mut parse_body = original_body;
    match parse_body.parse::<IcmpPacketRaw<I, _, IcmpEchoRequest>>() {
        // Only pass things along to the Echo socket layer if this is an echo
        // request.
        Ok(_echo_request) => (),
        Err(_) => {
            // NOTE: This might just mean that the error message was in response
            // to a packet that we sent that wasn't an echo request, so we just
            // silently ignore it.
            return;
        }
    }

    <CC::EchoTransportContext as IpTransportContext<I, BC, CC>>::receive_icmp_error(
        core_ctx,
        bindings_ctx,
        device,
        original_src_ip,
        original_dst_ip,
        original_body,
        err,
    );
}

impl<
        BC: IcmpBindingsContext,
        CC: InnerIcmpv4Context<BC>
            + PmtuHandler<Ipv4, BC>
            + CounterContext<IcmpRxCounters<Ipv4>>
            + CounterContext<IcmpTxCounters<Ipv4>>,
    > IpTransportContext<Ipv4, BC, CC> for IcmpIpTransportContext
{
    fn receive_icmp_error(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        device: &CC::DeviceId,
        original_src_ip: Option<SpecifiedAddr<Ipv4Addr>>,
        original_dst_ip: SpecifiedAddr<Ipv4Addr>,
        original_body: &[u8],
        err: Icmpv4ErrorCode,
    ) {
        receive_ip_transport_icmp_error(
            core_ctx,
            bindings_ctx,
            device,
            original_src_ip,
            original_dst_ip,
            original_body,
            err,
        )
    }

    fn receive_ip_packet<B: BufferMut, H: IpHeaderInfo<Ipv4>>(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        device: &CC::DeviceId,
        src_ip: Ipv4SourceAddr,
        dst_ip: SpecifiedAddr<Ipv4Addr>,
        mut buffer: B,
        info: &LocalDeliveryPacketInfo<Ipv4, H>,
    ) -> Result<(), (B, TransportReceiveError)> {
        let LocalDeliveryPacketInfo { meta, header_info: _, marks } = info;
        let ReceiveIpPacketMeta { broadcast: _, transparent_override } = meta;
        if let Some(delivery) = transparent_override {
            unreachable!(
                "cannot perform transparent local delivery {delivery:?} to an ICMP socket; \
                transparent proxy rules can only be configured for TCP and UDP packets"
            );
        }

        trace!(
            "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::receive_ip_packet({}, {})",
            src_ip,
            dst_ip
        );
        let packet =
            match buffer.parse_with::<_, Icmpv4Packet<_>>(IcmpParseArgs::new(src_ip, dst_ip)) {
                Ok(packet) => packet,
                Err(_) => return Ok(()), // TODO(joshlf): Do something else here?
            };

        match packet {
            Icmpv4Packet::EchoRequest(echo_request) => {
                CounterContext::<IcmpRxCounters<Ipv4>>::counters(core_ctx).echo_request.increment();

                if let Ipv4SourceAddr::Specified(src_ip) = src_ip {
                    let req = *echo_request.message();
                    let code = echo_request.code();
                    let (local_ip, remote_ip) = (dst_ip, src_ip);
                    debug!(
                        "replying to ICMP echo request from {remote_ip} to {local_ip}%{device:?}: \
                        id={}, seq={}",
                        req.id(),
                        req.seq()
                    );
                    send_icmp_reply(
                        core_ctx,
                        bindings_ctx,
                        device,
                        SocketIpAddr::new_ipv4_specified(remote_ip.get()),
                        SocketIpAddr::new_ipv4_specified(local_ip),
                        |src_ip| {
                            IcmpPacketBuilder::<Ipv4, _>::new(src_ip, *remote_ip, code, req.reply())
                                .wrap_body(buffer)
                        },
                        &WithMarks(marks),
                    );
                } else {
                    trace!(
                        "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::receive_ip_packet: \
                        Received echo request with an unspecified source address"
                    );
                }
            }
            Icmpv4Packet::EchoReply(echo_reply) => {
                CounterContext::<IcmpRxCounters<Ipv4>>::counters(core_ctx).echo_reply.increment();
                trace!(
                    "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::receive_ip_packet: \
                    Received an EchoReply message"
                );
                let parse_metadata = echo_reply.parse_metadata();
                buffer.undo_parse(parse_metadata);
                return <CC::EchoTransportContext
                            as IpTransportContext<Ipv4, BC, CC>>::receive_ip_packet(
                        core_ctx,
                        bindings_ctx,
                        device,
                        src_ip,
                        dst_ip,
                        buffer,
                        info,
                );
            }
            Icmpv4Packet::TimestampRequest(timestamp_request) => {
                CounterContext::<IcmpRxCounters<Ipv4>>::counters(core_ctx)
                    .timestamp_request
                    .increment();
                if let Ipv4SourceAddr::Specified(src_ip) = src_ip {
                    if core_ctx.should_send_timestamp_reply() {
                        trace!(
                            "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::\
                            receive_ip_packet: Responding to Timestamp Request message"
                        );
                        // We're supposed to respond with the time that we
                        // processed this message as measured in milliseconds
                        // since midnight UT. However, that would require that
                        // we knew the local time zone and had a way to convert
                        // `InstantContext::Instant` to a `u32` value. We can't
                        // do that, and probably don't want to introduce all of
                        // the machinery necessary just to support this one use
                        // case. Luckily, RFC 792 page 17 provides us with an
                        // out:
                        //
                        //   If the time is not available in miliseconds [sic]
                        //   or cannot be provided with respect to midnight UT
                        //   then any time can be inserted in a timestamp
                        //   provided the high order bit of the timestamp is
                        //   also set to indicate this non-standard value.
                        //
                        // Thus, we provide a zero timestamp with the high order
                        // bit set.
                        const NOW: u32 = 0x80000000;
                        let reply = timestamp_request.message().reply(NOW, NOW);
                        let (local_ip, remote_ip) = (dst_ip, src_ip);
                        // We don't actually want to use any of the _contents_
                        // of the buffer, but we would like to reuse it as
                        // scratch space. Eventually, `IcmpPacketBuilder` will
                        // implement `InnerPacketBuilder` for messages without
                        // bodies, but until that happens, we need to give it an
                        // empty buffer.
                        buffer.shrink_front_to(0);
                        send_icmp_reply(
                            core_ctx,
                            bindings_ctx,
                            device,
                            SocketIpAddr::new_ipv4_specified(remote_ip.get()),
                            SocketIpAddr::new_ipv4_specified(local_ip),
                            |src_ip| {
                                IcmpPacketBuilder::<Ipv4, _>::new(
                                    src_ip,
                                    *remote_ip,
                                    IcmpZeroCode,
                                    reply,
                                )
                                .wrap_body(buffer)
                            },
                            &WithMarks(marks),
                        );
                    } else {
                        trace!(
                            "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::\
                            receive_ip_packet: Silently ignoring Timestamp Request message"
                        );
                    }
                } else {
                    trace!(
                        "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::\
                        receive_ip_packet: Received timestamp request with an unspecified source \
                        address"
                    );
                }
            }
            Icmpv4Packet::TimestampReply(_) => {
                // TODO(joshlf): Support sending Timestamp Requests and
                // receiving Timestamp Replies?
                debug!(
                    "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::receive_ip_packet: \
                    Received unsolicited Timestamp Reply message"
                );
            }
            Icmpv4Packet::DestUnreachable(dest_unreachable) => {
                CounterContext::<IcmpRxCounters<Ipv4>>::counters(core_ctx)
                    .dest_unreachable
                    .increment();
                trace!(
                    "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::receive_ip_packet: \
                    Received a Destination Unreachable message"
                );

                let error = if dest_unreachable.code()
                    == Icmpv4DestUnreachableCode::FragmentationRequired
                {
                    let mtu = if let Some(next_hop_mtu) = dest_unreachable.message().next_hop_mtu()
                    {
                        // We are updating the path MTU from the destination
                        // address of this `packet` (which is an IP address on
                        // this node) to some remote (identified by the source
                        // address of this `packet`).
                        //
                        // `update_pmtu_if_less` will only update the PMTU if
                        // the Dest Unreachable message's MTU field had a value
                        // that was at least the IPv4 minimum MTU (which is
                        // required by IPv4 RFC 791).
                        core_ctx.update_pmtu_if_less(
                            bindings_ctx,
                            dst_ip.get(),
                            src_ip.get(),
                            Mtu::new(u32::from(next_hop_mtu.get())),
                        )
                    } else {
                        // If the Next-Hop MTU from an incoming ICMP message is
                        // `0`, then we assume the source node of the ICMP
                        // message does not implement RFC 1191 and therefore
                        // does not actually use the Next-Hop MTU field and
                        // still considers it as an unused field.
                        //
                        // In this case, the only information we have is the
                        // size of the original IP packet that was too big (the
                        // original packet header should be included in the ICMP
                        // response). Here we will simply reduce our PMTU
                        // estimate to a value less than the total length of the
                        // original packet. See RFC 1191 Section 5.
                        //
                        // `update_pmtu_next_lower` may return an error, but it
                        // will only happen if no valid lower value exists from
                        // the original packet's length. It is safe to silently
                        // ignore the error when we have no valid lower PMTU
                        // value as the node from `src_ip` would not be IP RFC
                        // compliant and we expect this to be very rare (for
                        // IPv4, the lowest MTU value for a link can be 68
                        // bytes).
                        let (original_packet_buf, inner_body) = dest_unreachable.body().bytes();
                        // Note: ICMP Dest Unreachable messages don't have a variable size body.
                        debug_assert!(inner_body.is_none());
                        if original_packet_buf.len() >= 4 {
                            // We need the first 4 bytes as the total length
                            // field is at bytes 2/3 of the original packet
                            // buffer.
                            let total_len =
                                u16::from_be_bytes(original_packet_buf[2..4].try_into().unwrap());

                            trace!(
                                "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::\
                                receive_ip_packet: Next-Hop MTU is 0 so using the next best PMTU \
                                value from {total_len}"
                            );

                            core_ctx.update_pmtu_next_lower(
                                bindings_ctx,
                                dst_ip.get(),
                                src_ip.get(),
                                Mtu::new(u32::from(total_len)),
                            )
                        } else {
                            // Ok to silently ignore as RFC 792 requires nodes
                            // to send the original IP packet header + 64 bytes
                            // of the original IP packet's body so the node
                            // itself is already violating the RFC.
                            trace!(
                                "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::\
                                receive_ip_packet: Original packet buf is too small to get \
                                original packet len so ignoring"
                            );
                            None
                        }
                    };
                    mtu.and_then(|mtu| {
                        let mtu = u16::try_from(mtu.get()).unwrap_or(u16::MAX);
                        let mtu = NonZeroU16::new(mtu)?;
                        Some(Icmpv4ErrorCode::DestUnreachable(
                            dest_unreachable.code(),
                            IcmpDestUnreachable::new_for_frag_req(mtu),
                        ))
                    })
                } else {
                    Some(Icmpv4ErrorCode::DestUnreachable(
                        dest_unreachable.code(),
                        *dest_unreachable.message(),
                    ))
                };

                if let Some(error) = error {
                    receive_icmpv4_error(core_ctx, bindings_ctx, device, &dest_unreachable, error);
                }
            }
            Icmpv4Packet::TimeExceeded(time_exceeded) => {
                CounterContext::<IcmpRxCounters<Ipv4>>::counters(core_ctx)
                    .time_exceeded
                    .increment();
                trace!(
                    "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::receive_ip_packet: \
                    Received a Time Exceeded message"
                );

                receive_icmpv4_error(
                    core_ctx,
                    bindings_ctx,
                    device,
                    &time_exceeded,
                    Icmpv4ErrorCode::TimeExceeded(time_exceeded.code()),
                );
            }
            // TODO(https://fxbug.dev/323400954): Support ICMP Redirect.
            Icmpv4Packet::Redirect(_) => {
                debug!(
                    "Unimplemented: <IcmpIpTransportContext as IpTransportContext<Ipv4>>::\
                    receive_ip_packet::redirect"
                )
            }
            Icmpv4Packet::ParameterProblem(parameter_problem) => {
                CounterContext::<IcmpRxCounters<Ipv4>>::counters(core_ctx)
                    .parameter_problem
                    .increment();
                trace!(
                    "<IcmpIpTransportContext as IpTransportContext<Ipv4>>::receive_ip_packet: \
                    Received a Parameter Problem message"
                );

                receive_icmpv4_error(
                    core_ctx,
                    bindings_ctx,
                    device,
                    &parameter_problem,
                    Icmpv4ErrorCode::ParameterProblem(parameter_problem.code()),
                );
            }
        }

        Ok(())
    }
}

/// A type to allow implementing the required filtering traits on a concrete
/// subset of message types.
#[allow(missing_docs)]
pub enum NdpMessage {
    NeighborSolicitation {
        message: NeighborSolicitation,
        code: <NeighborSolicitation as IcmpMessage<Ipv6>>::Code,
    },

    RouterSolicitation {
        message: RouterSolicitation,
        code: <RouterSolicitation as IcmpMessage<Ipv6>>::Code,
    },

    NeighborAdvertisement {
        message: NeighborAdvertisement,
        code: <NeighborAdvertisement as IcmpMessage<Ipv6>>::Code,
    },
}

/// Sends an NDP packet from `device_id` with the provided parameters.
pub fn send_ndp_packet<BC, CC, S>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device_id: &CC::DeviceId,
    src_ip: Option<SpecifiedAddr<Ipv6Addr>>,
    dst_ip: SpecifiedAddr<Ipv6Addr>,
    body: S,
    message: NdpMessage,
) -> Result<(), IpSendFrameError<S>>
where
    CC: IpLayerHandler<Ipv6, BC>,
    S: Serializer + PartialSerializer,
    S::Buffer: BufferMut,
{
    macro_rules! send {
        ($message:expr, $code:expr) => {{
            // TODO(https://fxbug.dev/42177356): Send through ICMPv6 send path.
            IpLayerHandler::<Ipv6, _>::send_ip_packet_from_device(
                core_ctx,
                bindings_ctx,
                SendIpPacketMeta {
                    device: device_id,
                    src_ip,
                    dst_ip,
                    destination: IpPacketDestination::from_addr(dst_ip),
                    ttl: NonZeroU8::new(REQUIRED_NDP_IP_PACKET_HOP_LIMIT),
                    proto: Ipv6Proto::Icmpv6,
                    mtu: Mtu::no_limit(),
                    dscp_and_ecn: DscpAndEcn::default(),
                },
                IcmpPacketBuilder::<Ipv6, _>::new(
                    src_ip.map_or(Ipv6::UNSPECIFIED_ADDRESS, |a| a.get()),
                    dst_ip.get(),
                    $code,
                    $message,
                )
                .wrap_body(body),
            )
            .map_err(|s| s.into_inner())
        }};
    }

    match message {
        NdpMessage::NeighborSolicitation { message, code } => send!(message, code),
        NdpMessage::RouterSolicitation { message, code } => send!(message, code),
        NdpMessage::NeighborAdvertisement { message, code } => send!(message, code),
    }
}

fn send_neighbor_advertisement<
    BC,
    CC: Ipv6DeviceHandler<BC>
        + IpDeviceHandler<Ipv6, BC>
        + IpLayerHandler<Ipv6, BC>
        + CounterContext<NdpCounters>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device_id: &CC::DeviceId,
    solicited: bool,
    device_addr: UnicastAddr<Ipv6Addr>,
    dst_ip: SpecifiedAddr<Ipv6Addr>,
) {
    core_ctx.counters().tx.neighbor_advertisement.increment();
    debug!("send_neighbor_advertisement from {:?} to {:?}", device_addr, dst_ip);
    // We currently only allow the destination address to be:
    // 1) a unicast address.
    // 2) a multicast destination but the message should be an unsolicited
    //    neighbor advertisement.
    // NOTE: this assertion may need change if more messages are to be allowed
    // in the future.
    debug_assert!(dst_ip.is_valid_unicast() || (!solicited && dst_ip.is_multicast()));

    // We must call into the higher level send_ip_packet_from_device function
    // because it is not guaranteed that we actually know the link-layer
    // address of the destination IP. Typically, the solicitation request will
    // carry that information, but it is not necessary. So it is perfectly valid
    // that trying to send this advertisement will end up triggering a neighbor
    // solicitation to be sent.
    let src_ll = core_ctx.get_link_layer_addr(&device_id);

    // Nothing reasonable to do with the error.
    let advertisement = NeighborAdvertisement::new(
        core_ctx.is_router_device(&device_id),
        solicited,
        false,
        device_addr.get(),
    );
    let _: Result<(), _> = send_ndp_packet(
        core_ctx,
        bindings_ctx,
        &device_id,
        Some(device_addr.into_specified()),
        dst_ip,
        OptionSequenceBuilder::new(
            src_ll
                .as_ref()
                .map(Ipv6LinkLayerAddr::as_bytes)
                .map(NdpOptionBuilder::TargetLinkLayerAddress)
                .iter(),
        )
        .into_serializer(),
        NdpMessage::NeighborAdvertisement { message: advertisement, code: IcmpZeroCode },
    );
}

fn receive_ndp_packet<
    B: SplitByteSlice,
    BC: IcmpBindingsContext + NdpBindingsContext<CC::DeviceId>,
    CC: InnerIcmpv6Context<BC>
        + Ipv6DeviceHandler<BC>
        + IpDeviceHandler<Ipv6, BC>
        + IpDeviceIngressStateContext<Ipv6>
        + NudIpHandler<Ipv6, BC>
        + IpLayerHandler<Ipv6, BC>
        + CounterContext<NdpCounters>,
    H: IpHeaderInfo<Ipv6>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device_id: &CC::DeviceId,
    src_ip: Ipv6SourceAddr,
    packet: NdpPacket<B>,
    header_info: &H,
) {
    // All NDP messages should be dropped if the hop-limit != 255. See
    //   Router Solicitations: RFC 4861 section 6.1.1,
    //   Router Advertisements: RFC 4861 section 6.1.2,
    //   Neighbor Solicitations: RFC 4861 section 7.1.1,
    //   Neighbor Advertisements: RFC 4861 section 7.1.2, and
    //   Redirect: RFC 4861 section 8.1:
    //
    //       A node MUST silently discard any received [NDP Message Type]
    //       messages that do not satisfy all of the following validity
    //       checks:
    //
    //          ...
    //
    //          - The IP Hop Limit field has a value of 255, i.e., the packet
    //            could not possibly have been forwarded by a router.
    //
    //          ...
    if header_info.hop_limit() != REQUIRED_NDP_IP_PACKET_HOP_LIMIT {
        trace!("dropping NDP packet from {src_ip} with invalid hop limit");
        return;
    }

    match packet {
        NdpPacket::RouterSolicitation(_) | NdpPacket::Redirect(_) => {}
        NdpPacket::NeighborSolicitation(ref p) => {
            let target_address = p.message().target_address();
            let target_address = match UnicastAddr::new(*target_address) {
                Some(a) => a,
                None => {
                    trace!(
                        "dropping NS from {} with non-unicast target={:?}",
                        src_ip,
                        target_address
                    );
                    return;
                }
            };

            core_ctx.counters().rx.neighbor_solicitation.increment();

            match src_ip {
                Ipv6SourceAddr::Unspecified => {
                    // The neighbor is performing Duplicate address detection.
                    //
                    // As per RFC 4861 section 4.3,
                    //
                    //   Source Address
                    //       Either an address assigned to the interface from
                    //       which this message is sent or (if Duplicate Address
                    //       Detection is in progress [ADDRCONF]) the
                    //       unspecified address.
                    match IpDeviceHandler::handle_received_dad_packet(
                        core_ctx,
                        bindings_ctx,
                        &device_id,
                        target_address.into_specified(),
                        p.body().iter().find_map(|option| option.nonce()),
                    ) {
                        IpAddressState::Assigned => {
                            // Address is assigned to us to we let the
                            // remote node performing DAD that we own the
                            // address.
                            send_neighbor_advertisement(
                                core_ctx,
                                bindings_ctx,
                                &device_id,
                                false,
                                target_address,
                                Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS.into_specified(),
                            );
                        }
                        IpAddressState::Tentative => {
                            // Nothing further to do in response to DAD
                            // messages.
                        }
                        IpAddressState::Unavailable => {
                            // Nothing further to do for unassigned target
                            // addresses.
                        }
                    }

                    return;
                }
                Ipv6SourceAddr::Unicast(src_ip) => {
                    // Neighbor is performing link address resolution.
                    match core_ctx
                        .address_status_for_device(target_address.into_specified(), device_id)
                    {
                        AddressStatus::Present(Ipv6PresentAddressStatus::UnicastAssigned) => {}
                        AddressStatus::Present(
                            Ipv6PresentAddressStatus::UnicastTentative
                            | Ipv6PresentAddressStatus::Multicast,
                        )
                        | AddressStatus::Unassigned => {
                            // Address is not considered assigned to us as a
                            // unicast so don't send a neighbor advertisement
                            // reply.
                            return;
                        }
                    }

                    let link_addr = p.body().iter().find_map(|o| o.source_link_layer_address());

                    if let Some(link_addr) = link_addr {
                        NudIpHandler::handle_neighbor_probe(
                            core_ctx,
                            bindings_ctx,
                            &device_id,
                            src_ip.into_specified(),
                            link_addr,
                        );
                    }

                    send_neighbor_advertisement(
                        core_ctx,
                        bindings_ctx,
                        &device_id,
                        true,
                        target_address,
                        src_ip.into_specified(),
                    );
                }
            }
        }
        NdpPacket::NeighborAdvertisement(ref p) => {
            // TODO(https://fxbug.dev/42179526): Invalidate discovered routers when
            // neighbor entry's IsRouter field transitions to false.

            let target_address = p.message().target_address();

            let src_ip = match src_ip {
                Ipv6SourceAddr::Unicast(src_ip) => src_ip,
                Ipv6SourceAddr::Unspecified => {
                    trace!("dropping NA with unspecified source and target = {:?}", target_address);
                    return;
                }
            };

            let target_address = match UnicastAddr::new(*target_address) {
                Some(a) => a,
                None => {
                    trace!(
                        "dropping NA from {} with non-unicast target={:?}",
                        src_ip,
                        target_address
                    );
                    return;
                }
            };

            core_ctx.counters().rx.neighbor_advertisement.increment();

            // Note: Neighbor Advertisements don't carry a nonce value. Handle
            // a NA in the same way that we would handle an NS that omitted the
            // nonce (i.e. conclude it's not-looped-back).
            let nonce = None;
            match IpDeviceHandler::handle_received_dad_packet(
                core_ctx,
                bindings_ctx,
                &device_id,
                target_address.into_specified(),
                nonce,
            ) {
                IpAddressState::Assigned => {
                    // A neighbor is advertising that it owns an address
                    // that we also have assigned. This is out of scope
                    // for DAD.
                    //
                    // As per RFC 4862 section 5.4.4,
                    //
                    //   2.  If the target address matches a unicast address
                    //       assigned to the receiving interface, it would
                    //       possibly indicate that the address is a
                    //       duplicate but it has not been detected by the
                    //       Duplicate Address Detection procedure (recall
                    //       that Duplicate Address Detection is not
                    //       completely reliable). How to handle such a case
                    //       is beyond the scope of this document.
                    //
                    // TODO(https://fxbug.dev/42111744): Signal to bindings
                    // that a duplicate address is detected.
                    error!(
                        "NA from {src_ip} with target address {target_address} that is also \
                        assigned on device {device_id:?}",
                    );
                }
                IpAddressState::Tentative => {
                    // Nothing further to do for an NA from a neighbor that
                    // targets an address we also have assigned.
                    return;
                }
                IpAddressState::Unavailable => {
                    // Address not targeting us so we know its for a neighbor.
                    //
                    // TODO(https://fxbug.dev/42182317): Move NUD to IP.
                }
            }

            let link_addr = p.body().iter().find_map(|o| o.target_link_layer_address());
            let link_addr = match link_addr {
                Some(a) => a,
                None => {
                    trace!(
                        "dropping NA from {} targetting {} with no TLL option",
                        src_ip,
                        target_address
                    );
                    return;
                }
            };

            NudIpHandler::handle_neighbor_confirmation(
                core_ctx,
                bindings_ctx,
                &device_id,
                target_address.into_specified(),
                link_addr,
                ConfirmationFlags {
                    solicited_flag: p.message().solicited_flag(),
                    override_flag: p.message().override_flag(),
                },
            );
        }
        NdpPacket::RouterAdvertisement(ref p) => {
            // As per RFC 4861 section 6.1.2,
            //
            //   A node MUST silently discard any received Router Advertisement
            //   messages that do not satisfy all of the following validity
            //   checks:
            //
            //      - IP Source Address is a link-local address.  Routers must
            //        use their link-local address as the source for Router
            //        Advertisement and Redirect messages so that hosts can
            //        uniquely identify routers.
            //
            //        ...
            let src_ip = match src_ip {
                Ipv6SourceAddr::Unicast(ip) => match LinkLocalUnicastAddr::new(*ip) {
                    Some(ip) => ip,
                    None => return,
                },
                Ipv6SourceAddr::Unspecified => return,
            };

            let ra = p.message();
            debug!("received router advertisement from {:?}: {:?}", src_ip, ra);
            core_ctx.counters().rx.router_advertisement.increment();

            // As per RFC 4861 section 6.3.4,
            //   The RetransTimer variable SHOULD be copied from the Retrans
            //   Timer field, if it is specified.
            //
            // TODO(https://fxbug.dev/42052173): Control whether or not we should
            // update the retransmit timer.
            if let Some(retransmit_timer) = ra.retransmit_timer() {
                Ipv6DeviceHandler::set_discovered_retrans_timer(
                    core_ctx,
                    bindings_ctx,
                    &device_id,
                    retransmit_timer,
                );
            }

            // As per RFC 4861 section 6.3.4:
            //   If the received Cur Hop Limit value is specified, the host
            //   SHOULD set its CurHopLimit variable to the received value.
            //
            // TODO(https://fxbug.dev/42052173): Control whether or not we should
            // update the default hop limit.
            if let Some(hop_limit) = ra.current_hop_limit() {
                trace!("receive_ndp_packet: NDP RA: updating device's hop limit to {:?} for router: {:?}", ra.current_hop_limit(), src_ip);
                IpDeviceHandler::set_default_hop_limit(core_ctx, &device_id, hop_limit);
            }

            // TODO(https://fxbug.dev/42077316): Support default router preference.
            Ipv6DeviceHandler::update_discovered_ipv6_route(
                core_ctx,
                bindings_ctx,
                &device_id,
                Ipv6DiscoveredRoute { subnet: IPV6_DEFAULT_SUBNET, gateway: Some(src_ip) },
                p.message().router_lifetime().map(NonZeroNdpLifetime::Finite),
            );

            for option in p.body().iter() {
                match option {
                    NdpOption::TargetLinkLayerAddress(_)
                    | NdpOption::RedirectedHeader { .. }
                    | NdpOption::RecursiveDnsServer(_)
                    | NdpOption::Nonce(_) => {}
                    NdpOption::SourceLinkLayerAddress(addr) => {
                        debug!("processing SourceLinkLayerAddress option in RA: {:?}", addr);
                        // As per RFC 4861 section 6.3.4,
                        //
                        //   If the advertisement contains a Source Link-Layer
                        //   Address option, the link-layer address SHOULD be
                        //   recorded in the Neighbor Cache entry for the router
                        //   (creating an entry if necessary) and the IsRouter
                        //   flag in the Neighbor Cache entry MUST be set to
                        //   TRUE. If no Source Link-Layer Address is included,
                        //   but a corresponding Neighbor Cache entry exists,
                        //   its IsRouter flag MUST be set to TRUE. The IsRouter
                        //   flag is used by Neighbor Unreachability Detection
                        //   to determine when a router changes to being a host
                        //   (i.e., no longer capable of forwarding packets).
                        //   If a Neighbor Cache entry is created for the
                        //   router, its reachability state MUST be set to STALE
                        //   as specified in Section 7.3.3.  If a cache entry
                        //   already exists and is updated with a different
                        //   link-layer address, the reachability state MUST
                        //   also be set to STALE.if a Neighbor Cache entry
                        //
                        // We do not yet support NUD as described in RFC 4861
                        // so for now we just record the link-layer address in
                        // our neighbor table.
                        //
                        // TODO(https://fxbug.dev/42083367): Add support for routers in NUD.
                        NudIpHandler::handle_neighbor_probe(
                            core_ctx,
                            bindings_ctx,
                            &device_id,
                            {
                                let src_ip: UnicastAddr<_> = src_ip.into_addr();
                                src_ip.into_specified()
                            },
                            addr,
                        );
                    }
                    NdpOption::PrefixInformation(prefix_info) => {
                        debug!("processing Prefix Information option in RA: {:?}", prefix_info);
                        // As per RFC 4861 section 6.3.4,
                        //
                        //   For each Prefix Information option with the on-link
                        //   flag set, a host does the following:
                        //
                        //      - If the prefix is the link-local prefix,
                        //        silently ignore the Prefix Information option.
                        //
                        // Also as per RFC 4862 section 5.5.3,
                        //
                        //   For each Prefix-Information option in the Router
                        //   Advertisement:
                        //
                        //    ..
                        //
                        //    b)  If the prefix is the link-local prefix,
                        //        silently ignore the Prefix Information option.
                        if prefix_info.prefix().is_link_local() {
                            continue;
                        }

                        let subnet = match prefix_info.subnet() {
                            Ok(subnet) => subnet,
                            Err(err) => match err {
                                SubnetError::PrefixTooLong | SubnetError::HostBitsSet => continue,
                            },
                        };

                        match UnicastAddr::new(subnet.network()) {
                            Some(UnicastAddr { .. }) => {}
                            None => continue,
                        }

                        let valid_lifetime = prefix_info.valid_lifetime();

                        if prefix_info.on_link_flag() {
                            // TODO(https://fxbug.dev/42077316): Support route preference.
                            Ipv6DeviceHandler::update_discovered_ipv6_route(
                                core_ctx,
                                bindings_ctx,
                                &device_id,
                                Ipv6DiscoveredRoute { subnet, gateway: None },
                                valid_lifetime,
                            )
                        }

                        if prefix_info.autonomous_address_configuration_flag() {
                            Ipv6DeviceHandler::apply_slaac_update(
                                core_ctx,
                                bindings_ctx,
                                &device_id,
                                subnet,
                                prefix_info.preferred_lifetime(),
                                valid_lifetime,
                            );
                        }
                    }
                    NdpOption::RouteInformation(rio) => {
                        debug!("processing Route Information option in RA: {:?}", rio);
                        // TODO(https://fxbug.dev/42077316): Support route preference.
                        Ipv6DeviceHandler::update_discovered_ipv6_route(
                            core_ctx,
                            bindings_ctx,
                            &device_id,
                            Ipv6DiscoveredRoute {
                                subnet: rio.prefix().clone(),
                                gateway: Some(src_ip),
                            },
                            rio.route_lifetime(),
                        )
                    }
                    NdpOption::Mtu(mtu) => {
                        debug!("processing MTU option in RA: {:?}", mtu);
                        // TODO(https://fxbug.dev/42052173): Control whether or
                        // not we should update the link's MTU in response to
                        // RAs.
                        Ipv6DeviceHandler::set_link_mtu(core_ctx, &device_id, Mtu::new(mtu));
                    }
                }
            }

            bindings_ctx.on_event(RouterAdvertisementEvent {
                options_bytes: Box::from(p.body().bytes()),
                source: **src_ip,
                device: device_id.clone(),
            });
        }
    }
}

impl<
        BC: IcmpBindingsContext + NdpBindingsContext<CC::DeviceId>,
        CC: InnerIcmpv6Context<BC>
            + InnerIcmpContext<Ipv6, BC>
            + Ipv6DeviceHandler<BC>
            + IpDeviceHandler<Ipv6, BC>
            + IpDeviceIngressStateContext<Ipv6>
            + PmtuHandler<Ipv6, BC>
            + NudIpHandler<Ipv6, BC>
            + IpLayerHandler<Ipv6, BC>
            + CounterContext<IcmpRxCounters<Ipv6>>
            + CounterContext<IcmpTxCounters<Ipv6>>
            + CounterContext<NdpCounters>,
    > IpTransportContext<Ipv6, BC, CC> for IcmpIpTransportContext
{
    fn receive_icmp_error(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        device: &CC::DeviceId,
        original_src_ip: Option<SpecifiedAddr<Ipv6Addr>>,
        original_dst_ip: SpecifiedAddr<Ipv6Addr>,
        original_body: &[u8],
        err: Icmpv6ErrorCode,
    ) {
        receive_ip_transport_icmp_error(
            core_ctx,
            bindings_ctx,
            device,
            original_src_ip,
            original_dst_ip,
            original_body,
            err,
        )
    }

    fn receive_ip_packet<B: BufferMut, H: IpHeaderInfo<Ipv6>>(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        device: &CC::DeviceId,
        src_ip: Ipv6SourceAddr,
        dst_ip: SpecifiedAddr<Ipv6Addr>,
        mut buffer: B,
        info: &LocalDeliveryPacketInfo<Ipv6, H>,
    ) -> Result<(), (B, TransportReceiveError)> {
        let LocalDeliveryPacketInfo { meta, header_info, marks } = info;
        let ReceiveIpPacketMeta { broadcast: _, transparent_override } = meta;
        if let Some(delivery) = transparent_override {
            unreachable!(
                "cannot perform transparent local delivery {delivery:?} to an ICMP socket; \
                transparent proxy rules can only be configured for TCP and UDP packets"
            );
        }

        trace!(
            "<IcmpIpTransportContext as IpTransportContext<Ipv6>>::receive_ip_packet({:?}, {})",
            src_ip,
            dst_ip
        );

        let packet = match buffer
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip.get(), dst_ip))
        {
            Ok(packet) => packet,
            Err(_) => return Ok(()), // TODO(joshlf): Do something else here?
        };

        match packet {
            Icmpv6Packet::EchoRequest(echo_request) => {
                CounterContext::<IcmpRxCounters<Ipv6>>::counters(core_ctx).echo_request.increment();

                if let Some(src_ip) = SocketIpAddr::new_from_ipv6_source(src_ip) {
                    match SocketIpAddr::try_from(dst_ip) {
                        Ok(dst_ip) => {
                            let req = *echo_request.message();
                            let code = echo_request.code();
                            let (local_ip, remote_ip) = (dst_ip, src_ip);
                            debug!(
                                "replying to ICMP echo request from {remote_ip}: id={}, seq={}",
                                req.id(),
                                req.seq()
                            );
                            send_icmp_reply(
                                core_ctx,
                                bindings_ctx,
                                device,
                                remote_ip,
                                local_ip,
                                |src_ip| {
                                    IcmpPacketBuilder::<Ipv6, _>::new(
                                        src_ip,
                                        remote_ip.addr(),
                                        code,
                                        req.reply(),
                                    )
                                    .wrap_body(buffer)
                                },
                                &WithMarks(marks),
                            );
                        }
                        Err(AddrIsMappedError {}) => {
                            trace!("IpTransportContext<Ipv6>::receive_ip_packet: Received echo request with an ipv4-mapped-ipv6 destination address");
                        }
                    }
                } else {
                    trace!("<IcmpIpTransportContext as IpTransportContext<Ipv6>>::receive_ip_packet: Received echo request with an unspecified source address");
                }
            }
            Icmpv6Packet::EchoReply(echo_reply) => {
                CounterContext::<IcmpRxCounters<Ipv6>>::counters(core_ctx).echo_reply.increment();
                trace!("<IcmpIpTransportContext as IpTransportContext<Ipv6>>::receive_ip_packet: Received an EchoReply message");
                let parse_metadata = echo_reply.parse_metadata();
                buffer.undo_parse(parse_metadata);
                return <CC::EchoTransportContext
                            as IpTransportContext<Ipv6, BC, CC>>::receive_ip_packet(
                        core_ctx,
                        bindings_ctx,
                        device,
                        src_ip,
                        dst_ip,
                        buffer,
                        info
                );
            }
            Icmpv6Packet::Ndp(packet) => {
                receive_ndp_packet(core_ctx, bindings_ctx, device, src_ip, packet, header_info)
            }
            Icmpv6Packet::PacketTooBig(packet_too_big) => {
                CounterContext::<IcmpRxCounters<Ipv6>>::counters(core_ctx)
                    .packet_too_big
                    .increment();
                trace!("<IcmpIpTransportContext as IpTransportContext<Ipv6>>::receive_ip_packet: Received a Packet Too Big message");
                let new_mtu = if let Ipv6SourceAddr::Unicast(src_ip) = src_ip {
                    // We are updating the path MTU from the destination address
                    // of this `packet` (which is an IP address on this node) to
                    // some remote (identified by the source address of this
                    // `packet`).
                    //
                    // `update_pmtu_if_less` will only update the PMTU if the
                    // Packet Too Big message's MTU field had a value that was
                    // at least the IPv6 minimum MTU (which is required by IPv6
                    // RFC 8200).
                    core_ctx.update_pmtu_if_less(
                        bindings_ctx,
                        dst_ip.get(),
                        src_ip.get(),
                        Mtu::new(packet_too_big.message().mtu()),
                    )
                } else {
                    None
                };
                if let Some(mtu) = new_mtu {
                    receive_icmpv6_error(
                        core_ctx,
                        bindings_ctx,
                        device,
                        &packet_too_big,
                        Icmpv6ErrorCode::PacketTooBig(mtu),
                    );
                }
            }
            Icmpv6Packet::Mld(packet) => {
                core_ctx.receive_mld_packet(
                    bindings_ctx,
                    &device,
                    src_ip,
                    dst_ip,
                    packet,
                    header_info,
                );
            }
            Icmpv6Packet::DestUnreachable(dest_unreachable) => receive_icmpv6_error(
                core_ctx,
                bindings_ctx,
                device,
                &dest_unreachable,
                Icmpv6ErrorCode::DestUnreachable(dest_unreachable.code()),
            ),
            Icmpv6Packet::TimeExceeded(time_exceeded) => receive_icmpv6_error(
                core_ctx,
                bindings_ctx,
                device,
                &time_exceeded,
                Icmpv6ErrorCode::TimeExceeded(time_exceeded.code()),
            ),
            Icmpv6Packet::ParameterProblem(parameter_problem) => receive_icmpv6_error(
                core_ctx,
                bindings_ctx,
                device,
                &parameter_problem,
                Icmpv6ErrorCode::ParameterProblem(parameter_problem.code()),
            ),
        }

        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct WithMarks<'a>(&'a Marks);

impl<'a> OptionDelegationMarker for WithMarks<'a> {}

impl<'a, I: IpExt> DelegatedRouteResolutionOptions<I> for WithMarks<'a> {
    fn marks(&self) -> &Marks {
        let Self(marks) = self;
        marks
    }
}

impl<'a, I: IpExt> DelegatedSendOptions<I> for WithMarks<'a> {}

/// Sends an ICMP reply to a remote host.
///
/// `send_icmp_reply` sends a reply to a non-error message (e.g., "echo request"
/// or "timestamp request" messages).
///
/// `get_body_from_src_ip` returns a `Serializer` with the bytes of the ICMP
/// packet, and, when called, is given the source IP address chosen for the
/// outbound packet. This allows `get_body_from_src_ip` to properly compute the
/// ICMP checksum, which relies on both the source and destination IP addresses
/// of the IP packet it's encapsulated in.
fn send_icmp_reply<I, BC, CC, S, F, O>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    original_src_ip: SocketIpAddr<I::Addr>,
    original_dst_ip: SocketIpAddr<I::Addr>,
    get_body_from_src_ip: F,
    ip_options: &O,
) where
    I: IpExt + FilterIpExt,
    CC: IpSocketHandler<I, BC> + DeviceIdContext<AnyDevice> + CounterContext<IcmpTxCounters<I>>,
    BC: TxMetadataBindingsTypes,
    S: TransportPacketSerializer<I>,
    S::Buffer: BufferMut,
    F: FnOnce(SpecifiedAddr<I::Addr>) -> S,
    O: SendOptions<I> + RouteResolutionOptions<I>,
{
    trace!("send_icmp_reply({:?}, {}, {})", device, original_src_ip, original_dst_ip);
    core_ctx.counters().reply.increment();
    let tx_metadata: BC::TxMetadata = Default::default();

    // Force the egress device if the original destination is multicast or
    // requires a zone (i.e. link-local non-loopback), ensuring we pick the
    // correct return route.
    let egress_device = (original_dst_ip.as_ref().is_multicast()
        || original_dst_ip.as_ref().must_have_zone())
    .then_some(EitherDeviceId::Strong(device));

    core_ctx
        .send_oneshot_ip_packet(
            bindings_ctx,
            egress_device,
            IpDeviceAddr::new_from_socket_ip_addr(original_dst_ip),
            original_src_ip,
            I::ICMP_IP_PROTO,
            ip_options,
            tx_metadata,
            |src_ip| get_body_from_src_ip(src_ip.into()),
        )
        .unwrap_or_else(|err| {
            debug!("failed to send ICMP reply: {}", err);
        })
}

/// Receive an ICMP(v4) error message.
///
/// `receive_icmpv4_error` handles an incoming ICMP error message by parsing the
/// original IPv4 packet and then delegating to the context.
fn receive_icmpv4_error<
    BC: IcmpBindingsContext,
    CC: InnerIcmpv4Context<BC>,
    B: SplitByteSlice,
    M: IcmpMessage<Ipv4, Body<B> = OriginalPacket<B>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    packet: &IcmpPacket<Ipv4, B, M>,
    err: Icmpv4ErrorCode,
) {
    packet.with_original_packet(|res| match res {
        Ok(original_packet) => {
            let dst_ip = match SpecifiedAddr::new(original_packet.dst_ip()) {
                Some(ip) => ip,
                None => {
                    trace!("receive_icmpv4_error: Got ICMP error message whose original IPv4 packet contains an unspecified destination address; discarding");
                    return;
                },
            };
            InnerIcmpContext::receive_icmp_error(
                core_ctx,
                bindings_ctx,
                device,
                SpecifiedAddr::new(original_packet.src_ip()),
                dst_ip,
                original_packet.proto(),
                original_packet.body().into_inner(),
                err,
            );
        }
        Err(_) => debug!(
            "receive_icmpv4_error: Got ICMP error message with unparsable original IPv4 packet"
        ),
    })
}

/// Receive an ICMPv6 error message.
///
/// `receive_icmpv6_error` handles an incoming ICMPv6 error message by parsing
/// the original IPv6 packet and then delegating to the context.
fn receive_icmpv6_error<
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC>,
    B: SplitByteSlice,
    M: IcmpMessage<Ipv6, Body<B> = OriginalPacket<B>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    packet: &IcmpPacket<Ipv6, B, M>,
    err: Icmpv6ErrorCode,
) {
    packet.with_original_packet(|res| match res {
        Ok(original_packet) => {
            let dst_ip = match SpecifiedAddr::new(original_packet.dst_ip()) {
                Some(ip)=>ip,
                None => {
                    trace!("receive_icmpv6_error: Got ICMP error message whose original IPv6 packet contains an unspecified destination address; discarding");
                    return;
                },
            };
            match original_packet.body_proto() {
                Ok((body, proto)) => {
                    InnerIcmpContext::receive_icmp_error(
                        core_ctx,
                        bindings_ctx,
                        device,
                        SpecifiedAddr::new(original_packet.src_ip()),
                        dst_ip,
                        proto,
                        body.into_inner(),
                        err,
                    );
                }
                Err(ExtHdrParseError) => {
                    trace!("receive_icmpv6_error: We could not parse the original packet's extension headers, and so we don't know where the original packet's body begins; discarding");
                    // There's nothing we can do in this case, so we just
                    // return.
                    return;
                }
            }
        }
        Err(_body) => debug!(
            "receive_icmpv6_error: Got ICMPv6 error message with unparsable original IPv6 packet"
        ),
    })
}

/// Send an ICMP(v4) message in response to receiving a packet destined for an
/// unreachable address.
///
/// `send_icmpv4_host_unreachable` sends the appropriate ICMP message in
/// response to receiving an IP packet from `src_ip` to `dst_ip`, where
/// `dst_ip` is unreachable. In particular, this is an ICMP
/// "destination unreachable" message with a "host unreachable" code.
///
/// `original_packet` must be an initial fragment or a complete IP
/// packet, per [RFC 792 Introduction]:
///
///   Also ICMP messages are only sent about errors in handling fragment zero of
///   fragemented [sic] datagrams.
///
/// `header_len` is the length of the header including all options.
///
/// [RFC 792 Introduction]: https://datatracker.ietf.org/doc/html/rfc792
pub fn send_icmpv4_host_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv4Context<BC> + CounterContext<IcmpTxCounters<Ipv4>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: Option<&CC::DeviceId>,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv4Addr>,
    dst_ip: SocketIpAddr<Ipv4Addr>,
    original_packet: B,
    header_len: usize,
    fragment_type: Ipv4FragmentType,
    marks: &Marks,
) {
    core_ctx.counters().address_unreachable.increment();

    send_icmpv4_dest_unreachable(
        core_ctx,
        bindings_ctx,
        device,
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv4DestUnreachableCode::DestHostUnreachable,
        original_packet,
        header_len,
        fragment_type,
        marks,
    );
}

/// Send an ICMPv6 message in response to receiving a packet destined for an
/// unreachable address.
///
/// `send_icmpv6_address_unreachable` sends the appropriate ICMP message in
/// response to receiving an IP packet from `src_ip` to `dst_ip`, where
/// `dst_ip` is unreachable. In particular, this is an ICMP
/// "destination unreachable" message with an "address unreachable" code.
///
/// `original_packet` contains the contents of the entire original packet,
/// including extension headers.
pub fn send_icmpv6_address_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: Option<&CC::DeviceId>,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv6Addr>,
    dst_ip: SocketIpAddr<Ipv6Addr>,
    original_packet: B,
    marks: &Marks,
) {
    core_ctx.counters().address_unreachable.increment();

    send_icmpv6_dest_unreachable(
        core_ctx,
        bindings_ctx,
        device,
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv6DestUnreachableCode::AddrUnreachable,
        original_packet,
        marks,
    );
}

/// Send an ICMP(v4) message in response to receiving a packet destined for an
/// unsupported IPv4 protocol.
///
/// `send_icmpv4_protocol_unreachable` sends the appropriate ICMP message in
/// response to receiving an IP packet from `src_ip` to `dst_ip` identifying an
/// unsupported protocol - in particular, a "destination unreachable" message
/// with a "protocol unreachable" code.
///
/// `original_packet` contains the contents of the entire original packet,
/// including the IP header. This must be a whole packet, not a packet fragment.
/// `header_len` is the length of the header including all options.
pub(crate) fn send_icmpv4_protocol_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv4Context<BC> + CounterContext<IcmpTxCounters<Ipv4>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv4Addr>,
    dst_ip: SocketIpAddr<Ipv4Addr>,
    original_packet: B,
    header_len: usize,
    marks: &Marks,
) {
    core_ctx.counters().protocol_unreachable.increment();

    send_icmpv4_dest_unreachable(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv4DestUnreachableCode::DestProtocolUnreachable,
        original_packet,
        header_len,
        // If we are sending a protocol unreachable error it is correct to assume that, if the
        // packet was initially fragmented, it has been successfully reassembled by now. It
        // guarantees that we won't send more than one ICMP Destination Unreachable message for
        // different fragments of the same original packet, so we should behave as if we are
        // handling an initial fragment.
        Ipv4FragmentType::InitialFragment,
        marks,
    );
}

/// Send an ICMPv6 message in response to receiving a packet destined for an
/// unsupported Next Header.
///
/// `send_icmpv6_protocol_unreachable` is like
/// [`send_icmpv4_protocol_unreachable`], but for ICMPv6. It sends an ICMPv6
/// "parameter problem" message with an "unrecognized next header type" code.
///
/// `header_len` is the length of all IPv6 headers (including extension headers)
/// *before* the payload with the problematic Next Header type.
pub(crate) fn send_icmpv6_protocol_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv6Addr>,
    dst_ip: SocketIpAddr<Ipv6Addr>,
    original_packet: B,
    header_len: usize,
    marks: &Marks,
) {
    core_ctx.counters().protocol_unreachable.increment();

    send_icmpv6_parameter_problem(
        core_ctx,
        bindings_ctx,
        device,
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv6ParameterProblemCode::UnrecognizedNextHeaderType,
        // Per RFC 4443, the pointer refers to the first byte of the packet
        // whose Next Header field was unrecognized. It is measured as an offset
        // from the beginning of the first IPv6 header. E.g., a pointer of 40
        // (the length of a single IPv6 header) would indicate that the Next
        // Header field from that header - and hence of the first encapsulated
        // packet - was unrecognized.
        //
        // NOTE: Since header_len is a usize, this could theoretically be a
        // lossy conversion. However, all that means in practice is that, if a
        // remote host somehow managed to get us to process a frame with a 4GB
        // IP header and send an ICMP response, the pointer value would be
        // wrong. It's not worth wasting special logic to avoid generating a
        // malformed packet in a case that will almost certainly never happen.
        Icmpv6ParameterProblem::new(header_len as u32),
        original_packet,
        false,
        marks,
    );
}

/// Send an ICMP(v4) message in response to receiving a packet destined for an
/// unreachable local transport-layer port.
///
/// `send_icmpv4_port_unreachable` sends the appropriate ICMP message in
/// response to receiving an IP packet from `src_ip` to `dst_ip` with an
/// unreachable local transport-layer port. In particular, this is an ICMP
/// "destination unreachable" message with a "port unreachable" code.
///
/// `original_packet` contains the contents of the entire original packet,
/// including the IP header. This must be a whole packet, not a packet fragment.
/// `header_len` is the length of the header including all options.
pub(crate) fn send_icmpv4_port_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv4Context<BC> + CounterContext<IcmpTxCounters<Ipv4>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv4Addr>,
    dst_ip: SocketIpAddr<Ipv4Addr>,
    original_packet: B,
    header_len: usize,
    marks: &Marks,
) {
    core_ctx.counters().port_unreachable.increment();

    send_icmpv4_dest_unreachable(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv4DestUnreachableCode::DestPortUnreachable,
        original_packet,
        header_len,
        // If we are sending a port unreachable error it is correct to assume that, if the packet
        // was initially fragmented, it has been successfully reassembled by now. It guarantees that
        // we won't send more than one ICMP Destination Unreachable message for different fragments
        // of the same original packet, so we should behave as if we are handling an initial
        // fragment.
        Ipv4FragmentType::InitialFragment,
        marks,
    );
}

/// Send an ICMPv6 message in response to receiving a packet destined for an
/// unreachable local transport-layer port.
///
/// `send_icmpv6_port_unreachable` is like [`send_icmpv4_port_unreachable`], but
/// for ICMPv6.
///
/// `original_packet` contains the contents of the entire original packet,
/// including extension headers.
pub(crate) fn send_icmpv6_port_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv6Addr>,
    dst_ip: SocketIpAddr<Ipv6Addr>,
    original_packet: B,
    marks: &Marks,
) {
    core_ctx.counters().port_unreachable.increment();

    send_icmpv6_dest_unreachable(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv6DestUnreachableCode::PortUnreachable,
        original_packet,
        marks,
    );
}

/// Send an ICMP(v4) message in response to receiving a packet destined for an
/// unreachable network.
///
/// `send_icmpv4_net_unreachable` sends the appropriate ICMP message in response
/// to receiving an IP packet from `src_ip` to an unreachable `dst_ip`. In
/// particular, this is an ICMP "destination unreachable" message with a "net
/// unreachable" code.
///
/// `original_packet` contains the contents of the entire original packet -
/// including all IP headers. `header_len` is the length of the IPv4 header. It
/// is ignored for IPv6.
pub(crate) fn send_icmpv4_net_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv4Context<BC> + CounterContext<IcmpTxCounters<Ipv4>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv4Addr>,
    dst_ip: SocketIpAddr<Ipv4Addr>,
    proto: Ipv4Proto,
    original_packet: B,
    header_len: usize,
    fragment_type: Ipv4FragmentType,
    marks: &Marks,
) {
    core_ctx.counters().net_unreachable.increment();

    // Check whether we MUST NOT send an ICMP error message
    // because the original packet was itself an ICMP error message.
    if is_icmp_error_message::<Ipv4>(proto, &original_packet.as_ref()[header_len..]) {
        return;
    }

    send_icmpv4_dest_unreachable(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv4DestUnreachableCode::DestNetworkUnreachable,
        original_packet,
        header_len,
        fragment_type,
        marks,
    );
}

/// Send an ICMPv6 message in response to receiving a packet destined for an
/// unreachable network.
///
/// `send_icmpv6_net_unreachable` is like [`send_icmpv4_net_unreachable`], but
/// for ICMPv6. It sends an ICMPv6 "destination unreachable" message with a "no
/// route to destination" code.
///
/// `original_packet` contains the contents of the entire original packet
/// including extension headers. `header_len` is the length of the IP header and
/// all extension headers.
pub(crate) fn send_icmpv6_net_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv6Addr>,
    dst_ip: SocketIpAddr<Ipv6Addr>,
    proto: Ipv6Proto,
    original_packet: B,
    header_len: usize,
    marks: &Marks,
) {
    core_ctx.counters().net_unreachable.increment();

    // Check whether we MUST NOT send an ICMP error message
    // because the original packet was itself an ICMP error message.
    if is_icmp_error_message::<Ipv6>(proto, &original_packet.as_ref()[header_len..]) {
        return;
    }

    send_icmpv6_dest_unreachable(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv6DestUnreachableCode::NoRoute,
        original_packet,
        marks,
    );
}

/// Send an ICMP(v4) message in response to receiving a packet whose TTL has
/// expired.
///
/// `send_icmpv4_ttl_expired` sends the appropriate ICMP in response to
/// receiving an IP packet from `src_ip` to `dst_ip` whose TTL has expired. In
/// particular, this is an ICMP "time exceeded" message with a "time to live
/// exceeded in transit" code.
///
/// `original_packet` contains the contents of the entire original packet,
/// including the header. `header_len` is the length of the IP header including
/// options.
pub(crate) fn send_icmpv4_ttl_expired<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv4Context<BC> + CounterContext<IcmpTxCounters<Ipv4>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv4Addr>,
    dst_ip: SocketIpAddr<Ipv4Addr>,
    proto: Ipv4Proto,
    original_packet: B,
    header_len: usize,
    fragment_type: Ipv4FragmentType,
    marks: &Marks,
) {
    core_ctx.counters().ttl_expired.increment();

    // Check whether we MUST NOT send an ICMP error message because the original
    // packet was itself an ICMP error message.
    if is_icmp_error_message::<Ipv4>(proto, &original_packet.as_ref()[header_len..]) {
        return;
    }

    send_icmpv4_error_message(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv4ErrorMessage::TimeExceeded {
            message: IcmpTimeExceeded::default(),
            code: Icmpv4TimeExceededCode::TtlExpired,
        },
        original_packet,
        header_len,
        fragment_type,
        marks,
    )
}

/// Send an ICMPv6 message in response to receiving a packet whose hop limit has
/// expired.
///
/// `send_icmpv6_ttl_expired` is like [`send_icmpv4_ttl_expired`], but for
/// ICMPv6. It sends an ICMPv6 "time exceeded" message with a "hop limit
/// exceeded in transit" code.
///
/// `original_packet` contains the contents of the entire original packet
/// including extension headers. `header_len` is the length of the IP header and
/// all extension headers.
pub(crate) fn send_icmpv6_ttl_expired<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv6Addr>,
    dst_ip: SocketIpAddr<Ipv6Addr>,
    proto: Ipv6Proto,
    original_packet: B,
    header_len: usize,
    marks: &Marks,
) {
    core_ctx.counters().ttl_expired.increment();

    // Check whether we MUST NOT send an ICMP error message because the
    // original packet was itself an ICMP error message.
    if is_icmp_error_message::<Ipv6>(proto, &original_packet.as_ref()[header_len..]) {
        return;
    }

    send_icmpv6_error_message(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv6ErrorMessage::TimeExceeded {
            message: IcmpTimeExceeded::default(),
            code: Icmpv6TimeExceededCode::HopLimitExceeded,
        },
        original_packet,
        false, /* allow_dst_multicast */
        marks,
    )
}

// TODO(joshlf): Test send_icmpv6_packet_too_big once we support fake IPv6 test
// setups.

/// Send an ICMPv6 message in response to receiving a packet whose size exceeds
/// the MTU of the next hop interface.
///
/// `send_icmpv6_packet_too_big` sends an ICMPv6 "packet too big" message in
/// response to receiving an IP packet from `src_ip` to `dst_ip` whose size
/// exceeds the `mtu` of the next hop interface.
pub(crate) fn send_icmpv6_packet_too_big<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv6Addr>,
    dst_ip: SocketIpAddr<Ipv6Addr>,
    proto: Ipv6Proto,
    mtu: Mtu,
    original_packet: B,
    header_len: usize,
    marks: &Marks,
) {
    core_ctx.counters().packet_too_big.increment();
    // Check whether we MUST NOT send an ICMP error message because the
    // original packet was itself an ICMP error message.
    if is_icmp_error_message::<Ipv6>(proto, &original_packet.as_ref()[header_len..]) {
        return;
    }

    send_icmpv6_error_message(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv6ErrorMessage::PacketTooBig {
            message: Icmpv6PacketTooBig::new(mtu.into()),
            code: IcmpZeroCode,
        },
        original_packet,
        // As per RFC 4443 section 2.4.e,
        //
        //   An ICMPv6 error message MUST NOT be originated as a result of
        //   receiving the following:
        //
        //     (e.3) A packet destined to an IPv6 multicast address.  (There are
        //           two exceptions to this rule: (1) the Packet Too Big Message
        //           (Section 3.2) to allow Path MTU discovery to work for IPv6
        //           multicast, and (2) the Parameter Problem Message, Code 2
        //           (Section 3.4) reporting an unrecognized IPv6 option (see
        //           Section 4.2 of [IPv6]) that has the Option Type highest-
        //           order two bits set to 10).
        //
        //     (e.4) A packet sent as a link-layer multicast (the exceptions
        //           from e.3 apply to this case, too).
        //
        // Thus, we explicitly allow sending a Packet Too Big error if the
        // destination was a multicast packet.
        true, /* allow_dst_multicast */
        marks,
    )
}

pub(crate) fn send_icmpv4_parameter_problem<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv4Context<BC> + CounterContext<IcmpTxCounters<Ipv4>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv4Addr>,
    dst_ip: SocketIpAddr<Ipv4Addr>,
    code: Icmpv4ParameterProblemCode,
    parameter_problem: Icmpv4ParameterProblem,
    original_packet: B,
    header_len: usize,
    fragment_type: Ipv4FragmentType,
    marks: &Marks,
) {
    core_ctx.counters().parameter_problem.increment();

    send_icmpv4_error_message(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv4ErrorMessage::ParameterProblem { message: parameter_problem, code },
        original_packet,
        header_len,
        fragment_type,
        marks,
    )
}

/// Send an ICMPv6 Parameter Problem error message.
///
/// If the error message is Code 2 reporting an unrecognized IPv6 option that
/// has the Option Type highest-order two bits set to 10, `allow_dst_multicast`
/// must be set to `true`. See [`should_send_icmpv6_error`] for more details.
///
/// # Panics
///
/// Panics if `allow_multicast_addr` is set to `true`, but this Parameter
/// Problem's code is not 2 (Unrecognized IPv6 Option).
pub(crate) fn send_icmpv6_parameter_problem<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv6Addr>,
    dst_ip: SocketIpAddr<Ipv6Addr>,
    code: Icmpv6ParameterProblemCode,
    parameter_problem: Icmpv6ParameterProblem,
    original_packet: B,
    allow_dst_multicast: bool,
    marks: &Marks,
) {
    // Only allow the `allow_dst_multicast` parameter to be set if the code is
    // the unrecognized IPv6 option as that is one of the few exceptions where
    // we can send an ICMP packet in response to a packet that was destined for
    // a multicast address.
    assert!(!allow_dst_multicast || code == Icmpv6ParameterProblemCode::UnrecognizedIpv6Option);

    core_ctx.counters().parameter_problem.increment();

    send_icmpv6_error_message(
        core_ctx,
        bindings_ctx,
        Some(device),
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv6ErrorMessage::ParameterProblem { message: parameter_problem, code },
        original_packet,
        allow_dst_multicast,
        marks,
    )
}

fn send_icmpv4_dest_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv4Context<BC> + CounterContext<IcmpTxCounters<Ipv4>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: Option<&CC::DeviceId>,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv4Addr>,
    dst_ip: SocketIpAddr<Ipv4Addr>,
    code: Icmpv4DestUnreachableCode,
    original_packet: B,
    header_len: usize,
    fragment_type: Ipv4FragmentType,
    marks: &Marks,
) {
    core_ctx.counters().dest_unreachable.increment();
    send_icmpv4_error_message(
        core_ctx,
        bindings_ctx,
        device,
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv4ErrorMessage::DestUnreachable { message: IcmpDestUnreachable::default(), code },
        original_packet,
        header_len,
        fragment_type,
        marks,
    )
}

fn send_icmpv6_dest_unreachable<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: Option<&CC::DeviceId>,
    frame_dst: Option<FrameDestination>,
    src_ip: SocketIpAddr<Ipv6Addr>,
    dst_ip: SocketIpAddr<Ipv6Addr>,
    code: Icmpv6DestUnreachableCode,
    original_packet: B,
    marks: &Marks,
) {
    send_icmpv6_error_message(
        core_ctx,
        bindings_ctx,
        device,
        frame_dst,
        src_ip,
        dst_ip,
        Icmpv6ErrorMessage::DestUnreachable { message: IcmpDestUnreachable::default(), code },
        original_packet,
        false, /* allow_dst_multicast */
        marks,
    )
}

/// A type to allow implementing the required filtering traits on a concrete
/// subset of message types.
#[allow(missing_docs)]
enum Icmpv4ErrorMessage {
    TimeExceeded {
        message: IcmpTimeExceeded,
        code: <IcmpTimeExceeded as IcmpMessage<Ipv4>>::Code,
    },
    ParameterProblem {
        message: Icmpv4ParameterProblem,
        code: <Icmpv4ParameterProblem as IcmpMessage<Ipv4>>::Code,
    },
    DestUnreachable {
        message: IcmpDestUnreachable,
        code: <IcmpDestUnreachable as IcmpMessage<Ipv4>>::Code,
    },
}

fn send_icmpv4_error_message<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv4Context<BC> + CounterContext<IcmpTxCounters<Ipv4>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: Option<&CC::DeviceId>,
    frame_dst: Option<FrameDestination>,
    original_src_ip: SocketIpAddr<Ipv4Addr>,
    original_dst_ip: SocketIpAddr<Ipv4Addr>,
    message: Icmpv4ErrorMessage,
    mut original_packet: B,
    header_len: usize,
    fragment_type: Ipv4FragmentType,
    marks: &Marks,
) {
    // TODO(https://fxbug.dev/42177876): Come up with rules for when to send ICMP
    // error messages.

    if !should_send_icmpv4_error(
        frame_dst,
        original_src_ip.into(),
        original_dst_ip.into(),
        fragment_type,
    ) {
        return;
    }

    // Per RFC 792, body contains entire IPv4 header + 64 bytes of original
    // body.
    original_packet.shrink_back_to(header_len + 64);

    let tx_metadata: BC::TxMetadata = Default::default();

    macro_rules! send {
        ($message:expr, $code:expr) => {{
            // TODO(https://fxbug.dev/42177877): Improve source address selection for ICMP
            // errors sent from unnumbered/router interfaces.
            let _ = try_send_error!(
                core_ctx,
                bindings_ctx,
                core_ctx.send_oneshot_ip_packet(
                    bindings_ctx,
                    device.map(EitherDeviceId::Strong),
                    None,
                    original_src_ip,
                    Ipv4Proto::Icmp,
                    &WithMarks(marks),
                    tx_metadata,
                    |local_ip| {
                        IcmpPacketBuilder::<Ipv4, _>::new(
                            local_ip.addr(),
                            original_src_ip.addr(),
                            $code,
                            $message,
                        )
                        .wrap_body(original_packet)
                    },
                )
            );
        }};
    }

    match message {
        Icmpv4ErrorMessage::TimeExceeded { message, code } => send!(message, code),
        Icmpv4ErrorMessage::ParameterProblem { message, code } => send!(message, code),
        Icmpv4ErrorMessage::DestUnreachable { message, code } => send!(message, code),
    }
}

/// A type to allow implementing the required filtering traits on a concrete
/// subset of message types.
#[allow(missing_docs)]
enum Icmpv6ErrorMessage {
    TimeExceeded {
        message: IcmpTimeExceeded,
        code: <IcmpTimeExceeded as IcmpMessage<Ipv6>>::Code,
    },
    PacketTooBig {
        message: Icmpv6PacketTooBig,
        code: <Icmpv6PacketTooBig as IcmpMessage<Ipv6>>::Code,
    },
    ParameterProblem {
        message: Icmpv6ParameterProblem,
        code: <Icmpv6ParameterProblem as IcmpMessage<Ipv6>>::Code,
    },
    DestUnreachable {
        message: IcmpDestUnreachable,
        code: <IcmpDestUnreachable as IcmpMessage<Ipv6>>::Code,
    },
}

fn send_icmpv6_error_message<
    B: BufferMut,
    BC: IcmpBindingsContext,
    CC: InnerIcmpv6Context<BC> + CounterContext<IcmpTxCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: Option<&CC::DeviceId>,
    frame_dst: Option<FrameDestination>,
    original_src_ip: SocketIpAddr<Ipv6Addr>,
    original_dst_ip: SocketIpAddr<Ipv6Addr>,
    message: Icmpv6ErrorMessage,
    original_packet: B,
    allow_dst_multicast: bool,
    marks: &Marks,
) {
    // TODO(https://fxbug.dev/42177876): Come up with rules for when to send ICMP
    // error messages.

    if !should_send_icmpv6_error(
        frame_dst,
        original_src_ip.into(),
        original_dst_ip.into(),
        allow_dst_multicast,
    ) {
        return;
    }

    struct Icmpv6ErrorOptions<'a>(&'a Marks);
    impl<'a> OptionDelegationMarker for Icmpv6ErrorOptions<'a> {}
    impl<'a> DelegatedSendOptions<Ipv6> for Icmpv6ErrorOptions<'a> {
        fn mtu(&self) -> Mtu {
            Ipv6::MINIMUM_LINK_MTU
        }
    }
    impl<'a> DelegatedRouteResolutionOptions<Ipv6> for Icmpv6ErrorOptions<'a> {
        fn marks(&self) -> &Marks {
            let Self(marks) = self;
            marks
        }
    }

    let tx_metadata: BC::TxMetadata = Default::default();

    macro_rules! send {
        ($message:expr, $code:expr) => {{
            // TODO(https://fxbug.dev/42177877): Improve source address selection for ICMP
            // errors sent from unnumbered/router interfaces.
            let _ = try_send_error!(
                core_ctx,
                bindings_ctx,
                core_ctx.send_oneshot_ip_packet(
                    bindings_ctx,
                    device.map(EitherDeviceId::Strong),
                    None,
                    original_src_ip,
                    Ipv6Proto::Icmpv6,
                    &Icmpv6ErrorOptions(marks),
                    tx_metadata,
                    |local_ip| {
                        let icmp_builder = IcmpPacketBuilder::<Ipv6, _>::new(
                            local_ip.addr(),
                            original_src_ip.addr(),
                            $code,
                            $message,
                        );

                        // Per RFC 4443, body contains as much of the original body as
                        // possible without exceeding IPv6 minimum MTU.
                        icmp_builder.wrap_body(TruncatingSerializer::new(
                            original_packet,
                            TruncateDirection::DiscardBack,
                        ))
                    },
                )
            );
        }};
    }

    match message {
        Icmpv6ErrorMessage::TimeExceeded { message, code } => send!(message, code),
        Icmpv6ErrorMessage::PacketTooBig { message, code } => send!(message, code),
        Icmpv6ErrorMessage::ParameterProblem { message, code } => send!(message, code),
        Icmpv6ErrorMessage::DestUnreachable { message, code } => send!(message, code),
    }
}

/// Should we send an ICMP(v4) response?
///
/// `should_send_icmpv4_error` implements the logic described in RFC 1122
/// Section 3.2.2. It decides whether, upon receiving an incoming packet with
/// the given parameters, we should send an ICMP response or not. In particular,
/// we do not send an ICMP response if we've received:
/// - a packet destined to a broadcast or multicast address
/// - a packet sent in a link-layer broadcast
/// - a non-initial fragment
/// - a packet whose source address does not define a single host (a
///   zero/unspecified address, a loopback address, a broadcast address, a
///   multicast address, or a Class E address)
///
/// Note that `should_send_icmpv4_error` does NOT check whether the incoming
/// packet contained an ICMP error message. This is because that check is
/// unnecessary for some ICMP error conditions. The ICMP error message check can
/// be performed separately with `is_icmp_error_message`.
fn should_send_icmpv4_error(
    frame_dst: Option<FrameDestination>,
    src_ip: SpecifiedAddr<Ipv4Addr>,
    dst_ip: SpecifiedAddr<Ipv4Addr>,
    fragment_type: Ipv4FragmentType,
) -> bool {
    // NOTE: We do not explicitly implement the "unspecified address" check, as
    // it is enforced by the types of the arguments.

    // TODO(joshlf): Implement the rest of the rules:
    // - a packet destined to a subnet broadcast address
    // - a packet whose source address is a subnet broadcast address

    // NOTE: The FrameDestination type has variants for unicast, multicast, and
    // broadcast. One implication of the fact that we only check for broadcast
    // here (in compliance with the RFC) is that we could, in one very unlikely
    // edge case, respond with an ICMP error message to an IP packet which was
    // sent in a link-layer multicast frame. In particular, that can happen if
    // we subscribe to a multicast IP group and, as a result, subscribe to the
    // corresponding multicast MAC address, and we receive a unicast IP packet
    // in a multicast link-layer frame destined to that MAC address.
    //
    // TODO(joshlf): Should we filter incoming multicast IP traffic to make sure
    // that it matches the multicast MAC address of the frame it was
    // encapsulated in?
    fragment_type == Ipv4FragmentType::InitialFragment
        && !(dst_ip.is_multicast()
            || dst_ip.is_limited_broadcast()
            || frame_dst.is_some_and(|dst| dst.is_broadcast())
            || src_ip.is_loopback()
            || src_ip.is_limited_broadcast()
            || src_ip.is_multicast()
            || src_ip.is_class_e())
}

/// Should we send an ICMPv6 response?
///
/// `should_send_icmpv6_error` implements the logic described in RFC 4443
/// Section 2.4.e. It decides whether, upon receiving an incoming packet with
/// the given parameters, we should send an ICMP response or not. In particular,
/// we do not send an ICMP response if we've received:
/// - a packet destined to a multicast address
///   - Two exceptions to this rules:
///     1) the Packet Too Big Message to allow Path MTU discovery to work for
///        IPv6 multicast
///     2) the Parameter Problem Message, Code 2 reporting an unrecognized IPv6
///        option that has the Option Type highest-order two bits set to 10
/// - a packet sent as a link-layer multicast or broadcast
///   - same exceptions apply here as well.
/// - a packet whose source address does not define a single host (a
///   zero/unspecified address, a loopback address, or a multicast address)
///
/// If an ICMP response will be a Packet Too Big Message or a Parameter Problem
/// Message, Code 2 reporting an unrecognized IPv6 option that has the Option
/// Type highest-order two bits set to 10, `info.allow_dst_multicast` must be
/// set to `true` so this function will allow the exception mentioned above.
///
/// Note that `should_send_icmpv6_error` does NOT check whether the incoming
/// packet contained an ICMP error message. This is because that check is
/// unnecessary for some ICMP error conditions. The ICMP error message check can
/// be performed separately with `is_icmp_error_message`.
fn should_send_icmpv6_error(
    frame_dst: Option<FrameDestination>,
    src_ip: SpecifiedAddr<Ipv6Addr>,
    dst_ip: SpecifiedAddr<Ipv6Addr>,
    allow_dst_multicast: bool,
) -> bool {
    // NOTE: We do not explicitly implement the "unspecified address" check, as
    // it is enforced by the types of the arguments.
    let multicast_frame_dst = match frame_dst {
        Some(FrameDestination::Individual { local: _ }) | None => false,
        Some(FrameDestination::Broadcast) | Some(FrameDestination::Multicast) => true,
    };
    if (dst_ip.is_multicast() || multicast_frame_dst) && !allow_dst_multicast {
        return false;
    }
    if src_ip.is_loopback() || src_ip.is_multicast() {
        return false;
    }
    true
}

/// Determine whether or not an IP packet body contains an ICMP error message
/// for the purposes of determining whether or not to send an ICMP response.
///
/// `is_icmp_error_message` checks whether `proto` is ICMP(v4) for IPv4 or
/// ICMPv6 for IPv6 and, if so, attempts to parse `buf` as an ICMP packet in
/// order to determine whether it is an error message or not. If parsing fails,
/// it conservatively assumes that it is an error packet in order to avoid
/// violating the MUST NOT directives of RFC 1122 Section 3.2.2 and [RFC 4443
/// Section 2.4.e].
///
/// [RFC 4443 Section 2.4.e]: https://tools.ietf.org/html/rfc4443#section-2.4
fn is_icmp_error_message<I: IcmpIpExt>(proto: I::Proto, buf: &[u8]) -> bool {
    proto == I::ICMP_IP_PROTO
        && peek_message_type::<I::IcmpMessageType>(buf).map(IcmpMessageType::is_err).unwrap_or(true)
}

/// Test utilities for ICMP.
#[cfg(any(test, feature = "testutils"))]
pub(crate) mod testutil {
    use alloc::vec::Vec;
    use net_types::ethernet::Mac;
    use net_types::ip::{Ipv6, Ipv6Addr};
    use packet::{Buf, InnerPacketBuilder as _, Serializer as _};
    use packet_formats::icmp::ndp::options::NdpOptionBuilder;
    use packet_formats::icmp::ndp::{
        NeighborAdvertisement, NeighborSolicitation, OptionSequenceBuilder,
    };
    use packet_formats::icmp::{IcmpPacketBuilder, IcmpZeroCode};
    use packet_formats::ip::Ipv6Proto;
    use packet_formats::ipv6::Ipv6PacketBuilder;

    use super::REQUIRED_NDP_IP_PACKET_HOP_LIMIT;

    /// Serialize an IP packet containing a neighbor advertisement with the
    /// provided parameters.
    pub fn neighbor_advertisement_ip_packet(
        src_ip: Ipv6Addr,
        dst_ip: Ipv6Addr,
        router_flag: bool,
        solicited_flag: bool,
        override_flag: bool,
        mac: Mac,
    ) -> Buf<Vec<u8>> {
        OptionSequenceBuilder::new([NdpOptionBuilder::TargetLinkLayerAddress(&mac.bytes())].iter())
            .into_serializer()
            .wrap_in(IcmpPacketBuilder::<Ipv6, _>::new(
                src_ip,
                dst_ip,
                IcmpZeroCode,
                NeighborAdvertisement::new(router_flag, solicited_flag, override_flag, src_ip),
            ))
            .wrap_in(Ipv6PacketBuilder::new(
                src_ip,
                dst_ip,
                REQUIRED_NDP_IP_PACKET_HOP_LIMIT,
                Ipv6Proto::Icmpv6,
            ))
            .serialize_vec_outer()
            .unwrap()
            .unwrap_b()
    }

    /// Serialize an IP packet containing a neighbor solicitation with the
    /// provided parameters.
    pub fn neighbor_solicitation_ip_packet(
        src_ip: Ipv6Addr,
        dst_ip: Ipv6Addr,
        target_addr: Ipv6Addr,
        mac: Mac,
    ) -> Buf<Vec<u8>> {
        OptionSequenceBuilder::new([NdpOptionBuilder::SourceLinkLayerAddress(&mac.bytes())].iter())
            .into_serializer()
            .wrap_in(IcmpPacketBuilder::<Ipv6, _>::new(
                src_ip,
                dst_ip,
                IcmpZeroCode,
                NeighborSolicitation::new(target_addr),
            ))
            .wrap_in(Ipv6PacketBuilder::new(
                src_ip,
                dst_ip,
                REQUIRED_NDP_IP_PACKET_HOP_LIMIT,
                Ipv6Proto::Icmpv6,
            ))
            .serialize_vec_outer()
            .unwrap()
            .unwrap_b()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;
    use packet_formats::icmp::ndp::options::NdpNonce;

    use core::fmt::Debug;
    use core::time::Duration;

    use net_types::ip::Subnet;
    use netstack3_base::testutil::{
        set_logger_for_test, FakeBindingsCtx, FakeCoreCtx, FakeDeviceId, FakeInstant,
        FakeTxMetadata, FakeWeakDeviceId, TestIpExt, TEST_ADDRS_V4, TEST_ADDRS_V6,
    };
    use netstack3_base::{CtxPair, Uninstantiable};
    use packet::{Buf, EmptyBuf};
    use packet_formats::icmp::mld::MldPacket;
    use packet_formats::ip::IpProto;
    use packet_formats::utils::NonZeroDuration;

    use super::*;
    use crate::internal::base::{IpDeviceEgressStateContext, RouterAdvertisementEvent};
    use crate::internal::socket::testutil::{FakeDeviceConfig, FakeIpSocketCtx};
    use crate::internal::socket::{
        IpSock, IpSockCreationError, IpSockSendError, IpSocketHandler, SendOptions,
    };
    use crate::socket::RouteResolutionOptions;

    /// The FakeCoreCtx held as the inner state of [`FakeIcmpCoreCtx`].
    type InnerIpSocketCtx<I> = FakeCoreCtx<
        FakeIpSocketCtx<I, FakeDeviceId>,
        SendIpPacketMeta<I, FakeDeviceId, SpecifiedAddr<<I as Ip>::Addr>>,
        FakeDeviceId,
    >;

    /// `FakeCoreCtx` specialized for ICMP.
    pub(super) struct FakeIcmpCoreCtx<I: IpExt> {
        ip_socket_ctx: InnerIpSocketCtx<I>,
        icmp: FakeIcmpCoreCtxState<I>,
    }

    /// `FakeBindingsCtx` specialized for ICMP.
    type FakeIcmpBindingsCtx<I> = FakeBindingsCtx<
        (),
        RouterAdvertisementEvent<FakeDeviceId>,
        FakeIcmpBindingsCtxState<I>,
        (),
    >;

    /// A fake ICMP bindings and core contexts.
    ///
    /// This is exposed to super so it can be shared with the socket tests.
    pub(super) type FakeIcmpCtx<I> = CtxPair<FakeIcmpCoreCtx<I>, FakeIcmpBindingsCtx<I>>;

    pub(super) struct FakeIcmpCoreCtxState<I: IpExt> {
        error_send_bucket: TokenBucket<FakeInstant>,
        receive_icmp_error: Vec<I::ErrorCode>,
        rx_counters: IcmpRxCounters<I>,
        tx_counters: IcmpTxCounters<I>,
        ndp_counters: NdpCounters,
    }

    impl<I: TestIpExt + IpExt> FakeIcmpCoreCtx<I> {
        fn with_errors_per_second(errors_per_second: u64) -> Self {
            Self {
                icmp: FakeIcmpCoreCtxState {
                    error_send_bucket: TokenBucket::new(errors_per_second),
                    receive_icmp_error: Default::default(),
                    rx_counters: Default::default(),
                    tx_counters: Default::default(),
                    ndp_counters: Default::default(),
                },
                ip_socket_ctx: InnerIpSocketCtx::with_state(FakeIpSocketCtx::new(
                    core::iter::once(FakeDeviceConfig {
                        device: FakeDeviceId,
                        local_ips: vec![I::TEST_ADDRS.local_ip],
                        remote_ips: vec![I::TEST_ADDRS.remote_ip],
                    }),
                )),
            }
        }
    }

    impl<I: TestIpExt + IpExt> Default for FakeIcmpCoreCtx<I> {
        fn default() -> Self {
            Self::with_errors_per_second(DEFAULT_ERRORS_PER_SECOND)
        }
    }

    impl<I: IpExt> DeviceIdContext<AnyDevice> for FakeIcmpCoreCtx<I> {
        type DeviceId = FakeDeviceId;
        type WeakDeviceId = FakeWeakDeviceId<FakeDeviceId>;
    }

    impl<I: IpExt> IcmpStateContext for FakeIcmpCoreCtx<I> {}
    impl<I: IpExt> IcmpStateContext for InnerIpSocketCtx<I> {}

    impl<I: IpExt> CounterContext<IcmpRxCounters<I>> for FakeIcmpCoreCtx<I> {
        fn counters(&self) -> &IcmpRxCounters<I> {
            &self.icmp.rx_counters
        }
    }

    impl<I: IpExt> CounterContext<IcmpTxCounters<I>> for FakeIcmpCoreCtx<I> {
        fn counters(&self) -> &IcmpTxCounters<I> {
            &self.icmp.tx_counters
        }
    }

    impl<I: IpExt> CounterContext<NdpCounters> for FakeIcmpCoreCtx<I> {
        fn counters(&self) -> &NdpCounters {
            &self.icmp.ndp_counters
        }
    }

    pub enum FakeEchoIpTransportContext {}

    impl EchoTransportContextMarker for FakeEchoIpTransportContext {}

    impl<I: IpExt> IpTransportContext<I, FakeIcmpBindingsCtx<I>, FakeIcmpCoreCtx<I>>
        for FakeEchoIpTransportContext
    {
        fn receive_icmp_error(
            core_ctx: &mut FakeIcmpCoreCtx<I>,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<I>,
            _device: &FakeDeviceId,
            _original_src_ip: Option<SpecifiedAddr<I::Addr>>,
            _original_dst_ip: SpecifiedAddr<I::Addr>,
            _original_body: &[u8],
            _err: I::ErrorCode,
        ) {
            core_ctx.icmp.rx_counters.error_delivered_to_socket.increment()
        }

        fn receive_ip_packet<B: BufferMut, H: IpHeaderInfo<I>>(
            _core_ctx: &mut FakeIcmpCoreCtx<I>,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<I>,
            _device: &FakeDeviceId,
            _src_ip: I::RecvSrcAddr,
            _dst_ip: SpecifiedAddr<I::Addr>,
            _buffer: B,
            _info: &LocalDeliveryPacketInfo<I, H>,
        ) -> Result<(), (B, TransportReceiveError)> {
            unimplemented!()
        }
    }

    impl<I: IpExt + FilterIpExt> InnerIcmpContext<I, FakeIcmpBindingsCtx<I>> for FakeIcmpCoreCtx<I> {
        type EchoTransportContext = FakeEchoIpTransportContext;

        fn receive_icmp_error(
            &mut self,
            bindings_ctx: &mut FakeIcmpBindingsCtx<I>,
            device: &Self::DeviceId,
            original_src_ip: Option<SpecifiedAddr<I::Addr>>,
            original_dst_ip: SpecifiedAddr<I::Addr>,
            original_proto: I::Proto,
            original_body: &[u8],
            err: I::ErrorCode,
        ) {
            CounterContext::<IcmpRxCounters<I>>::counters(self).error.increment();
            self.icmp.receive_icmp_error.push(err);
            if original_proto == I::ICMP_IP_PROTO {
                receive_ip_transport_icmp_error(
                    self,
                    bindings_ctx,
                    device,
                    original_src_ip,
                    original_dst_ip,
                    original_body,
                    err,
                )
            }
        }

        fn with_error_send_bucket_mut<O, F: FnOnce(&mut TokenBucket<FakeInstant>) -> O>(
            &mut self,
            cb: F,
        ) -> O {
            cb(&mut self.icmp.error_send_bucket)
        }
    }

    #[test]
    fn test_should_send_icmpv4_error() {
        let src_ip = TEST_ADDRS_V4.local_ip;
        let dst_ip = TEST_ADDRS_V4.remote_ip;
        let frame_dst = FrameDestination::Individual { local: true };
        let multicast_ip_1 = SpecifiedAddr::new(Ipv4Addr::new([224, 0, 0, 1])).unwrap();
        let multicast_ip_2 = SpecifiedAddr::new(Ipv4Addr::new([224, 0, 0, 2])).unwrap();

        // Should Send, unless non initial fragment.
        assert!(should_send_icmpv4_error(
            Some(frame_dst),
            src_ip,
            dst_ip,
            Ipv4FragmentType::InitialFragment
        ));
        assert!(should_send_icmpv4_error(None, src_ip, dst_ip, Ipv4FragmentType::InitialFragment));
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            src_ip,
            dst_ip,
            Ipv4FragmentType::NonInitialFragment
        ));

        // Should not send because destined for IP broadcast addr
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            src_ip,
            Ipv4::LIMITED_BROADCAST_ADDRESS,
            Ipv4FragmentType::InitialFragment
        ));
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            src_ip,
            Ipv4::LIMITED_BROADCAST_ADDRESS,
            Ipv4FragmentType::NonInitialFragment
        ));

        // Should not send because destined for multicast addr
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            src_ip,
            multicast_ip_1,
            Ipv4FragmentType::InitialFragment
        ));
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            src_ip,
            multicast_ip_1,
            Ipv4FragmentType::NonInitialFragment
        ));

        // Should not send because Link Layer Broadcast.
        assert!(!should_send_icmpv4_error(
            Some(FrameDestination::Broadcast),
            src_ip,
            dst_ip,
            Ipv4FragmentType::InitialFragment
        ));
        assert!(!should_send_icmpv4_error(
            Some(FrameDestination::Broadcast),
            src_ip,
            dst_ip,
            Ipv4FragmentType::NonInitialFragment
        ));

        // Should not send because from loopback addr
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            Ipv4::LOOPBACK_ADDRESS,
            dst_ip,
            Ipv4FragmentType::InitialFragment
        ));
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            Ipv4::LOOPBACK_ADDRESS,
            dst_ip,
            Ipv4FragmentType::NonInitialFragment
        ));

        // Should not send because from limited broadcast addr
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            Ipv4::LIMITED_BROADCAST_ADDRESS,
            dst_ip,
            Ipv4FragmentType::InitialFragment
        ));
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            Ipv4::LIMITED_BROADCAST_ADDRESS,
            dst_ip,
            Ipv4FragmentType::NonInitialFragment
        ));

        // Should not send because from multicast addr
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            multicast_ip_2,
            dst_ip,
            Ipv4FragmentType::InitialFragment
        ));
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            multicast_ip_2,
            dst_ip,
            Ipv4FragmentType::NonInitialFragment
        ));

        // Should not send because from class E addr
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            SpecifiedAddr::new(Ipv4Addr::new([240, 0, 0, 1])).unwrap(),
            dst_ip,
            Ipv4FragmentType::InitialFragment
        ));
        assert!(!should_send_icmpv4_error(
            Some(frame_dst),
            SpecifiedAddr::new(Ipv4Addr::new([240, 0, 0, 1])).unwrap(),
            dst_ip,
            Ipv4FragmentType::NonInitialFragment
        ));
    }

    #[test]
    fn test_should_send_icmpv6_error() {
        let src_ip = TEST_ADDRS_V6.local_ip;
        let dst_ip = TEST_ADDRS_V6.remote_ip;
        let frame_dst = FrameDestination::Individual { local: true };
        let multicast_ip_1 =
            SpecifiedAddr::new(Ipv6Addr::new([0xff00, 0, 0, 0, 0, 0, 0, 1])).unwrap();
        let multicast_ip_2 =
            SpecifiedAddr::new(Ipv6Addr::new([0xff00, 0, 0, 0, 0, 0, 0, 2])).unwrap();

        // Should Send.
        assert!(should_send_icmpv6_error(
            Some(frame_dst),
            src_ip,
            dst_ip,
            false /* allow_dst_multicast */
        ));
        assert!(should_send_icmpv6_error(None, src_ip, dst_ip, false /* allow_dst_multicast */));
        assert!(should_send_icmpv6_error(
            Some(frame_dst),
            src_ip,
            dst_ip,
            true /* allow_dst_multicast */
        ));

        // Should not send because destined for multicast addr, unless exception
        // applies.
        assert!(!should_send_icmpv6_error(
            Some(frame_dst),
            src_ip,
            multicast_ip_1,
            false /* allow_dst_multicast */
        ));
        assert!(should_send_icmpv6_error(
            Some(frame_dst),
            src_ip,
            multicast_ip_1,
            true /* allow_dst_multicast */
        ));

        // Should not send because Link Layer Broadcast, unless exception
        // applies.
        assert!(!should_send_icmpv6_error(
            Some(FrameDestination::Broadcast),
            src_ip,
            dst_ip,
            false /* allow_dst_multicast */
        ));
        assert!(should_send_icmpv6_error(
            Some(FrameDestination::Broadcast),
            src_ip,
            dst_ip,
            true /* allow_dst_multicast */
        ));

        // Should not send because from loopback addr.
        assert!(!should_send_icmpv6_error(
            Some(frame_dst),
            Ipv6::LOOPBACK_ADDRESS,
            dst_ip,
            false /* allow_dst_multicast */
        ));
        assert!(!should_send_icmpv6_error(
            Some(frame_dst),
            Ipv6::LOOPBACK_ADDRESS,
            dst_ip,
            true /* allow_dst_multicast */
        ));

        // Should not send because from multicast addr.
        assert!(!should_send_icmpv6_error(
            Some(frame_dst),
            multicast_ip_2,
            dst_ip,
            false /* allow_dst_multicast */
        ));
        assert!(!should_send_icmpv6_error(
            Some(frame_dst),
            multicast_ip_2,
            dst_ip,
            true /* allow_dst_multicast */
        ));

        // Should not send because from multicast addr, even though dest
        // multicast exception applies.
        assert!(!should_send_icmpv6_error(
            Some(FrameDestination::Broadcast),
            multicast_ip_2,
            dst_ip,
            false /* allow_dst_multicast */
        ));
        assert!(!should_send_icmpv6_error(
            Some(FrameDestination::Broadcast),
            multicast_ip_2,
            dst_ip,
            true /* allow_dst_multicast */
        ));
        assert!(!should_send_icmpv6_error(
            Some(frame_dst),
            multicast_ip_2,
            multicast_ip_1,
            false /* allow_dst_multicast */
        ));
        assert!(!should_send_icmpv6_error(
            Some(frame_dst),
            multicast_ip_2,
            multicast_ip_1,
            true /* allow_dst_multicast */
        ));
    }

    // Tests that only require an ICMP stack. Unlike the preceding tests, these
    // only test the ICMP stack and state, and fake everything else. We define
    // the `FakeIcmpv4Ctx` and `FakeIcmpv6Ctx` types, which we wrap in a
    // `FakeCtx` to provide automatic implementations of a number of required
    // traits. The rest we implement manually.

    #[derive(Default)]
    pub(super) struct FakeIcmpBindingsCtxState<I: IpExt> {
        _marker: core::marker::PhantomData<I>,
    }

    impl InnerIcmpv4Context<FakeIcmpBindingsCtx<Ipv4>> for FakeIcmpCoreCtx<Ipv4> {
        fn should_send_timestamp_reply(&self) -> bool {
            false
        }
    }
    impl_pmtu_handler!(FakeIcmpCoreCtx<Ipv4>, FakeIcmpBindingsCtx<Ipv4>, Ipv4);
    impl_pmtu_handler!(FakeIcmpCoreCtx<Ipv6>, FakeIcmpBindingsCtx<Ipv6>, Ipv6);

    impl<I: IpExt + FilterIpExt> IpSocketHandler<I, FakeIcmpBindingsCtx<I>> for FakeIcmpCoreCtx<I> {
        fn new_ip_socket<O>(
            &mut self,
            bindings_ctx: &mut FakeIcmpBindingsCtx<I>,
            device: Option<EitherDeviceId<&Self::DeviceId, &Self::WeakDeviceId>>,
            local_ip: Option<IpDeviceAddr<I::Addr>>,
            remote_ip: SocketIpAddr<I::Addr>,
            proto: I::Proto,
            options: &O,
        ) -> Result<IpSock<I, Self::WeakDeviceId>, IpSockCreationError>
        where
            O: RouteResolutionOptions<I>,
        {
            self.ip_socket_ctx.new_ip_socket(
                bindings_ctx,
                device,
                local_ip,
                remote_ip,
                proto,
                options,
            )
        }

        fn send_ip_packet<S, O>(
            &mut self,
            bindings_ctx: &mut FakeIcmpBindingsCtx<I>,
            socket: &IpSock<I, Self::WeakDeviceId>,
            body: S,
            options: &O,
            tx_meta: FakeTxMetadata,
        ) -> Result<(), IpSockSendError>
        where
            S: TransportPacketSerializer<I>,
            S::Buffer: BufferMut,
            O: SendOptions<I> + RouteResolutionOptions<I>,
        {
            self.ip_socket_ctx.send_ip_packet(bindings_ctx, socket, body, options, tx_meta)
        }

        fn confirm_reachable<O>(
            &mut self,
            bindings_ctx: &mut FakeIcmpBindingsCtx<I>,
            socket: &IpSock<I, Self::WeakDeviceId>,
            options: &O,
        ) where
            O: RouteResolutionOptions<I>,
        {
            self.ip_socket_ctx.confirm_reachable(bindings_ctx, socket, options)
        }
    }

    impl IpDeviceHandler<Ipv6, FakeIcmpBindingsCtx<Ipv6>> for FakeIcmpCoreCtx<Ipv6> {
        fn is_router_device(&mut self, _device_id: &Self::DeviceId) -> bool {
            unimplemented!()
        }

        fn set_default_hop_limit(&mut self, _device_id: &Self::DeviceId, _hop_limit: NonZeroU8) {
            unreachable!()
        }

        fn handle_received_dad_packet(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _device_id: &Self::DeviceId,
            _addr: SpecifiedAddr<Ipv6Addr>,
            _probe_data: Option<NdpNonce<&'_ [u8]>>,
        ) -> IpAddressState {
            unimplemented!()
        }
    }

    impl IpDeviceEgressStateContext<Ipv6> for FakeIcmpCoreCtx<Ipv6> {
        fn with_next_packet_id<O, F: FnOnce(&()) -> O>(&self, cb: F) -> O {
            cb(&())
        }

        fn get_local_addr_for_remote(
            &mut self,
            _device_id: &Self::DeviceId,
            _remote: Option<SpecifiedAddr<Ipv6Addr>>,
        ) -> Option<IpDeviceAddr<Ipv6Addr>> {
            unimplemented!()
        }

        fn get_hop_limit(&mut self, _device_id: &Self::DeviceId) -> NonZeroU8 {
            unimplemented!()
        }
    }

    impl IpDeviceIngressStateContext<Ipv6> for FakeIcmpCoreCtx<Ipv6> {
        fn address_status_for_device(
            &mut self,
            _addr: SpecifiedAddr<Ipv6Addr>,
            _device_id: &Self::DeviceId,
        ) -> AddressStatus<Ipv6PresentAddressStatus> {
            unimplemented!()
        }
    }

    impl Ipv6DeviceHandler<FakeIcmpBindingsCtx<Ipv6>> for FakeIcmpCoreCtx<Ipv6> {
        type LinkLayerAddr = Uninstantiable;

        fn get_link_layer_addr(&mut self, _device_id: &Self::DeviceId) -> Option<Uninstantiable> {
            unimplemented!()
        }

        fn set_discovered_retrans_timer(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _device_id: &Self::DeviceId,
            _retrans_timer: NonZeroDuration,
        ) {
            unimplemented!()
        }

        fn set_link_mtu(&mut self, _device_id: &Self::DeviceId, _mtu: Mtu) {
            unimplemented!()
        }

        fn update_discovered_ipv6_route(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _device_id: &Self::DeviceId,
            _route: Ipv6DiscoveredRoute,
            _lifetime: Option<NonZeroNdpLifetime>,
        ) {
            unimplemented!()
        }

        fn apply_slaac_update(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _device_id: &Self::DeviceId,
            _subnet: Subnet<Ipv6Addr>,
            _preferred_lifetime: Option<NonZeroNdpLifetime>,
            _valid_lifetime: Option<NonZeroNdpLifetime>,
        ) {
            unimplemented!()
        }

        fn receive_mld_packet<B: SplitByteSlice, H: IpHeaderInfo<Ipv6>>(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _device: &FakeDeviceId,
            _src_ip: Ipv6SourceAddr,
            _dst_ip: SpecifiedAddr<Ipv6Addr>,
            _packet: MldPacket<B>,
            _header_info: &H,
        ) {
            unimplemented!()
        }
    }

    impl IpLayerHandler<Ipv6, FakeIcmpBindingsCtx<Ipv6>> for FakeIcmpCoreCtx<Ipv6> {
        fn send_ip_packet_from_device<S>(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _meta: SendIpPacketMeta<Ipv6, &Self::DeviceId, Option<SpecifiedAddr<Ipv6Addr>>>,
            _body: S,
        ) -> Result<(), IpSendFrameError<S>> {
            unimplemented!()
        }

        fn send_ip_frame<S>(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _device: &Self::DeviceId,
            _destination: IpPacketDestination<Ipv6, &Self::DeviceId>,
            _body: S,
        ) -> Result<(), IpSendFrameError<S>>
        where
            S: Serializer,
            S::Buffer: BufferMut,
        {
            unimplemented!()
        }
    }

    impl NudIpHandler<Ipv6, FakeIcmpBindingsCtx<Ipv6>> for FakeIcmpCoreCtx<Ipv6> {
        fn handle_neighbor_probe(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _device_id: &Self::DeviceId,
            _neighbor: SpecifiedAddr<Ipv6Addr>,
            _link_addr: &[u8],
        ) {
            unimplemented!()
        }

        fn handle_neighbor_confirmation(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _device_id: &Self::DeviceId,
            _neighbor: SpecifiedAddr<Ipv6Addr>,
            _link_addr: &[u8],
            _flags: ConfirmationFlags,
        ) {
            unimplemented!()
        }

        fn flush_neighbor_table(
            &mut self,
            _bindings_ctx: &mut FakeIcmpBindingsCtx<Ipv6>,
            _device_id: &Self::DeviceId,
        ) {
            unimplemented!()
        }
    }

    #[test]
    fn test_receive_icmpv4_error() {
        // Chosen arbitrarily to be a) non-zero (it's easy to accidentally get
        // the value 0) and, b) different from each other.
        const ICMP_ID: u16 = 0x0F;
        const SEQ_NUM: u16 = 0xF0;

        /// Test receiving an ICMP error message.
        ///
        /// Test that receiving an ICMP error message with the given code and
        /// message contents, and containing the given original IPv4 packet,
        /// results in the counter values in `assert_counters`. After that
        /// assertion passes, `f` is called on the context so that the caller
        /// can perform whatever extra validation they want.
        ///
        /// The error message will be sent from `TEST_ADDRS_V4.remote_ip` to
        /// `TEST_ADDRS_V4.local_ip`. Before the message is sent, an ICMP
        /// socket will be established with the ID `ICMP_ID`, and
        /// `test_receive_icmpv4_error_helper` will assert that its `SocketId`
        /// is 0. This allows the caller to craft the `original_packet` so that
        /// it should be delivered to this socket.
        fn test_receive_icmpv4_error_helper<
            C: Debug,
            M: IcmpMessage<Ipv4, Code = C> + Debug,
            F: Fn(&FakeIcmpCtx<Ipv4>),
        >(
            original_packet: &mut [u8],
            code: C,
            msg: M,
            f: F,
        ) {
            set_logger_for_test();

            let mut ctx: FakeIcmpCtx<Ipv4> = FakeIcmpCtx::default();

            let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
            <IcmpIpTransportContext as IpTransportContext<Ipv4, _, _>>::receive_ip_packet(
                core_ctx,
                bindings_ctx,
                &FakeDeviceId,
                Ipv4SourceAddr::new(*TEST_ADDRS_V4.remote_ip).unwrap(),
                TEST_ADDRS_V4.local_ip,
                IcmpPacketBuilder::new(TEST_ADDRS_V4.remote_ip, TEST_ADDRS_V4.local_ip, code, msg)
                    .wrap_body(Buf::new(original_packet, ..))
                    .serialize_vec_outer()
                    .unwrap(),
                &LocalDeliveryPacketInfo::default(),
            )
            .unwrap();
            f(&ctx);
        }
        // Test that, when we receive various ICMPv4 error messages, we properly
        // pass them up to the IP layer and, sometimes, to the transport layer.

        // First, test with an original packet containing an ICMP message. Since
        // this test fake supports ICMP sockets, this error can be delivered all
        // the way up the stack.

        // A buffer containing an ICMP echo request with ID `ICMP_ID` and
        // sequence number `SEQ_NUM` from the local IP to the remote IP. Any
        // ICMP error message which contains this as its original packet should
        // be delivered to the socket created in
        // `test_receive_icmpv4_error_helper`.
        let mut buffer = EmptyBuf
            .wrap_in(IcmpPacketBuilder::<Ipv4, _>::new(
                TEST_ADDRS_V4.local_ip,
                TEST_ADDRS_V4.remote_ip,
                IcmpZeroCode,
                IcmpEchoRequest::new(ICMP_ID, SEQ_NUM),
            ))
            .wrap_in(<Ipv4 as packet_formats::ip::IpExt>::PacketBuilder::new(
                TEST_ADDRS_V4.local_ip,
                TEST_ADDRS_V4.remote_ip,
                64,
                Ipv4Proto::Icmp,
            ))
            .serialize_vec_outer()
            .unwrap();

        test_receive_icmpv4_error_helper(
            buffer.as_mut(),
            Icmpv4DestUnreachableCode::DestNetworkUnreachable,
            IcmpDestUnreachable::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 1);
                let err = Icmpv4ErrorCode::DestUnreachable(
                    Icmpv4DestUnreachableCode::DestNetworkUnreachable,
                    IcmpDestUnreachable::default(),
                );
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv4_error_helper(
            buffer.as_mut(),
            Icmpv4TimeExceededCode::TtlExpired,
            IcmpTimeExceeded::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 1);
                let err = Icmpv4ErrorCode::TimeExceeded(Icmpv4TimeExceededCode::TtlExpired);
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv4_error_helper(
            buffer.as_mut(),
            Icmpv4ParameterProblemCode::PointerIndicatesError,
            Icmpv4ParameterProblem::new(0),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 1);
                let err = Icmpv4ErrorCode::ParameterProblem(
                    Icmpv4ParameterProblemCode::PointerIndicatesError,
                );
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        // Second, test with an original packet containing a malformed ICMP
        // packet (we accomplish this by leaving the IP packet's body empty). We
        // should process this packet in
        // `IcmpIpTransportContext::receive_icmp_error`, but we should go no
        // further - in particular, we should not dispatch to the Echo sockets.

        let mut buffer = <Ipv4 as packet_formats::ip::IpExt>::PacketBuilder::new(
            TEST_ADDRS_V4.local_ip,
            TEST_ADDRS_V4.remote_ip,
            64,
            Ipv4Proto::Icmp,
        )
        .wrap_body(EmptyBuf)
        .serialize_vec_outer()
        .unwrap();

        test_receive_icmpv4_error_helper(
            buffer.as_mut(),
            Icmpv4DestUnreachableCode::DestNetworkUnreachable,
            IcmpDestUnreachable::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv4ErrorCode::DestUnreachable(
                    Icmpv4DestUnreachableCode::DestNetworkUnreachable,
                    IcmpDestUnreachable::default(),
                );
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv4_error_helper(
            buffer.as_mut(),
            Icmpv4TimeExceededCode::TtlExpired,
            IcmpTimeExceeded::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv4ErrorCode::TimeExceeded(Icmpv4TimeExceededCode::TtlExpired);
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv4_error_helper(
            buffer.as_mut(),
            Icmpv4ParameterProblemCode::PointerIndicatesError,
            Icmpv4ParameterProblem::new(0),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv4ErrorCode::ParameterProblem(
                    Icmpv4ParameterProblemCode::PointerIndicatesError,
                );
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        // Third, test with an original packet containing a UDP packet. This
        // allows us to verify that protocol numbers are handled properly by
        // checking that `IcmpIpTransportContext::receive_icmp_error` was NOT
        // called.

        let mut buffer = <Ipv4 as packet_formats::ip::IpExt>::PacketBuilder::new(
            TEST_ADDRS_V4.local_ip,
            TEST_ADDRS_V4.remote_ip,
            64,
            IpProto::Udp.into(),
        )
        .wrap_body(EmptyBuf)
        .serialize_vec_outer()
        .unwrap();

        test_receive_icmpv4_error_helper(
            buffer.as_mut(),
            Icmpv4DestUnreachableCode::DestNetworkUnreachable,
            IcmpDestUnreachable::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 0);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv4ErrorCode::DestUnreachable(
                    Icmpv4DestUnreachableCode::DestNetworkUnreachable,
                    IcmpDestUnreachable::default(),
                );
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv4_error_helper(
            buffer.as_mut(),
            Icmpv4TimeExceededCode::TtlExpired,
            IcmpTimeExceeded::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 0);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv4ErrorCode::TimeExceeded(Icmpv4TimeExceededCode::TtlExpired);
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv4_error_helper(
            buffer.as_mut(),
            Icmpv4ParameterProblemCode::PointerIndicatesError,
            Icmpv4ParameterProblem::new(0),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 0);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv4ErrorCode::ParameterProblem(
                    Icmpv4ParameterProblemCode::PointerIndicatesError,
                );
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );
    }

    #[test]
    fn test_receive_icmpv6_error() {
        // Chosen arbitrarily to be a) non-zero (it's easy to accidentally get
        // the value 0) and, b) different from each other.
        const ICMP_ID: u16 = 0x0F;
        const SEQ_NUM: u16 = 0xF0;

        /// Test receiving an ICMPv6 error message.
        ///
        /// Test that receiving an ICMP error message with the given code and
        /// message contents, and containing the given original IPv4 packet,
        /// results in the counter values in `assert_counters`. After that
        /// assertion passes, `f` is called on the context so that the caller
        /// can perform whatever extra validation they want.
        ///
        /// The error message will be sent from `TEST_ADDRS_V6.remote_ip` to
        /// `TEST_ADDRS_V6.local_ip`. Before the message is sent, an ICMP
        /// socket will be established with the ID `ICMP_ID`, and
        /// `test_receive_icmpv6_error_helper` will assert that its `SocketId`
        /// is 0. This allows the caller to craft the `original_packet` so that
        /// it should be delivered to this socket.
        fn test_receive_icmpv6_error_helper<
            C: Debug,
            M: IcmpMessage<Ipv6, Code = C> + Debug,
            F: Fn(&FakeIcmpCtx<Ipv6>),
        >(
            original_packet: &mut [u8],
            code: C,
            msg: M,
            f: F,
        ) {
            set_logger_for_test();

            let mut ctx = FakeIcmpCtx::<Ipv6>::default();
            let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
            <IcmpIpTransportContext as IpTransportContext<Ipv6, _, _>>::receive_ip_packet(
                core_ctx,
                bindings_ctx,
                &FakeDeviceId,
                TEST_ADDRS_V6.remote_ip.get().try_into().unwrap(),
                TEST_ADDRS_V6.local_ip,
                IcmpPacketBuilder::new(TEST_ADDRS_V6.remote_ip, TEST_ADDRS_V6.local_ip, code, msg)
                    .wrap_body(Buf::new(original_packet, ..))
                    .serialize_vec_outer()
                    .unwrap(),
                &LocalDeliveryPacketInfo::default(),
            )
            .unwrap();
            f(&ctx);
        }
        // Test that, when we receive various ICMPv6 error messages, we properly
        // pass them up to the IP layer and, sometimes, to the transport layer.

        // First, test with an original packet containing an ICMPv6 message.
        // Since this test fake supports ICMPv6 sockets, this error can be
        // delivered all the way up the stack.

        // A buffer containing an ICMPv6 echo request with ID `ICMP_ID` and
        // sequence number `SEQ_NUM` from the local IP to the remote IP. Any
        // ICMPv6 error message which contains this as its original packet
        // should be delivered to the socket created in
        // `test_receive_icmpv6_error_helper`.
        let mut buffer = EmptyBuf
            .wrap_in(IcmpPacketBuilder::<Ipv6, _>::new(
                TEST_ADDRS_V6.local_ip,
                TEST_ADDRS_V6.remote_ip,
                IcmpZeroCode,
                IcmpEchoRequest::new(ICMP_ID, SEQ_NUM),
            ))
            .wrap_in(<Ipv6 as packet_formats::ip::IpExt>::PacketBuilder::new(
                TEST_ADDRS_V6.local_ip,
                TEST_ADDRS_V6.remote_ip,
                64,
                Ipv6Proto::Icmpv6,
            ))
            .serialize_vec_outer()
            .unwrap();

        test_receive_icmpv6_error_helper(
            buffer.as_mut(),
            Icmpv6DestUnreachableCode::NoRoute,
            IcmpDestUnreachable::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 1);
                let err = Icmpv6ErrorCode::DestUnreachable(Icmpv6DestUnreachableCode::NoRoute);
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv6_error_helper(
            buffer.as_mut(),
            Icmpv6TimeExceededCode::HopLimitExceeded,
            IcmpTimeExceeded::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 1);
                let err = Icmpv6ErrorCode::TimeExceeded(Icmpv6TimeExceededCode::HopLimitExceeded);
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv6_error_helper(
            buffer.as_mut(),
            Icmpv6ParameterProblemCode::UnrecognizedNextHeaderType,
            Icmpv6ParameterProblem::new(0),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 1);
                let err = Icmpv6ErrorCode::ParameterProblem(
                    Icmpv6ParameterProblemCode::UnrecognizedNextHeaderType,
                );
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        // Second, test with an original packet containing a malformed ICMPv6
        // packet (we accomplish this by leaving the IP packet's body empty). We
        // should process this packet in
        // `IcmpIpTransportContext::receive_icmp_error`, but we should go no
        // further - in particular, we should not call into Echo sockets.

        let mut buffer = EmptyBuf
            .wrap_in(<Ipv6 as packet_formats::ip::IpExt>::PacketBuilder::new(
                TEST_ADDRS_V6.local_ip,
                TEST_ADDRS_V6.remote_ip,
                64,
                Ipv6Proto::Icmpv6,
            ))
            .serialize_vec_outer()
            .unwrap();

        test_receive_icmpv6_error_helper(
            buffer.as_mut(),
            Icmpv6DestUnreachableCode::NoRoute,
            IcmpDestUnreachable::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv6ErrorCode::DestUnreachable(Icmpv6DestUnreachableCode::NoRoute);
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv6_error_helper(
            buffer.as_mut(),
            Icmpv6TimeExceededCode::HopLimitExceeded,
            IcmpTimeExceeded::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv6ErrorCode::TimeExceeded(Icmpv6TimeExceededCode::HopLimitExceeded);
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv6_error_helper(
            buffer.as_mut(),
            Icmpv6ParameterProblemCode::UnrecognizedNextHeaderType,
            Icmpv6ParameterProblem::new(0),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv6ErrorCode::ParameterProblem(
                    Icmpv6ParameterProblemCode::UnrecognizedNextHeaderType,
                );
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        // Third, test with an original packet containing a UDP packet. This
        // allows us to verify that protocol numbers are handled properly by
        // checking that `IcmpIpTransportContext::receive_icmp_error` was NOT
        // called.

        let mut buffer = <Ipv6 as packet_formats::ip::IpExt>::PacketBuilder::new(
            TEST_ADDRS_V6.local_ip,
            TEST_ADDRS_V6.remote_ip,
            64,
            IpProto::Udp.into(),
        )
        .wrap_body(EmptyBuf)
        .serialize_vec_outer()
        .unwrap();

        test_receive_icmpv6_error_helper(
            buffer.as_mut(),
            Icmpv6DestUnreachableCode::NoRoute,
            IcmpDestUnreachable::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 0);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv6ErrorCode::DestUnreachable(Icmpv6DestUnreachableCode::NoRoute);
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv6_error_helper(
            buffer.as_mut(),
            Icmpv6TimeExceededCode::HopLimitExceeded,
            IcmpTimeExceeded::default(),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 0);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv6ErrorCode::TimeExceeded(Icmpv6TimeExceededCode::HopLimitExceeded);
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );

        test_receive_icmpv6_error_helper(
            buffer.as_mut(),
            Icmpv6ParameterProblemCode::UnrecognizedNextHeaderType,
            Icmpv6ParameterProblem::new(0),
            |CtxPair { core_ctx, bindings_ctx: _ }| {
                assert_eq!(core_ctx.icmp.rx_counters.error.get(), 1);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_transport_layer.get(), 0);
                assert_eq!(core_ctx.icmp.rx_counters.error_delivered_to_socket.get(), 0);
                let err = Icmpv6ErrorCode::ParameterProblem(
                    Icmpv6ParameterProblemCode::UnrecognizedNextHeaderType,
                );
                assert_eq!(core_ctx.icmp.receive_icmp_error, [err]);
            },
        );
    }

    #[test]
    fn test_error_rate_limit() {
        set_logger_for_test();

        /// Call `send_icmpv4_ttl_expired` with fake values.
        fn send_icmpv4_ttl_expired_helper(
            CtxPair { core_ctx, bindings_ctx }: &mut FakeIcmpCtx<Ipv4>,
        ) {
            send_icmpv4_ttl_expired(
                core_ctx,
                bindings_ctx,
                &FakeDeviceId,
                Some(FrameDestination::Individual { local: true }),
                TEST_ADDRS_V4.remote_ip.try_into().unwrap(),
                TEST_ADDRS_V4.local_ip.try_into().unwrap(),
                IpProto::Udp.into(),
                EmptyBuf,
                0,
                Ipv4FragmentType::InitialFragment,
                &Default::default(),
            );
        }

        /// Call `send_icmpv4_parameter_problem` with fake values.
        fn send_icmpv4_parameter_problem_helper(
            CtxPair { core_ctx, bindings_ctx }: &mut FakeIcmpCtx<Ipv4>,
        ) {
            send_icmpv4_parameter_problem(
                core_ctx,
                bindings_ctx,
                &FakeDeviceId,
                Some(FrameDestination::Individual { local: true }),
                TEST_ADDRS_V4.remote_ip.try_into().unwrap(),
                TEST_ADDRS_V4.local_ip.try_into().unwrap(),
                Icmpv4ParameterProblemCode::PointerIndicatesError,
                Icmpv4ParameterProblem::new(0),
                EmptyBuf,
                0,
                Ipv4FragmentType::InitialFragment,
                &Default::default(),
            );
        }

        /// Call `send_icmpv4_dest_unreachable` with fake values.
        fn send_icmpv4_dest_unreachable_helper(
            CtxPair { core_ctx, bindings_ctx }: &mut FakeIcmpCtx<Ipv4>,
        ) {
            send_icmpv4_dest_unreachable(
                core_ctx,
                bindings_ctx,
                Some(&FakeDeviceId),
                Some(FrameDestination::Individual { local: true }),
                TEST_ADDRS_V4.remote_ip.try_into().unwrap(),
                TEST_ADDRS_V4.local_ip.try_into().unwrap(),
                Icmpv4DestUnreachableCode::DestNetworkUnreachable,
                EmptyBuf,
                0,
                Ipv4FragmentType::InitialFragment,
                &Default::default(),
            );
        }

        /// Call `send_icmpv6_ttl_expired` with fake values.
        fn send_icmpv6_ttl_expired_helper(
            CtxPair { core_ctx, bindings_ctx }: &mut FakeIcmpCtx<Ipv6>,
        ) {
            send_icmpv6_ttl_expired(
                core_ctx,
                bindings_ctx,
                &FakeDeviceId,
                Some(FrameDestination::Individual { local: true }),
                TEST_ADDRS_V6.remote_ip.try_into().unwrap(),
                TEST_ADDRS_V6.local_ip.try_into().unwrap(),
                IpProto::Udp.into(),
                EmptyBuf,
                0,
                &Default::default(),
            );
        }

        /// Call `send_icmpv6_packet_too_big` with fake values.
        fn send_icmpv6_packet_too_big_helper(
            CtxPair { core_ctx, bindings_ctx }: &mut FakeIcmpCtx<Ipv6>,
        ) {
            send_icmpv6_packet_too_big(
                core_ctx,
                bindings_ctx,
                &FakeDeviceId,
                Some(FrameDestination::Individual { local: true }),
                TEST_ADDRS_V6.remote_ip.try_into().unwrap(),
                TEST_ADDRS_V6.local_ip.try_into().unwrap(),
                IpProto::Udp.into(),
                Mtu::new(0),
                EmptyBuf,
                0,
                &Default::default(),
            );
        }

        /// Call `send_icmpv6_parameter_problem` with fake values.
        fn send_icmpv6_parameter_problem_helper(
            CtxPair { core_ctx, bindings_ctx }: &mut FakeIcmpCtx<Ipv6>,
        ) {
            send_icmpv6_parameter_problem(
                core_ctx,
                bindings_ctx,
                &FakeDeviceId,
                Some(FrameDestination::Individual { local: true }),
                TEST_ADDRS_V6.remote_ip.try_into().unwrap(),
                TEST_ADDRS_V6.local_ip.try_into().unwrap(),
                Icmpv6ParameterProblemCode::ErroneousHeaderField,
                Icmpv6ParameterProblem::new(0),
                EmptyBuf,
                false,
                &Default::default(),
            );
        }

        /// Call `send_icmpv6_dest_unreachable` with fake values.
        fn send_icmpv6_dest_unreachable_helper(
            CtxPair { core_ctx, bindings_ctx }: &mut FakeIcmpCtx<Ipv6>,
        ) {
            send_icmpv6_dest_unreachable(
                core_ctx,
                bindings_ctx,
                Some(&FakeDeviceId),
                Some(FrameDestination::Individual { local: true }),
                TEST_ADDRS_V6.remote_ip.try_into().unwrap(),
                TEST_ADDRS_V6.local_ip.try_into().unwrap(),
                Icmpv6DestUnreachableCode::NoRoute,
                EmptyBuf,
                &Default::default(),
            );
        }

        // Run tests for each function that sends error messages to make sure
        // they're all properly rate limited.

        fn run_test<I: IpExt, W: Fn(u64) -> FakeIcmpCtx<I>, S: Fn(&mut FakeIcmpCtx<I>)>(
            with_errors_per_second: W,
            send: S,
        ) {
            // Note that we could theoretically have more precise tests here
            // (e.g., a test that we send at the correct rate over the long
            // term), but those would amount to testing the `TokenBucket`
            // implementation, which has its own exhaustive tests. Instead, we
            // just have a few sanity checks to make sure that we're actually
            // invoking it when we expect to (as opposed to bypassing it
            // entirely or something).

            // Test that, if no time has elapsed, we can successfully send up to
            // `ERRORS_PER_SECOND` error messages, but no more.

            // Don't use `DEFAULT_ERRORS_PER_SECOND` because it's 2^16 and it
            // makes this test take a long time.
            const ERRORS_PER_SECOND: u64 = 64;

            let mut ctx = with_errors_per_second(ERRORS_PER_SECOND);

            for i in 0..ERRORS_PER_SECOND {
                send(&mut ctx);
                assert_eq!(ctx.core_ctx.icmp.tx_counters.error.get(), i + 1);
            }

            assert_eq!(ctx.core_ctx.icmp.tx_counters.error.get(), ERRORS_PER_SECOND);
            send(&mut ctx);
            assert_eq!(ctx.core_ctx.icmp.tx_counters.error.get(), ERRORS_PER_SECOND);

            // Test that, if we set a rate of 0, we are not able to send any
            // error messages regardless of how much time has elapsed.

            let mut ctx = with_errors_per_second(0);
            send(&mut ctx);
            assert_eq!(ctx.core_ctx.icmp.tx_counters.error.get(), 0);
            ctx.bindings_ctx.timers.instant.sleep(Duration::from_secs(1));
            send(&mut ctx);
            assert_eq!(ctx.core_ctx.icmp.tx_counters.error.get(), 0);
            ctx.bindings_ctx.timers.instant.sleep(Duration::from_secs(1));
            send(&mut ctx);
            assert_eq!(ctx.core_ctx.icmp.tx_counters.error.get(), 0);
        }

        fn with_errors_per_second_v4(errors_per_second: u64) -> FakeIcmpCtx<Ipv4> {
            CtxPair::with_core_ctx(FakeIcmpCoreCtx::with_errors_per_second(errors_per_second))
        }
        run_test::<Ipv4, _, _>(with_errors_per_second_v4, send_icmpv4_ttl_expired_helper);
        run_test::<Ipv4, _, _>(with_errors_per_second_v4, send_icmpv4_parameter_problem_helper);
        run_test::<Ipv4, _, _>(with_errors_per_second_v4, send_icmpv4_dest_unreachable_helper);

        fn with_errors_per_second_v6(errors_per_second: u64) -> FakeIcmpCtx<Ipv6> {
            CtxPair::with_core_ctx(FakeIcmpCoreCtx::with_errors_per_second(errors_per_second))
        }

        run_test::<Ipv6, _, _>(with_errors_per_second_v6, send_icmpv6_ttl_expired_helper);
        run_test::<Ipv6, _, _>(with_errors_per_second_v6, send_icmpv6_packet_too_big_helper);
        run_test::<Ipv6, _, _>(with_errors_per_second_v6, send_icmpv6_parameter_problem_helper);
        run_test::<Ipv6, _, _>(with_errors_per_second_v6, send_icmpv6_dest_unreachable_helper);
    }
}

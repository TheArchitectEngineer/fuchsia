// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TX device queues.

use alloc::vec::Vec;
use core::convert::Infallible as Never;

use derivative::Derivative;
use log::trace;
use netstack3_base::sync::Mutex;
use netstack3_base::{Device, DeviceIdContext, ErrorAndSerializer};
use packet::{
    new_buf_vec, Buf, BufferAlloc, ContiguousBuffer, GrowBufferMut, NoReuseBufferProvider,
    ReusableBuffer, Serializer,
};

use crate::internal::base::DeviceSendFrameError;
use crate::internal::queue::{fifo, DequeueState, EnqueueResult, TransmitQueueFrameError};
use crate::internal::socket::{DeviceSocketHandler, ParseSentFrameError, SentFrame};

/// State associated with a device transmit queue.
#[derive(Derivative)]
#[derivative(Default(bound = "Allocator: Default"))]
pub struct TransmitQueueState<Meta, Buffer, Allocator> {
    pub(super) allocator: Allocator,
    pub(super) queue: Option<fifo::Queue<Meta, Buffer>>,
}

/// Holds queue and dequeue state for the transmit queue.
#[derive(Derivative)]
#[derivative(Default(bound = "Allocator: Default"))]
pub struct TransmitQueue<Meta, Buffer, Allocator> {
    /// The state for dequeued packets that will be handled.
    ///
    /// See `queue` for lock ordering.
    pub(crate) deque: Mutex<DequeueState<Meta, Buffer>>,
    /// A queue of to-be-transmitted packets protected by a lock.
    ///
    /// Lock ordering: `deque` must be locked before `queue` is locked when both
    /// are needed at the same time.
    pub(crate) queue: Mutex<TransmitQueueState<Meta, Buffer, Allocator>>,
}

/// The bindings context for the transmit queue.
pub trait TransmitQueueBindingsContext<DeviceId> {
    /// Signals to bindings that TX frames are available and ready to be sent
    /// over the device.
    ///
    /// Implementations must make sure that the API call to handle queued
    /// packets is scheduled to be called as soon as possible so that enqueued
    /// TX frames are promptly handled.
    fn wake_tx_task(&mut self, device_id: &DeviceId);
}

/// Basic definitions for a transmit queue.
pub trait TransmitQueueCommon<D: Device, C>: DeviceIdContext<D> {
    /// The metadata associated with every packet in the queue.
    type Meta;
    /// An allocator of [`Self::Buffer`].
    type Allocator;
    /// The buffer type stored in the queue.
    type Buffer: GrowBufferMut + ContiguousBuffer;
    /// The context given to `send_frame` when dequeueing.
    type DequeueContext;

    /// Parses an outgoing frame for packet socket delivery.
    fn parse_outgoing_frame<'a, 'b>(
        buf: &'a [u8],
        meta: &'a Self::Meta,
    ) -> Result<SentFrame<&'a [u8]>, ParseSentFrameError>;
}

/// The execution context for a transmit queue.
pub trait TransmitQueueContext<D: Device, BC>: TransmitQueueCommon<D, BC> {
    /// Calls `cb` with mutable access to the queue state.
    fn with_transmit_queue_mut<
        O,
        F: FnOnce(&mut TransmitQueueState<Self::Meta, Self::Buffer, Self::Allocator>) -> O,
    >(
        &mut self,
        device_id: &Self::DeviceId,
        cb: F,
    ) -> O;

    /// Calls `cb` with immutable access to the queue state.
    fn with_transmit_queue<
        O,
        F: FnOnce(&TransmitQueueState<Self::Meta, Self::Buffer, Self::Allocator>) -> O,
    >(
        &mut self,
        device_id: &Self::DeviceId,
        cb: F,
    ) -> O;

    /// Send a frame out the device.
    ///
    /// This method may not block - if the device is not ready, an appropriate
    /// error must be returned.
    fn send_frame(
        &mut self,
        bindings_ctx: &mut BC,
        device_id: &Self::DeviceId,
        dequeue_context: Option<&mut Self::DequeueContext>,
        meta: Self::Meta,
        buf: Self::Buffer,
    ) -> Result<(), DeviceSendFrameError>;
}

/// The core execution context for dequeueing TX frames from the transmit queue.
pub trait TransmitDequeueContext<D: Device, BC>: TransmitQueueContext<D, BC> {
    /// The inner context providing dequeuing.
    type TransmitQueueCtx<'a>: TransmitQueueContext<
            D,
            BC,
            Meta = Self::Meta,
            Buffer = Self::Buffer,
            DequeueContext = Self::DequeueContext,
            DeviceId = Self::DeviceId,
        > + DeviceSocketHandler<D, BC>;

    /// Calls the function with the TX deque state and the TX queue context.
    fn with_dequed_packets_and_tx_queue_ctx<
        O,
        F: FnOnce(&mut DequeueState<Self::Meta, Self::Buffer>, &mut Self::TransmitQueueCtx<'_>) -> O,
    >(
        &mut self,
        device_id: &Self::DeviceId,
        cb: F,
    ) -> O;
}

/// The configuration for a transmit queue.
pub enum TransmitQueueConfiguration {
    /// No queue.
    None,
    /// FiFo queue.
    Fifo,
}

/// An implementation of a transmit queue that stores egress frames.
pub trait TransmitQueueHandler<D: Device, BC>: TransmitQueueCommon<D, BC> {
    /// Queues a frame for transmission.
    fn queue_tx_frame<S>(
        &mut self,
        bindings_ctx: &mut BC,
        device_id: &Self::DeviceId,
        meta: Self::Meta,
        body: S,
    ) -> Result<(), TransmitQueueFrameError<S>>
    where
        S: Serializer,
        S::Buffer: ReusableBuffer;
}

pub(super) fn deliver_to_device_sockets<
    D: Device,
    BC: TransmitQueueBindingsContext<CC::DeviceId>,
    CC: TransmitQueueCommon<D, BC> + DeviceSocketHandler<D, BC>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device_id: &CC::DeviceId,
    buffer: &CC::Buffer,
    meta: &CC::Meta,
) {
    let bytes = buffer.as_ref();
    match CC::parse_outgoing_frame(bytes, meta) {
        Ok(sent_frame) => DeviceSocketHandler::handle_frame(
            core_ctx,
            bindings_ctx,
            device_id,
            sent_frame.into(),
            bytes,
        ),
        Err(ParseSentFrameError) => {
            trace!("failed to parse outgoing frame on {:?} ({} bytes)", device_id, bytes.len())
        }
    }
}

impl EnqueueResult {
    fn maybe_wake_tx<D, BC: TransmitQueueBindingsContext<D>>(
        self,
        bindings_ctx: &mut BC,
        device_id: &D,
    ) {
        match self {
            Self::QueuePreviouslyWasOccupied => (),
            Self::QueueWasPreviouslyEmpty => bindings_ctx.wake_tx_task(device_id),
        }
    }
}

enum EnqueueStatus<Meta, Buffer> {
    NotAttempted(Meta, Buffer),
    Attempted,
}

// Extracted to a function without the generic serializer parameter to ease code
// generation.
fn insert_and_notify<
    D: Device,
    BC: TransmitQueueBindingsContext<CC::DeviceId>,
    CC: TransmitQueueContext<D, BC> + DeviceSocketHandler<D, BC>,
>(
    bindings_ctx: &mut BC,
    device_id: &CC::DeviceId,
    inserter: Option<fifo::QueueTxInserter<'_, CC::Meta, CC::Buffer>>,
    meta: CC::Meta,
    body: CC::Buffer,
) -> EnqueueStatus<CC::Meta, CC::Buffer> {
    match inserter {
        // No TX queue so send the frame immediately.
        None => EnqueueStatus::NotAttempted(meta, body),
        Some(inserter) => {
            inserter.insert(meta, body).maybe_wake_tx(bindings_ctx, device_id);
            EnqueueStatus::Attempted
        }
    }
}

// Extracted to a function without the generic serializer parameter to ease code
// generation.
fn handle_post_enqueue<
    D: Device,
    BC: TransmitQueueBindingsContext<CC::DeviceId>,
    CC: TransmitQueueContext<D, BC> + DeviceSocketHandler<D, BC>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device_id: &CC::DeviceId,
    status: EnqueueStatus<CC::Meta, CC::Buffer>,
) -> Result<(), DeviceSendFrameError> {
    match status {
        EnqueueStatus::NotAttempted(meta, body) => {
            // TODO(https://fxbug.dev/42077654): Deliver the frame to packet
            // sockets and to the device atomically.
            deliver_to_device_sockets(core_ctx, bindings_ctx, device_id, &body, &meta);
            // Send the frame while not holding the TX queue exclusively to
            // not block concurrent senders from making progress.
            core_ctx.send_frame(bindings_ctx, device_id, None, meta, body)
        }
        EnqueueStatus::Attempted => Ok(()),
    }
}

impl<
        D: Device,
        BC: TransmitQueueBindingsContext<CC::DeviceId>,
        CC: TransmitQueueContext<D, BC> + DeviceSocketHandler<D, BC>,
    > TransmitQueueHandler<D, BC> for CC
where
    for<'a> &'a mut CC::Allocator: BufferAlloc<CC::Buffer>,
    CC::Buffer: ReusableBuffer,
{
    fn queue_tx_frame<S>(
        &mut self,
        bindings_ctx: &mut BC,
        device_id: &CC::DeviceId,
        meta: CC::Meta,
        body: S,
    ) -> Result<(), TransmitQueueFrameError<S>>
    where
        S: Serializer,
        S::Buffer: ReusableBuffer,
    {
        let result =
            self.with_transmit_queue_mut(device_id, |TransmitQueueState { allocator, queue }| {
                let inserter = match queue {
                    None => None,
                    Some(q) => match q.tx_inserter() {
                        Some(i) => Some(i),
                        None => return Err(TransmitQueueFrameError::QueueFull(body)),
                    },
                };
                let body = body.serialize_outer(NoReuseBufferProvider(allocator)).map_err(
                    |(e, serializer)| {
                        TransmitQueueFrameError::SerializeError(ErrorAndSerializer {
                            serializer,
                            error: e.map_alloc(|_| ()),
                        })
                    },
                )?;
                Ok(insert_and_notify::<_, _, CC>(bindings_ctx, device_id, inserter, meta, body))
            })?;

        handle_post_enqueue(self, bindings_ctx, device_id, result)
            .map_err(TransmitQueueFrameError::NoQueue)
    }
}

/// An allocator of [`Buf<Vec<u8>>`] .
#[derive(Default)]
pub struct BufVecU8Allocator;

impl<'a> BufferAlloc<Buf<Vec<u8>>> for &'a mut BufVecU8Allocator {
    type Error = Never;

    fn alloc(self, len: usize) -> Result<Buf<Vec<u8>>, Self::Error> {
        new_buf_vec(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec;

    use assert_matches::assert_matches;
    use net_declare::net_mac;
    use net_types::ethernet::Mac;
    use netstack3_base::testutil::{
        FakeBindingsCtx, FakeCoreCtx, FakeLinkDevice, FakeLinkDeviceId,
    };
    use netstack3_base::{
        ContextPair, CounterContext, CtxPair, ResourceCounterContext, WorkQueueReport,
    };
    use test_case::test_case;

    use crate::internal::queue::api::TransmitQueueApi;
    use crate::internal::queue::{BatchSize, MAX_TX_QUEUED_LEN};
    use crate::internal::socket::{EthernetFrame, Frame};
    use crate::DeviceCounters;

    #[derive(Default)]
    struct FakeTxQueueState {
        queue: TransmitQueueState<(), Buf<Vec<u8>>, BufVecU8Allocator>,
        transmitted_packets: Vec<(Buf<Vec<u8>>, Option<DequeueContext>)>,
        no_buffers: bool,
        stack_wide_device_counters: DeviceCounters,
        per_device_counters: DeviceCounters,
    }

    #[derive(Default)]
    struct FakeTxQueueBindingsCtxState {
        woken_tx_tasks: Vec<FakeLinkDeviceId>,
        delivered_to_sockets: Vec<Frame<Vec<u8>>>,
    }

    type FakeCoreCtxImpl = FakeCoreCtx<FakeTxQueueState, (), FakeLinkDeviceId>;
    type FakeBindingsCtxImpl = FakeBindingsCtx<(), (), FakeTxQueueBindingsCtxState, ()>;

    impl TransmitQueueBindingsContext<FakeLinkDeviceId> for FakeBindingsCtxImpl {
        fn wake_tx_task(&mut self, device_id: &FakeLinkDeviceId) {
            self.state.woken_tx_tasks.push(device_id.clone())
        }
    }

    const SRC_MAC: Mac = net_mac!("AA:BB:CC:DD:EE:FF");
    const DEST_MAC: Mac = net_mac!("FF:EE:DD:CC:BB:AA");

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    struct DequeueContext;

    impl TransmitQueueCommon<FakeLinkDevice, FakeBindingsCtxImpl> for FakeCoreCtxImpl {
        type DequeueContext = DequeueContext;
        type Meta = ();
        type Buffer = Buf<Vec<u8>>;
        type Allocator = BufVecU8Allocator;

        fn parse_outgoing_frame<'a, 'b>(
            buf: &'a [u8],
            (): &'b Self::Meta,
        ) -> Result<SentFrame<&'a [u8]>, ParseSentFrameError> {
            Ok(fake_sent_ethernet_with_body(buf))
        }
    }

    fn fake_sent_ethernet_with_body<B>(body: B) -> SentFrame<B> {
        SentFrame::Ethernet(EthernetFrame {
            src_mac: SRC_MAC,
            dst_mac: DEST_MAC,
            ethertype: None,
            body,
        })
    }

    /// A trait providing a shortcut to instantiate a [`TransmitQueueApi`] from a context.
    trait TransmitQueueApiExt: ContextPair + Sized {
        fn transmit_queue_api<D>(&mut self) -> TransmitQueueApi<D, &mut Self> {
            TransmitQueueApi::new(self)
        }
    }

    impl<O> TransmitQueueApiExt for O where O: ContextPair + Sized {}

    impl TransmitQueueContext<FakeLinkDevice, FakeBindingsCtxImpl> for FakeCoreCtxImpl {
        fn with_transmit_queue<
            O,
            F: FnOnce(&TransmitQueueState<(), Buf<Vec<u8>>, BufVecU8Allocator>) -> O,
        >(
            &mut self,
            &FakeLinkDeviceId: &FakeLinkDeviceId,
            cb: F,
        ) -> O {
            cb(&self.state.queue)
        }

        fn with_transmit_queue_mut<
            O,
            F: FnOnce(&mut TransmitQueueState<(), Buf<Vec<u8>>, BufVecU8Allocator>) -> O,
        >(
            &mut self,
            &FakeLinkDeviceId: &FakeLinkDeviceId,
            cb: F,
        ) -> O {
            cb(&mut self.state.queue)
        }

        fn send_frame(
            &mut self,
            _bindings_ctx: &mut FakeBindingsCtxImpl,
            &FakeLinkDeviceId: &FakeLinkDeviceId,
            dequeue_context: Option<&mut DequeueContext>,
            (): (),
            buf: Buf<Vec<u8>>,
        ) -> Result<(), DeviceSendFrameError> {
            let FakeTxQueueState { transmitted_packets, no_buffers, .. } = &mut self.state;
            if *no_buffers {
                Err(DeviceSendFrameError::NoBuffers)
            } else {
                Ok(transmitted_packets.push((buf, dequeue_context.map(|c| *c))))
            }
        }
    }

    impl ResourceCounterContext<FakeLinkDeviceId, DeviceCounters> for FakeCoreCtxImpl {
        fn per_resource_counters<'a>(
            &'a self,
            _resource: &'a FakeLinkDeviceId,
        ) -> &'a DeviceCounters {
            &self.state.per_device_counters
        }
    }

    impl CounterContext<DeviceCounters> for FakeCoreCtxImpl {
        fn counters(&self) -> &DeviceCounters {
            &self.state.stack_wide_device_counters
        }
    }

    impl TransmitDequeueContext<FakeLinkDevice, FakeBindingsCtxImpl> for FakeCoreCtxImpl {
        type TransmitQueueCtx<'a> = Self;

        fn with_dequed_packets_and_tx_queue_ctx<
            O,
            F: FnOnce(
                &mut DequeueState<Self::Meta, Self::Buffer>,
                &mut Self::TransmitQueueCtx<'_>,
            ) -> O,
        >(
            &mut self,
            &FakeLinkDeviceId: &FakeLinkDeviceId,
            cb: F,
        ) -> O {
            cb(&mut DequeueState::default(), self)
        }
    }

    impl DeviceSocketHandler<FakeLinkDevice, FakeBindingsCtxImpl> for FakeCoreCtxImpl {
        fn handle_frame(
            &mut self,
            bindings_ctx: &mut FakeBindingsCtxImpl,
            _device: &Self::DeviceId,
            frame: Frame<&[u8]>,
            _whole_frame: &[u8],
        ) {
            bindings_ctx.state.delivered_to_sockets.push(frame.cloned())
        }
    }

    #[test]
    fn noqueue() {
        let mut ctx = CtxPair::with_core_ctx(FakeCoreCtxImpl::default());

        let body = Buf::new(vec![0], ..);

        let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
        assert_eq!(
            TransmitQueueHandler::queue_tx_frame(
                core_ctx,
                bindings_ctx,
                &FakeLinkDeviceId,
                (),
                body.clone(),
            ),
            Ok(())
        );
        let FakeTxQueueBindingsCtxState { woken_tx_tasks, delivered_to_sockets } =
            &bindings_ctx.state;
        assert_matches!(&woken_tx_tasks[..], &[]);
        assert_eq!(
            delivered_to_sockets,
            &[Frame::Sent(fake_sent_ethernet_with_body(body.as_ref().into()))]
        );
        assert_eq!(core::mem::take(&mut core_ctx.state.transmitted_packets), [(body, None)]);

        // Should not have any frames waiting to be transmitted since we have no
        // queue.
        assert_eq!(
            ctx.transmit_queue_api().transmit_queued_frames(
                &FakeLinkDeviceId,
                BatchSize::default(),
                &mut DequeueContext,
            ),
            Ok(WorkQueueReport::AllDone),
        );

        let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
        assert_matches!(&bindings_ctx.state.woken_tx_tasks[..], &[]);
        assert_eq!(core::mem::take(&mut core_ctx.state.transmitted_packets), []);
    }

    #[test_case(BatchSize::MAX)]
    #[test_case(BatchSize::MAX/2)]
    fn fifo_queue_and_dequeue(batch_size: usize) {
        let mut ctx = CtxPair::with_core_ctx(FakeCoreCtxImpl::default());

        ctx.transmit_queue_api()
            .set_configuration(&FakeLinkDeviceId, TransmitQueueConfiguration::Fifo);

        for _ in 0..2 {
            let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
            for i in 0..MAX_TX_QUEUED_LEN {
                let body = Buf::new(vec![i as u8], ..);
                assert_eq!(
                    TransmitQueueHandler::queue_tx_frame(
                        core_ctx,
                        bindings_ctx,
                        &FakeLinkDeviceId,
                        (),
                        body
                    ),
                    Ok(())
                );
                // We should only ever be woken up once when the first packet
                // was enqueued.
                assert_eq!(bindings_ctx.state.woken_tx_tasks, [FakeLinkDeviceId]);
            }

            let body = Buf::new(vec![131], ..);
            assert_eq!(
                TransmitQueueHandler::queue_tx_frame(
                    core_ctx,
                    bindings_ctx,
                    &FakeLinkDeviceId,
                    (),
                    body.clone(),
                ),
                Err(TransmitQueueFrameError::QueueFull(body))
            );

            let FakeTxQueueBindingsCtxState { woken_tx_tasks, delivered_to_sockets } =
                &mut bindings_ctx.state;
            // We should only ever be woken up once when the first packet
            // was enqueued.
            assert_eq!(core::mem::take(woken_tx_tasks), [FakeLinkDeviceId]);
            // No frames should be delivered to packet sockets before transmit.
            assert_eq!(core::mem::take(delivered_to_sockets), &[]);

            assert!(MAX_TX_QUEUED_LEN > batch_size);
            for i in (0..(MAX_TX_QUEUED_LEN - batch_size)).step_by(batch_size) {
                assert_eq!(
                    ctx.transmit_queue_api().transmit_queued_frames(
                        &FakeLinkDeviceId,
                        BatchSize::new_saturating(batch_size),
                        &mut DequeueContext
                    ),
                    Ok(WorkQueueReport::Pending),
                );
                assert_eq!(
                    core::mem::take(&mut ctx.core_ctx.state.transmitted_packets),
                    (i..i + batch_size)
                        .map(|i| (Buf::new(vec![i as u8], ..), Some(DequeueContext)))
                        .collect::<Vec<_>>()
                );
            }

            assert_eq!(
                ctx.transmit_queue_api().transmit_queued_frames(
                    &FakeLinkDeviceId,
                    BatchSize::new_saturating(batch_size),
                    &mut DequeueContext
                ),
                Ok(WorkQueueReport::AllDone),
            );

            let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
            assert_eq!(
                core::mem::take(&mut core_ctx.state.transmitted_packets),
                (batch_size * (MAX_TX_QUEUED_LEN / batch_size - 1)..MAX_TX_QUEUED_LEN)
                    .map(|i| (Buf::new(vec![i as u8], ..), Some(DequeueContext)))
                    .collect::<Vec<_>>()
            );
            // Should not have woken up the TX task since the queue should be
            // empty.
            let FakeTxQueueBindingsCtxState { woken_tx_tasks, delivered_to_sockets } =
                &mut bindings_ctx.state;
            assert_matches!(&core::mem::take(woken_tx_tasks)[..], &[]);

            // The queue should now be empty so the next iteration of queueing
            // `MAX_TX_QUEUED_FRAMES` packets should succeed.
            assert_eq!(
                core::mem::take(delivered_to_sockets),
                (0..MAX_TX_QUEUED_LEN)
                    .map(|i| Frame::Sent(fake_sent_ethernet_with_body(vec![i as u8])))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn dequeue_error() {
        let mut ctx = CtxPair::with_core_ctx(FakeCoreCtxImpl::default());

        ctx.transmit_queue_api()
            .set_configuration(&FakeLinkDeviceId, TransmitQueueConfiguration::Fifo);

        let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
        let body = Buf::new(vec![0], ..);
        assert_eq!(
            TransmitQueueHandler::queue_tx_frame(
                core_ctx,
                bindings_ctx,
                &FakeLinkDeviceId,
                (),
                body.clone(),
            ),
            Ok(())
        );
        assert_eq!(core::mem::take(&mut bindings_ctx.state.woken_tx_tasks), [FakeLinkDeviceId]);
        assert_eq!(core_ctx.state.transmitted_packets, []);

        core_ctx.state.no_buffers = true;
        assert_eq!(
            ctx.transmit_queue_api().transmit_queued_frames(
                &FakeLinkDeviceId,
                BatchSize::default(),
                &mut DequeueContext
            ),
            Err(DeviceSendFrameError::NoBuffers),
        );
        let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
        assert_eq!(core_ctx.state.transmitted_packets, []);
        let FakeTxQueueBindingsCtxState { woken_tx_tasks, delivered_to_sockets } =
            &bindings_ctx.state;
        assert_matches!(&woken_tx_tasks[..], &[]);
        // Frames were delivered to packet sockets before the device was found
        // to not be ready.
        assert_eq!(
            delivered_to_sockets,
            &[Frame::Sent(fake_sent_ethernet_with_body(body.as_ref().into()))]
        );

        core_ctx.state.no_buffers = false;
        assert_eq!(
            ctx.transmit_queue_api().transmit_queued_frames(
                &FakeLinkDeviceId,
                BatchSize::default(),
                &mut DequeueContext
            ),
            Ok(WorkQueueReport::AllDone),
        );
        let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
        assert_matches!(&bindings_ctx.state.woken_tx_tasks[..], &[]);
        // The packet that failed to dequeue is dropped.
        assert_eq!(core::mem::take(&mut core_ctx.state.transmitted_packets), []);
    }

    #[test_case(true; "device no buffers")]
    #[test_case(false; "device has buffers")]
    fn drain_before_noqueue(no_buffers: bool) {
        let mut ctx = CtxPair::with_core_ctx(FakeCoreCtxImpl::default());

        ctx.transmit_queue_api()
            .set_configuration(&FakeLinkDeviceId, TransmitQueueConfiguration::Fifo);

        let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
        let body = Buf::new(vec![0], ..);
        assert_eq!(
            TransmitQueueHandler::queue_tx_frame(
                core_ctx,
                bindings_ctx,
                &FakeLinkDeviceId,
                (),
                body.clone(),
            ),
            Ok(())
        );
        assert_eq!(core::mem::take(&mut bindings_ctx.state.woken_tx_tasks), [FakeLinkDeviceId]);
        assert_eq!(core_ctx.state.transmitted_packets, []);

        core_ctx.state.no_buffers = no_buffers;
        ctx.transmit_queue_api()
            .set_configuration(&FakeLinkDeviceId, TransmitQueueConfiguration::None);

        let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
        let FakeTxQueueBindingsCtxState { woken_tx_tasks, delivered_to_sockets } =
            &bindings_ctx.state;
        assert_matches!(&woken_tx_tasks[..], &[]);
        assert_eq!(
            delivered_to_sockets,
            &[Frame::Sent(fake_sent_ethernet_with_body(body.as_ref().into()))]
        );
        if no_buffers {
            assert_eq!(core_ctx.state.transmitted_packets, []);
        } else {
            assert_eq!(core::mem::take(&mut core_ctx.state.transmitted_packets), [(body, None)]);
        }
    }

    #[test]
    fn count() {
        let mut ctx = CtxPair::with_core_ctx(FakeCoreCtxImpl::default());
        assert_eq!(ctx.transmit_queue_api().count(&FakeLinkDeviceId), None);

        ctx.transmit_queue_api()
            .set_configuration(&FakeLinkDeviceId, TransmitQueueConfiguration::Fifo);

        assert_eq!(ctx.transmit_queue_api().count(&FakeLinkDeviceId), Some(0));

        let CtxPair { core_ctx, bindings_ctx } = &mut ctx;
        let body = Buf::new(vec![0], ..);
        assert_eq!(
            TransmitQueueHandler::queue_tx_frame(
                core_ctx,
                bindings_ctx,
                &FakeLinkDeviceId,
                (),
                body,
            ),
            Ok(())
        );

        assert_eq!(ctx.transmit_queue_api().count(&FakeLinkDeviceId), Some(1));
    }
}

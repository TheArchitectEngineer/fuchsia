// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_BLOCK_DRIVERS_VIRTIO_BLOCK_H_
#define SRC_DEVICES_BLOCK_DRIVERS_VIRTIO_BLOCK_H_

#include <fuchsia/hardware/block/driver/c/banjo.h>
#include <fuchsia/hardware/block/driver/cpp/banjo.h>
#include <lib/dma-buffer/buffer.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/sync/completion.h>
#include <lib/virtio/backends/backend.h>
#include <lib/virtio/device.h>
#include <lib/virtio/ring.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/time.h>
#include <stdlib.h>
#include <zircon/compiler.h>

#include <atomic>
#include <memory>
#include <span>

#include <virtio/block.h>

#include "src/lib/listnode/listnode.h"
#include "src/storage/lib/block_server/block_server.h"

namespace virtio {

// 1MB max transfer (unless further restricted by ring size).
#define MAX_SCATTER 257

struct block_txn_t {
  block_op_t op;
  // Only set for requests coming from the block server
  std::optional<block_server::RequestId> request;
  block_impl_queue_callback completion_cb;
  void* cookie;
  struct vring_desc* desc;
  size_t req_index;
  std::optional<size_t> discard_req_index;  // Only used if op is trim
  list_node_t node;
  zx_handle_t pmt;
};

class Ring;
class BlockDriver;
class BlockDevice : public virtio::Device, public block_server::Interface {
 public:
  BlockDevice(zx::bti bti, std::unique_ptr<Backend> backend, fdf::Logger& logger);

  // virtio::Device overrides
  zx_status_t Init() override;
  void Release() override;

  void IrqRingUpdate() override;
  void IrqConfigChange() override;

  uint32_t GetBlockSize() const { return config_.blk_size; }
  uint64_t GetBlockCount() const { return config_.capacity; }
  uint32_t GetMaxTransferSize() const;
  flag_t GetFlags() const;
  const char* tag() const override { return "virtio-blk"; }

  // ddk::BlockImplProtocol functions invoked by BlockDriver.
  void BlockImplQuery(block_info_t* info, size_t* bopsz);
  void BlockImplQueue(block_op_t* bop, block_impl_queue_callback completion_cb, void* cookie);

  // block_server::Interface
  void StartThread(block_server::Thread) override;
  void OnNewSession(block_server::Session) override;
  void OnRequests(std::span<block_server::Request>) override;
  void Log(std::string_view msg) const override {
    FDF_LOGL(INFO, logger(), "%.*s", static_cast<int>(msg.size()), msg.data());
  }

  void ServeRequests(fidl::ServerEnd<fuchsia_hardware_block_volume::Volume>);

 private:
  static constexpr uint16_t kRingSize = 128;  // 128 matches legacy pci.

  // A queue of block request/responses.
  static constexpr size_t kBlkReqCount = 32;

  static constexpr zx::duration kWatchdogInterval = zx::sec(10);

  // An object which bundles all of the resources needed to enqueue a transaction.
  // Once a transaction is successfully enqueued, RequestContext::Release() must be called, since
  // the resources are logically owned by the transaction.
  // If the RequestContext needs to be prematurely destroyed, BlockDevice::FreeResources() must be
  // called.
  class RequestContext {
   public:
    RequestContext() = default;
    RequestContext(RequestContext&&);
    RequestContext& operator=(RequestContext&&);
    RequestContext(const RequestContext&) = delete;
    RequestContext& operator=(const RequestContext&) = delete;
    ~RequestContext();

    // Explicitly acknowledge that the resources have been released by enqueueing a transaction.
    void Release();

    void set_req_index(size_t value) { req_index_ = value; }
    void set_discard_req_index(size_t value) { discard_req_index_ = value; }
    void set_desc(struct vring_desc* desc, uint16_t desc_index) {
      desc_ = desc;
      desc_index_ = desc_index;
    }
    void set_pages(zx_paddr_t* pages, size_t num_pages) {
      pages_ = pages;
      num_pages_ = num_pages;
    }
    void set_pmt(zx_handle_t value) { pmt_ = value; }

    size_t req_index() const { return req_index_; }
    const std::optional<size_t>& discard_req_index() const { return discard_req_index_; }
    uint16_t desc_index() const { return desc_index_; }
    struct vring_desc* desc() { return desc_; }
    zx_paddr_t* pages() { return pages_; }
    size_t num_pages() const { return num_pages_; }
    zx_handle_t pmt() const { return pmt_; }

   private:
    bool released_ = false;
    size_t req_index_ = kBlkReqCount;
    std::optional<size_t> discard_req_index_ = std::nullopt;
    uint16_t desc_index_ = UINT16_MAX;
    struct vring_desc* desc_ = nullptr;
    zx_paddr_t* pages_ = nullptr;
    size_t num_pages_ = 0;
    zx_handle_t pmt_ = ZX_HANDLE_INVALID;
  };

  void FreeRequestContext(RequestContext& context) __TA_REQUIRES(txn_lock_, ring_lock_);

  // Submit a transaction to the worker queue.  Only to be called for Banjo requests.
  void SignalWorker(block_txn_t*);
  void WorkerThread();
  void WatchdogThread();
  void FlushPendingTxns();
  void CleanupPendingTxns();

  // Blocks until the request has been submitted to virtio, which may require waiting until another
  // request completes.
  zx::result<> SubmitBanjoRequest(block_txn_t*);
  zx::result<> SubmitBlockServerRequest(const block_server::Request& request);

  zx::result<> FlushSync(const block_server::Request& request);

  // Allocates all of the resources necessary for enqueueing a virtio request.  Blocks until
  // resources are available.
  // The RequestContext object will point at `pages`, so must not outlive it.
  zx::result<RequestContext> AllocateRequestContext(uint32_t type, zx_handle_t vmo,
                                                    uint64_t vmo_offset_blocks, uint32_t num_blocks,
                                                    std::array<zx_paddr_t, MAX_SCATTER>* pages);

  // Returns std::nullopt if there are too many pending transactions; the caller should block on
  // txn_cond_ and try again.
  zx::result<std::optional<RequestContext>> TryAllocateRequestContextLocked(
      uint32_t type, zx_handle_t vmo, uint64_t vmo_offset_blocks, uint32_t num_blocks,
      std::array<zx_paddr_t, MAX_SCATTER>* pages) __TA_REQUIRES(txn_lock_, ring_lock_);

  zx::result<zx_handle_t> PinPages(zx_handle_t bti, zx_handle_t vmo, uint64_t vmo_offset_blocks,
                                   uint32_t num_blocks, std::array<zx_paddr_t, MAX_SCATTER>* pages,
                                   size_t* num_pages) const;

  // Enqueues the transaction to virtio.  Transfers ownership of `resources` into `txn`.
  void QueueTxn(block_txn_t* txn, RequestContext context);

  void CompleteTxn(block_txn_t*, zx_status_t status);

  // Frees the chain at `index`, returning the pointer of the head descriptor, which should only be
  // used for freeing related resources (e.g. completing a transaction pointing to the descriptor).
  struct vring_desc* FreeDescChainLocked(uint16_t index) __TA_REQUIRES(ring_lock_);

  fdf::Logger& logger() const { return logger_; }

  // The main virtio ring.
  Ring vring_{this};

  // Lock to be used around Ring::AllocDescChain and FreeDesc.
  // Lock ordering: Must be locked *after* txn_lock_.
  // TODO: Move this into Ring class once it's certain that other users of the class are okay with
  // it.
  std::mutex ring_lock_;

  // Saved block device configuration out of the pci config BAR.
  virtio_blk_config_t config_ = {};

  std::unique_ptr<dma_buffer::ContiguousBuffer> blk_req_buf_;
  virtio_blk_req_t* blk_req_ = nullptr;

  zx_paddr_t blk_res_pa_ = 0;
  uint8_t* blk_res_ = nullptr;

  uint32_t blk_req_bitmap_ __TA_GUARDED(txn_lock_) = 0;
  static_assert(kBlkReqCount <= sizeof(blk_req_bitmap_) * CHAR_BIT, "");

  std::optional<size_t> AllocateBlkReqLocked() __TA_REQUIRES(txn_lock_) {
    size_t i = 0;
    if (blk_req_bitmap_ != 0) {
      i = sizeof(blk_req_bitmap_) * CHAR_BIT - __builtin_clz(blk_req_bitmap_);
    }
    if (i < kBlkReqCount) {
      blk_req_bitmap_ |= (1 << i);
      return i;
    }
    return std::nullopt;
  }

  void FreeBlkReqLocked(size_t i) __TA_REQUIRES(txn_lock_) {
    if (i < kBlkReqCount) {
      blk_req_bitmap_ &= static_cast<uint32_t>(~(1 << i));
    }
  }

  // Requests originating from the block server are allocated out of this pool.  The req_index
  // allocated by AllocateRequestContext is used as the index.
  block_txn_t block_server_request_pool_[kBlkReqCount];

  // When a transaction is enqueued, its start time (in the monotonic clock) is recorded, and the
  // timestamp is cleared when the transaction completes.  A watchdog task will fire after a
  // configured interval, and all timestamps will be checked against a deadline; if any exceed the
  // deadline an error is logged.
  std::mutex watchdog_lock_;
  zx::time blk_req_start_timestamps_[kBlkReqCount] __TA_GUARDED(watchdog_lock_);

  // When the server is shut down (or has yet to start; see Init()), we set block_server_ to
  // std::nullopt.
  std::mutex block_server_lock_;
  std::optional<block_server::BlockServer> block_server_ __TA_GUARDED(block_server_lock_);

  // # Request flow
  //
  // Requests follow a slightly different path depending on whether they originate from the legacy
  // Banjo interface, or the block_server interface.
  //
  // - Banjo requests arrive in BlockDevice::BlockImplQueue as a block_txn_t.  They have already
  //   been allocated by the caller.
  //   - The txn is placed into the worker_txn_list_, and the worker thread is signaled via
  //     worker_signal_.
  //   - The worker thread loop (WorkerThread) pops the next transaction and calls
  //     SubmitBanjoRequest.
  //   - SubmitBanjoRequest calls AllocateRequestContext (which blocks until resources like
  //     descriptors and request indices are available) and then QueueTxn.
  //   - QueueTxn populates the virtio_blk_req_t, sets up descriptors, adds the txn to
  //     pending_txn_list_, and submits to the virtio ring.
  // - Block server requests arrive in BlockDevice::OnRequests as a block_server::Request reference.
  //   - For each request, SubmitBlockServerRequest is called.
  //   - SubmitBlockServerRequest calls AllocateRequestContext (same blocking behavior as above).
  //   - A block_txn_t is allocated from the block_server_request_pool_ using the obtained
  //     req_index.  From here, QueueTxn is called (same behaviour as above).

  // Pending txns which have been submitted to virtio, and completion signal.
  // List entries are PendingTransaction.
  std::mutex txn_lock_;
  list_node pending_txn_list_ TA_GUARDED(txn_lock_) = LIST_INITIAL_VALUE(pending_txn_list_);
  std::condition_variable txn_cond_;

  thrd_t worker_thread_;

  // Worker state for requests originating from Banjo.
  // TODO(https://fxbug.dev/394968352): Remove once all clients use Volume service provided by
  // block_server_.
  // `worker_txn_list_` contains incoming requests (block_txn_t).  Entries are processed by
  // `worker_thread_`, submitted to virtio, and moved into `pending_transaction_list_`.
  list_node worker_txn_list_ TA_GUARDED(lock_) = LIST_INITIAL_VALUE(worker_txn_list_);
  sync_completion_t worker_signal_;
  std::atomic_bool worker_shutdown_ = false;

  thrd_t watchdog_thread_;
  sync_completion_t watchdog_signal_;
  std::atomic_bool watchdog_shutdown_ = false;

  bool supports_discard_ = false;
  bool supports_barriers_ = false;

  fdf::Logger& logger_;
};

class BlockDriver : public fdf::DriverBase, public ddk::BlockImplProtocol<BlockDriver> {
 public:
  static constexpr char kDriverName[] = "virtio-block";

  BlockDriver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : fdf::DriverBase(kDriverName, std::move(start_args), std::move(dispatcher)) {}

  zx::result<> Start() override;

  void PrepareStop(fdf::PrepareStopCompleter completer) override;

  // ddk::BlockImplProtocol functions passed through to BlockDevice.
  void BlockImplQuery(block_info_t* info, size_t* bopsz);
  void BlockImplQueue(block_op_t* bop, block_impl_queue_callback completion_cb, void* cookie);

 protected:
  // Override to inject dependency for unit testing.
  virtual zx::result<std::unique_ptr<BlockDevice>> CreateBlockDevice();
  // Exposed for testing
  BlockDevice& block_device() const { return *block_device_; }

 private:
  std::unique_ptr<BlockDevice> block_device_;

  fidl::WireSyncClient<fuchsia_driver_framework::Node> parent_node_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> node_controller_;

  // Legacy DFv1-based protocols.
  // TODO(https://fxbug.dev/394968352): Remove once all clients use Volume service provided by
  // block_server_.
  compat::BanjoServer block_impl_server_{ZX_PROTOCOL_BLOCK_IMPL, this, &block_impl_protocol_ops_};
  compat::SyncInitializedDeviceServer compat_server_;
};

}  // namespace virtio

#endif  // SRC_DEVICES_BLOCK_DRIVERS_VIRTIO_BLOCK_H_

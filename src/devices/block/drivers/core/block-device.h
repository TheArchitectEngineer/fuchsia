// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_CORE_BLOCK_DEVICE_H_
#define SRC_DEVICES_BLOCK_DRIVERS_CORE_BLOCK_DEVICE_H_

#include <assert.h>
#include <fidl/fuchsia.hardware.block.partition/cpp/wire.h>
#include <fidl/fuchsia.hardware.block.volume/cpp/wire.h>
#include <fidl/fuchsia.hardware.block/cpp/wire.h>
#include <fuchsia/hardware/block/driver/cpp/banjo.h>
#include <fuchsia/hardware/block/partition/cpp/banjo.h>
#include <fuchsia/hardware/block/volume/cpp/banjo.h>
#include <inttypes.h>
#include <lib/ddk/binding_driver.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/ddk/metadata.h>
#include <lib/operation/block.h>
#include <lib/zbi-format/zbi.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/fifo.h>
#include <lib/zx/vmo.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/param.h>
#include <threads.h>
#include <zircon/process.h>

#include <algorithm>
#include <limits>
#include <list>
#include <mutex>
#include <new>

#include <ddktl/device.h>

// To maintain stats related to time taken by a command or its success/failure, we need to
// intercept command completion with a callback routine. This might introduce memory
// overhead.
// TODO(https://fxbug.dev/42072576): We should be able to turn on/off stats either at compile-time
// or load-time.
struct StatsCookie {
  zx::ticks start_tick;
};

class BlockDevice;
using BlockDeviceType = ddk::Device<BlockDevice, ddk::GetProtocolable,
                                    ddk::Messageable<fuchsia_hardware_block_volume::Volume>::Mixin>;

class BlockDevice : public BlockDeviceType,
                    public ddk::BlockProtocol<BlockDevice, ddk::base_protocol> {
 public:
  explicit BlockDevice(zx_device_t* parent)
      : BlockDeviceType(parent),
        parent_protocol_(parent),
        parent_partition_protocol_(parent),
        parent_volume_protocol_(parent) {
    block_protocol_t self{&block_protocol_ops_, this};
    self_protocol_ = ddk::BlockProtocolClient(&self);
  }

  static zx_status_t Bind(void* ctx, zx_device_t* dev);

  void DdkRelease();

  // ddk::GetProtocolable
  zx_status_t DdkGetProtocol(uint32_t proto_id, void* out_protocol);

  // ddk::BlockProtocol
  void BlockQuery(block_info_t* block_info, size_t* op_size);
  void BlockQueue(block_op_t* op, block_impl_queue_callback completion_cb, void* cookie);

  // fuchsia_hardware_block_volume::Volume
  void GetInfo(GetInfoCompleter::Sync& completer) override;
  void OpenSession(OpenSessionRequestView request, OpenSessionCompleter::Sync& completer) override;
  void OpenSessionWithOffsetMap(OpenSessionWithOffsetMapRequestView request,
                                OpenSessionWithOffsetMapCompleter::Sync& completer) override;

  void GetTypeGuid(GetTypeGuidCompleter::Sync& completer) override;
  void GetInstanceGuid(GetInstanceGuidCompleter::Sync& completer) override;
  void GetName(GetNameCompleter::Sync& completer) override;
  void GetMetadata(GetMetadataCompleter::Sync& completer) override;

  void QuerySlices(QuerySlicesRequestView request, QuerySlicesCompleter::Sync& completer) override;
  void GetVolumeInfo(GetVolumeInfoCompleter::Sync& completer) override;
  void Extend(ExtendRequestView request, ExtendCompleter::Sync& completer) override;
  void Shrink(ShrinkRequestView request, ShrinkCompleter::Sync& completer) override;
  void Destroy(DestroyCompleter::Sync& completer) override;

 private:
  zx_status_t DoIo(zx::vmo& vmo, size_t buf_len, zx_off_t off, zx_off_t vmo_off, bool write);

  void CreateSession(
      fidl::ServerEnd<fuchsia_hardware_block::Session> session,
      std::optional<fuchsia_hardware_block::wire::BlockOffsetMapping> mapping = std::nullopt);

  // Completion callback that expects StatsCookie as |cookie| and calls upper
  // layer completion cookie.
  static void UpdateStatsAndCallCompletion(void* cookie, zx_status_t status, block_op_t* op);
  void UpdateStats(bool success, zx::ticks start_tick, block_op_t* op);

  // The block protocol of the device we are binding against.
  ddk::BlockImplProtocolClient parent_protocol_;
  // An optional partition protocol, if supported by the parent device.
  ddk::BlockPartitionProtocolClient parent_partition_protocol_;
  // An optional volume protocol, if supported by the parent device.
  ddk::BlockVolumeProtocolClient parent_volume_protocol_;
  // The block protocol for ourselves, which redirects to the parent protocol,
  // but may also collect auxiliary information like statistics.
  ddk::BlockProtocolClient self_protocol_;
  block_info_t info_ = {};

  // parent device's op size
  size_t parent_op_size_ = 0;

  // True if we have metadata for a ZBI partition map.
  bool has_bootpart_ = false;

  std::mutex io_lock_;
  zx::vmo io_vmo_ TA_GUARDED(io_lock_);
  zx_status_t io_status_ = ZX_OK;
  sync_completion_t io_signal_;
  std::unique_ptr<uint8_t[]> io_op_;
};

#endif  // SRC_DEVICES_BLOCK_DRIVERS_CORE_BLOCK_DEVICE_H_

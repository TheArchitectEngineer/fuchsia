// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "internal_buffer.h"

#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/fpromise/result.h>
#include <lib/memory_barriers/memory_barriers.h>
#include <lib/sysmem-version/sysmem-version.h>
#include <threads.h>

#include <limits>

#include <bind/fuchsia/amlogic/platform/sysmem/heap/cpp/bind.h>
#include <bind/fuchsia/sysmem/heap/cpp/bind.h>
#include <fbl/algorithm.h>

fpromise::result<InternalBuffer, zx_status_t> InternalBuffer::Create(
    const char* name, fidl::SyncClient<fuchsia_sysmem2::Allocator>* sysmem,
    const zx::unowned_bti& bti, size_t size, bool is_secure, bool is_writable,
    bool is_mapping_needed) {
  return CreateAligned(name, sysmem, bti, size, 0, is_secure, is_writable, is_mapping_needed);
}

fpromise::result<InternalBuffer, zx_status_t> InternalBuffer::CreateAligned(
    const char* name, fidl::SyncClient<fuchsia_sysmem2::Allocator>* sysmem,
    const zx::unowned_bti& bti, size_t size, size_t alignment, bool is_secure, bool is_writable,
    bool is_mapping_needed) {
  ZX_DEBUG_ASSERT(sysmem);
  ZX_DEBUG_ASSERT(*sysmem);
  ZX_DEBUG_ASSERT(*bti);
  ZX_DEBUG_ASSERT(size);
  ZX_DEBUG_ASSERT(size % ZX_PAGE_SIZE == 0);
  ZX_DEBUG_ASSERT(!is_mapping_needed || !is_secure);
  InternalBuffer local_result(size, is_secure, is_writable, is_mapping_needed);
  zx_status_t status = local_result.Init(name, sysmem, alignment, bti);
  if (status != ZX_OK) {
    fprintf(stderr, "Init() failed status=%i\n", status);
    return fpromise::error(status);
  }
  return fpromise::ok(std::move(local_result));
}

InternalBuffer::~InternalBuffer() { DeInit(); }

InternalBuffer::InternalBuffer(InternalBuffer&& other)
    : size_(other.size_),
      alignment_(other.alignment_),
      is_secure_(other.is_secure_),
      is_writable_(other.is_writable_),
      is_mapping_needed_(other.is_mapping_needed_),
      virt_base_(other.virt_base_),
      real_size_(other.real_size_),
      real_virt_base_(other.real_virt_base_),
      alignment_offset_(other.alignment_offset_),
      pin_(std::move(other.pin_)),
      phys_base_(other.phys_base_),
      buffer_collection_(std::move(other.buffer_collection_)),
      vmo_(std::move(other.vmo_)) {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  other.is_moved_out_ = true;
  ZX_ASSERT(!other.pin_);
  check_pin_ = [this] { ZX_ASSERT(!pin_); };
}

InternalBuffer& InternalBuffer::operator=(InternalBuffer&& other) {
  // Let's just use a new variable instead of letting this happen, even though this isn't prevented
  // by C++ rules.
  ZX_ASSERT(!is_moved_out_);
  ZX_ASSERT(!other.is_moved_out_);
  DeInit();
  ZX_ASSERT(!pin_);
  size_ = other.size_;
  alignment_ = other.alignment_;
  is_secure_ = other.is_secure_;
  is_writable_ = other.is_writable_;
  is_mapping_needed_ = other.is_mapping_needed_;
  // Let's only move instances that returned success from Init() and haven't been moved out.
  ZX_ASSERT(other.pin_ && !other.is_moved_out_);
  pin_ = std::move(other.pin_);
  virt_base_ = other.virt_base_;
  real_size_ = other.real_size_;
  real_virt_base_ = other.real_virt_base_;
  alignment_offset_ = other.alignment_offset_;
  phys_base_ = other.phys_base_;
  buffer_collection_ = std::move(other.buffer_collection_);
  vmo_ = std::move(other.vmo_);
  other.is_moved_out_ = true;
  return *this;
}

uint8_t* InternalBuffer::virt_base() {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  ZX_DEBUG_ASSERT(is_mapping_needed_);
  return virt_base_;
}

zx_paddr_t InternalBuffer::phys_base() {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  ZX_DEBUG_ASSERT(pin_);
  return phys_base_;
}

size_t InternalBuffer::size() {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  ZX_DEBUG_ASSERT(pin_);
  return size_;
}

size_t InternalBuffer::alignment() {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  ZX_DEBUG_ASSERT(pin_);
  return alignment_;
}

bool InternalBuffer::is_secure() {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  ZX_DEBUG_ASSERT(pin_);
  return is_secure_;
}

bool InternalBuffer::is_writable() {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  ZX_DEBUG_ASSERT(pin_);
  return is_writable_;
}

bool InternalBuffer::is_mapping_needed() {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  ZX_DEBUG_ASSERT(pin_);
  return is_mapping_needed_;
}

const zx::vmo& InternalBuffer::vmo() {
  ZX_DEBUG_ASSERT(vmo_);
  return vmo_;
}

void InternalBuffer::CacheFlush(size_t offset, size_t length) {
  CacheFlushPossibleInvalidate(offset, length, false);
}

void InternalBuffer::CacheFlushInvalidate(size_t offset, size_t length) {
  CacheFlushPossibleInvalidate(offset, length, true);
}

void InternalBuffer::CacheFlushPossibleInvalidate(size_t offset, size_t length, bool invalidate) {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  ZX_DEBUG_ASSERT(offset <= size());
  ZX_DEBUG_ASSERT(offset + length >= offset);
  ZX_DEBUG_ASSERT(offset + length <= size());
  ZX_DEBUG_ASSERT(vmo_);
  zx_status_t status;
  if (is_secure_) {
    return;
  }
  if (invalidate) {
    BarrierBeforeInvalidate();
  }
  if (is_mapping_needed_) {
    ZX_DEBUG_ASSERT(virt_base_);
    status = zx_cache_flush(virt_base_ + offset, length,
                            ZX_CACHE_FLUSH_DATA | (invalidate ? ZX_CACHE_FLUSH_INVALIDATE : 0));
    if (status != ZX_OK) {
      ZX_PANIC("InternalBuffer::CacheFlush() zx_cache_flush() failed: %d", status);
    }
  } else {
    status = vmo_.op_range(ZX_VMO_OP_CACHE_CLEAN, alignment_offset_ + offset, length, nullptr, 0);
    if (status != ZX_OK) {
      ZX_PANIC("InternalBuffer::CacheFlush() op_range(CACHE_CLEAN) failed: %d", status);
    }
  }
  BarrierAfterFlush();
}

InternalBuffer::InternalBuffer(size_t size, bool is_secure, bool is_writable,
                               bool is_mapping_needed)
    : size_(size),
      is_secure_(is_secure),
      is_writable_(is_writable),
      is_mapping_needed_(is_mapping_needed) {
  ZX_DEBUG_ASSERT(size_);
  ZX_DEBUG_ASSERT(size_ % ZX_PAGE_SIZE == 0);
  ZX_ASSERT(!pin_);
  ZX_DEBUG_ASSERT(!is_moved_out_);
  ZX_DEBUG_ASSERT(!is_mapping_needed_ || !is_secure_);
  check_pin_ = [this] { ZX_ASSERT(!pin_); };
}

zx_status_t InternalBuffer::Init(const char* name,
                                 fidl::SyncClient<fuchsia_sysmem2::Allocator>* sysmem,
                                 size_t alignment, const zx::unowned_bti& bti) {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  // Init() should only be called on newly-constructed instances using a constructor other than the
  // move constructor.
  ZX_ASSERT(!pin_);

  // Let's interact with BufferCollection sync, since we're the only participant.
  auto collection_endpoints = fidl::CreateEndpoints<fuchsia_sysmem2::BufferCollection>();
  ZX_ASSERT(collection_endpoints.is_ok());
  fuchsia_sysmem2::AllocatorAllocateNonSharedCollectionRequest non_shared_request;
  non_shared_request.collection_request() = std::move(collection_endpoints->server);
  // discard result; deal with potential wait failed below instead
  (void)(*sysmem)->AllocateNonSharedCollection(std::move(non_shared_request));
  auto buffer_collection = fidl::SyncClient(std::move(collection_endpoints->client));

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  auto& usage = constraints.usage().emplace();
  usage.video() = fuchsia_sysmem2::kVideoUsageHwDecoderInternal;
  // we only want one buffer
  constraints.min_buffer_count_for_camping() = 1;
  constraints.max_buffer_count() = 1;

  // Allocate enough so that some portion must be aligned and large enough.
  alignment_ = alignment;
  real_size_ = size_ + alignment;
  ZX_DEBUG_ASSERT(real_size_ < std::numeric_limits<uint32_t>::max());
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = static_cast<uint32_t>(real_size_);
  bmc.max_size_bytes() = static_cast<uint32_t>(real_size_);
  // amlogic-video always requires contiguous; only contiguous is supported by InternalBuffer.
  bmc.physically_contiguous_required() = true;
  bmc.secure_required() = is_secure_;
  // If we need a mapping, then we don't want INACCESSIBLE domain, so we need to support at least
  // one other domain.  We choose RAM domain since InternalBuffer(s) are always used for HW DMA, and
  // we always have to CachFlush() after any write, or CacheInvalidate() before any read.  So RAM
  // domain is a better fit than CPU domain, even though we're not really sharing with any other
  // participant so the choice is less critical here.
  bmc.cpu_domain_supported() = false;
  bmc.ram_domain_supported() = is_mapping_needed_;
  // Secure buffers need support for INACCESSIBLE, and it's fine to indicate support for
  // INACCESSIBLE as long as we don't need to map, but when is_mapping_needed_ we shouldn't accept
  // INACCESSIBLE.
  //
  // Nothing presently technically stops us from mapping a buffer that's INACCESSIBLE, because MAP
  // and PIN are the same right and sysmem assumes PIN will be needed so always grants MAP, but if
  // the rights were separated, we'd potentially want to exclude MAP unless CPU/RAM domain in
  // sysmem.
  bmc.inaccessible_domain_supported() = !is_mapping_needed_;

  if (is_secure_) {
    // AMLOGIC_SECURE_VDEC is only ever allocated for input/output buffers, never for internal
    // buffers.  This is "normal" non-VDEC secure memory.  See also secmem TA's ProtectMemory /
    // sysmem.
    bmc.permitted_heaps() = {
        sysmem::MakeHeap(bind_fuchsia_amlogic_platform_sysmem_heap::HEAP_TYPE_SECURE, 0)};
  } else {
    bmc.permitted_heaps() = {sysmem::MakeHeap(bind_fuchsia_sysmem_heap::HEAP_TYPE_SYSTEM_RAM, 0)};
  }

  // InternalBuffer(s) don't need any image format constraints, as they don't store image data.
  ZX_DEBUG_ASSERT(!constraints.image_format_constraints().has_value());

  fuchsia_sysmem2::NodeSetNameRequest set_name_request;
  set_name_request.priority() = 10u;
  set_name_request.name() = name;
  // discard result; deal with potential wait failed below instead
  (void)buffer_collection->SetName(std::move(set_name_request));

  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  // discard result; deal with potential wait failed below instead
  (void)buffer_collection->SetConstraints(std::move(set_constraints_request));

  // There's only one participant, and we've already called SetConstraints(), so this should be
  // quick.
  auto wait_result = buffer_collection->WaitForAllBuffersAllocated();
  if (wait_result.is_error()) {
    zx_status_t status;
    if (wait_result.error_value().is_framework_error()) {
      status = wait_result.error_value().framework_error().status();
      fprintf(stderr, "WaitForBuffersAllocated() failed status=%d\n", status);
    } else {
      status = sysmem::V1CopyFromV2Error(wait_result.error_value().domain_error());
      fprintf(stderr, "WaitForBuffersAllocated() failed error=%u status=%d\n",
              fidl::ToUnderlying(wait_result.error_value().domain_error()), status);
    }
    return status;
  }
  auto out_buffer_collection_info = std::move(*wait_result->buffer_collection_info());

  if (!!is_secure_ != !!*out_buffer_collection_info.settings()->buffer_settings()->is_secure()) {
    fprintf(stderr, "sysmem bug?\n");
    return ZX_ERR_INTERNAL;
  }

  ZX_DEBUG_ASSERT(
      out_buffer_collection_info.buffers()->at(0).vmo_usable_start().value() % ZX_PAGE_SIZE == 0);
  zx::vmo vmo = std::move(*out_buffer_collection_info.buffers()->at(0).vmo());

  uintptr_t virt_base = 0;
  if (is_mapping_needed_) {
    zx_vm_option_t map_options = ZX_VM_PERM_READ;
    if (is_writable_) {
      map_options |= ZX_VM_PERM_WRITE;
    }

    zx_status_t status = zx::vmar::root_self()->map(map_options, /*vmar_offset=*/0, vmo,
                                                    /*vmo_offset=*/0, real_size_, &virt_base);
    if (status != ZX_OK) {
      fprintf(stderr, "zx::vmar::root_self()->map() failed status=%i\n", status);
      return status;
    }
  }

  uint32_t pin_options = ZX_BTI_CONTIGUOUS | ZX_BTI_PERM_READ;
  if (is_writable_) {
    pin_options |= ZX_BTI_PERM_WRITE;
  }

  zx_paddr_t phys_base;
  zx::pmt pin;
  zx_status_t status =
      bti->pin(pin_options, vmo, *out_buffer_collection_info.buffers()->at(0).vmo_usable_start(),
               real_size_, &phys_base, 1, &pin);
  if (status != ZX_OK) {
    fprintf(stderr, "BTI pin() failed status=%i\n", status);
    return status;
  }

  virt_base_ = reinterpret_cast<uint8_t*>(virt_base);
  real_virt_base_ = virt_base_;
  phys_base_ = phys_base;
  if (alignment) {
    // Shift the base addresses so the physical address is aligned correctly.
    zx_paddr_t new_phys_base = fbl::round_up(phys_base, alignment);
    alignment_offset_ = new_phys_base - phys_base;
    if (is_mapping_needed_) {
      virt_base_ += alignment_offset_;
    }
    phys_base_ = new_phys_base;
  }
  pin_ = std::move(pin);
  ZX_ASSERT(!pin);
  ZX_ASSERT(pin_);
  // We keep the buffer_collection_ channel alive, but we don't listen for channel failure.  This
  // isn't ideal, since we should listen for channel failure so that sysmem can request that we
  // close the VMO handle ASAP, but so far sysmem won't try to force relinquishing buffers anyway,
  // so ... it's acceptable for now.  We keep the channel open for the lifetime of the
  // InternalBuffer so this won't look like a buffer that's pending deletion in sysmem.
  buffer_collection_ = std::move(buffer_collection);
  vmo_ = std::move(vmo);

  // Sysmem guarantees that the newly-allocated buffer starts out zeroed and cache clean, to the
  // extent possible based on is_secure.

  return ZX_OK;
}

void InternalBuffer::DeInit() {
  if (is_moved_out_) {
    ZX_ASSERT(!pin_);
    return;
  }
  if (pin_) {
    pin_.unpin();
    ZX_ASSERT(!pin_);
  }
  if (virt_base_) {
    zx_status_t status =
        zx::vmar::root_self()->unmap(reinterpret_cast<uintptr_t>(real_virt_base_), real_size_);
    // Unmap is expected to work unless there's a bug in how we're calling it.
    ZX_ASSERT(status == ZX_OK);
    virt_base_ = nullptr;
    real_virt_base_ = nullptr;
  }
}

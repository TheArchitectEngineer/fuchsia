// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/tests/utils/screen_capture_utils.h"

#include <fbl/algorithm.h>
#include <zxtest/zxtest.h>

#include "src/ui/scenic/lib/flatland/buffers/util.h"
#include "src/ui/scenic/lib/utils/helpers.h"

namespace integration_tests {
using flatland::MapHostPointer;
using fuchsia::ui::composition::RegisterBufferCollectionArgs;
using fuchsia::ui::composition::RegisterBufferCollectionUsages;
using fuchsia::ui::composition::TransformId;

bool PixelEquals(const uint8_t* a, const uint8_t* b) { return memcmp(a, b, kBytesPerPixel) == 0; }

void AppendPixel(std::vector<uint8_t>* values, const uint8_t* pixel) {
  values->insert(values->end(), pixel, pixel + kBytesPerPixel);
}

void GenerateImageForFlatlandInstance(uint32_t buffer_collection_index,
                                      fuchsia::ui::composition::FlatlandPtr& flatland,
                                      TransformId parent_transform,
                                      allocation::BufferCollectionImportToken import_token,
                                      SizeU size, Vec translation, uint32_t image_id,
                                      uint32_t transform_id) {
  // Create the image in the Flatland instance.
  fuchsia::ui::composition::ImageProperties image_properties = {};
  image_properties.set_size(size);
  fuchsia::ui::composition::ContentId content_id = {.value = image_id};
  flatland->CreateImage(content_id, std::move(import_token), buffer_collection_index,
                        std::move(image_properties));

  // Add the created image as a child of the parent transform specified. Apply the right size and
  // orientation commands.
  const TransformId kTransform{.value = transform_id};
  flatland->CreateTransform(kTransform);

  flatland->SetContent(kTransform, content_id);
  flatland->SetImageDestinationSize(content_id, {size.width, size.height});
  flatland->SetTranslation(kTransform, translation);

  flatland->AddChild(parent_transform, kTransform);
}

// This method writes to a sysmem buffer, taking into account any potential stride width
// differences. The method also flushes the cache if the buffer is in RAM domain.
void WriteToSysmemBuffer(const std::vector<uint8_t>& write_values,
                         fuchsia::sysmem2::BufferCollectionInfo& buffer_collection_info,
                         uint32_t buffer_collection_idx, uint32_t kBytesPerPixel,
                         uint32_t image_width, uint32_t image_height) {
  FX_DCHECK(kBytesPerPixel == utils::GetBytesPerPixel(buffer_collection_info.settings()));
  uint32_t pixels_per_row = utils::GetPixelsPerRow(buffer_collection_info.settings(), image_width);
  // Flush the cache if we are operating in RAM.
  const bool need_flush = buffer_collection_info.settings().buffer_settings().coherency_domain() ==
                          fuchsia::sysmem2::CoherencyDomain::RAM;

  MapHostPointer(
      buffer_collection_info, buffer_collection_idx, flatland::HostPointerAccessMode::kReadWrite,
      [&write_values, pixels_per_row, kBytesPerPixel, image_width, image_height, need_flush](
          uint8_t* vmo_host, uint32_t num_bytes) {
        uint32_t bytes_per_row = pixels_per_row * kBytesPerPixel;
        uint32_t valid_bytes_per_row = image_width * kBytesPerPixel;

        EXPECT_GE(bytes_per_row, valid_bytes_per_row);
        EXPECT_GE(num_bytes, bytes_per_row * image_height);

        if (bytes_per_row == valid_bytes_per_row) {
          // Fast path.
          memcpy(vmo_host, write_values.data(), write_values.size());
          if (need_flush) {
            EXPECT_EQ(ZX_OK, zx_cache_flush(vmo_host, write_values.size(), ZX_CACHE_FLUSH_DATA));
          }
        } else {
          // Copy over row-by-row.
          for (size_t i = 0; i < image_height; ++i) {
            memcpy(&vmo_host[i * bytes_per_row], &write_values[i * image_width * kBytesPerPixel],
                   valid_bytes_per_row);
          }
          if (need_flush) {
            EXPECT_EQ(ZX_OK,
                      zx_cache_flush(vmo_host, static_cast<size_t>(image_height) * bytes_per_row,
                                     ZX_CACHE_FLUSH_DATA));
          }
        }
      });
}

fuchsia::sysmem2::BufferCollectionInfo CreateBufferCollectionInfoWithConstraints(
    fuchsia::sysmem2::BufferCollectionConstraints constraints,
    allocation::BufferCollectionExportToken export_token,
    fuchsia::ui::composition::Allocator_Sync* flatland_allocator,
    fuchsia::sysmem2::Allocator_Sync* sysmem_allocator, RegisterBufferCollectionUsages usage) {
  RegisterBufferCollectionArgs rbc_args = {};
  zx_status_t status;
  // Create Sysmem tokens.
  auto [local_token, dup_token] = utils::CreateSysmemTokens(sysmem_allocator);

  rbc_args.set_export_token(std::move(export_token));
  rbc_args.set_buffer_collection_token2(std::move(dup_token));
  rbc_args.set_usages(usage);

  fuchsia::sysmem2::BufferCollectionSyncPtr buffer_collection;
  fuchsia::sysmem2::AllocatorBindSharedCollectionRequest bind_shared_request;
  bind_shared_request.set_token(std::move(local_token));
  bind_shared_request.set_buffer_collection_request(buffer_collection.NewRequest());
  status = sysmem_allocator->BindSharedCollection(std::move(bind_shared_request));
  FX_DCHECK(status == ZX_OK);

  uint32_t constraints_min_buffer_count = constraints.min_buffer_count();

  fuchsia::sysmem2::BufferCollectionSetConstraintsRequest set_constraints_reuqest;
  set_constraints_reuqest.set_constraints(std::move(constraints));
  status = buffer_collection->SetConstraints(std::move(set_constraints_reuqest));
  FX_DCHECK(status == ZX_OK);

  fuchsia::ui::composition::Allocator_RegisterBufferCollection_Result result;
  flatland_allocator->RegisterBufferCollection(std::move(rbc_args), &result);
  FX_DCHECK(!result.is_err());

  // Wait for allocation.
  fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result wait_result;
  status = buffer_collection->WaitForAllBuffersAllocated(&wait_result);
  FX_DCHECK(ZX_OK == status);
  FX_DCHECK(!wait_result.is_framework_err());
  FX_DCHECK(!wait_result.is_err());
  FX_DCHECK(wait_result.is_response());
  auto buffer_collection_info = std::move(*wait_result.response().mutable_buffer_collection_info());
  FX_DCHECK(constraints_min_buffer_count == buffer_collection_info.buffers().size());

  EXPECT_EQ(ZX_OK, buffer_collection->Release());
  return buffer_collection_info;
}

// This function returns a linear buffer of pixels of size width * height.
std::vector<uint8_t> ExtractScreenCapture(
    uint32_t buffer_id, fuchsia::sysmem2::BufferCollectionInfo& buffer_collection_info,
    uint32_t kBytesPerPixel, uint32_t render_target_width, uint32_t render_target_height) {
  // Copy ScreenCapture output for inspection. Note that the stride of the buffer may be different
  // than the width of the image, if the width of the image is not a multiple of 64.
  //
  // For instance, is the original image were 1024x600, the new width is 600. 600*4=2400 bytes,
  // which is not a multiple of 64. The next multiple would be 2432, which would mean the buffer
  // is actually a 608x1024 "pixel" buffer, since 2432/4=608. We must account for that 8 byte
  // padding when copying the bytes over to be inspected.

  FX_DCHECK(kBytesPerPixel == utils::GetBytesPerPixel(buffer_collection_info.settings()));
  uint32_t pixels_per_row =
      utils::GetPixelsPerRow(buffer_collection_info.settings(), render_target_width);
  std::vector<uint8_t> read_values;
  read_values.resize(static_cast<size_t>(render_target_width) * render_target_height *
                     kBytesPerPixel);

  MapHostPointer(
      buffer_collection_info, buffer_id, flatland::HostPointerAccessMode::kReadOnly,
      [&read_values, kBytesPerPixel, pixels_per_row, render_target_width, render_target_height](
          const uint8_t* vmo_host, uint32_t num_bytes) {
        uint32_t bytes_per_row = pixels_per_row * kBytesPerPixel;
        uint32_t valid_bytes_per_row = render_target_width * kBytesPerPixel;

        EXPECT_GE(bytes_per_row, valid_bytes_per_row);

        if (bytes_per_row == valid_bytes_per_row) {
          EXPECT_EQ(ZX_OK, zx_cache_flush(vmo_host,
                                          static_cast<size_t>(bytes_per_row) * render_target_height,
                                          ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE));
          // Fast path.
          memcpy(read_values.data(), vmo_host,
                 static_cast<size_t>(bytes_per_row) * render_target_height);
        } else {
          EXPECT_EQ(ZX_OK, zx_cache_flush(vmo_host,
                                          static_cast<size_t>(render_target_height) * bytes_per_row,
                                          ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE));
          for (size_t i = 0; i < render_target_height; ++i) {
            memcpy(&read_values[i * render_target_width * kBytesPerPixel],
                   &vmo_host[i * bytes_per_row], valid_bytes_per_row);
          }
        }
      });

  return read_values;
}

}  // namespace integration_tests

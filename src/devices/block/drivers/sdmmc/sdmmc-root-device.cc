// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "sdmmc-root-device.h"

#include <lib/driver/metadata/cpp/metadata.h>

#include <memory>

#include <fbl/alloc_checker.h>

#include "sdio-controller-device.h"
#include "sdmmc-block-device.h"

namespace sdmmc {

zx::result<> SdmmcRootDevice::Start() {
  parent_node_.Bind(std::move(node()));

  fuchsia_hardware_sdmmc::SdmmcMetadata sdmmc_metadata;
  {
    zx::result result = GetMetadata();
    if (result.is_error()) {
      FDF_LOGL(ERROR, logger(), "Failed to get metadata: %s", result.status_string());
      return result.take_error();
    }
    sdmmc_metadata = std::move(result.value());
  }

  auto [controller_client_end, controller_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto [node_client_end, node_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::Node>::Create();

  controller_.Bind(std::move(controller_client_end));
  root_node_.Bind(std::move(node_client_end));

  fidl::Arena arena;
  const auto args =
      fuchsia_driver_framework::wire::NodeAddArgs::Builder(arena).name(arena, name()).Build();

  auto result =
      parent_node_->AddChild(args, std::move(controller_server_end), std::move(node_server_end));
  if (!result.ok()) {
    FDF_LOGL(ERROR, logger(), "Failed to add child: %s", result.status_string());
    return zx::error(result.status());
  }

  zx_status_t status = Init(sdmmc_metadata);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok();
}

void SdmmcRootDevice::PrepareStop(fdf::PrepareStopCompleter completer) {
  const auto* block_device = std::get_if<std::unique_ptr<SdmmcBlockDevice>>(&child_device_);
  if (block_device) {
    block_device->get()->StopWorkerDispatcher(std::move(completer));
    return;
  }

  const auto* sdio_device = std::get_if<std::unique_ptr<SdioControllerDevice>>(&child_device_);
  if (sdio_device) {
    sdio_device->get()->StopSdioIrqDispatcher(std::move(completer));
    return;
  }

  completer(zx::ok());
}

// Returns nullptr if device was successfully added. Returns the SdmmcDevice if the probe failed
// (i.e., no eligible device present).
template <class DeviceType>
zx::result<std::unique_ptr<SdmmcDevice>> SdmmcRootDevice::MaybeAddDevice(
    const std::string& name, std::unique_ptr<SdmmcDevice> sdmmc,
    const fuchsia_hardware_sdmmc::SdmmcMetadata& metadata) {
  if (zx_status_t st = sdmmc->Init(metadata.use_fidl().value()) != ZX_OK) {
    FDF_LOGL(ERROR, logger(), "Failed to initialize SdmmcDevice: %s", zx_status_get_string(st));
    return zx::error(st);
  }

  std::unique_ptr<DeviceType> device;
  if (zx_status_t st = DeviceType::Create(this, std::move(sdmmc), &device) != ZX_OK) {
    FDF_LOGL(ERROR, logger(), "Failed to create %s device: %s", name.c_str(),
             zx_status_get_string(st));
    return zx::error(st);
  }

  if (zx_status_t st = device->Probe(metadata); st != ZX_OK) {
    return zx::ok(std::move(device->TakeSdmmcDevice()));
  }

  if (zx_status_t st = device->AddDevice(); st != ZX_OK) {
    return zx::error(st);
  }

  child_device_ = std::move(device);
  return zx::ok(nullptr);
}

zx::result<fuchsia_hardware_sdmmc::SdmmcMetadata> SdmmcRootDevice::GetMetadata() {
  constexpr uint32_t kMaxCommandPacking = 16;

  zx::result metadata =
      fdf_metadata::GetMetadataIfExists<fuchsia_hardware_sdmmc::SdmmcMetadata>(*incoming());
  if (metadata.is_error()) {
    FDF_LOGL(ERROR, logger(), "Failed to get metadata: %s", metadata.status_string());
    return metadata.take_error();
  }
  if (!metadata.value().has_value()) {
    FDF_LOGL(INFO, logger(), "No metadata provided");
    return zx::ok(fuchsia_hardware_sdmmc::SdmmcMetadata{
        {.max_frequency = UINT32_MAX,
         .speed_capabilities = fuchsia_hardware_sdmmc::SdmmcHostPrefs{0},
         .enable_cache = true,
         .removable = false,
         .max_command_packing = kMaxCommandPacking,
         .use_fidl = true,
         .vccq_off_with_controller_off = false}});
  }

  // Default to trim and cache enabled, non-removable.
  uint32_t max_frequency = metadata->max_frequency().value_or(UINT32_MAX);
  fuchsia_hardware_sdmmc::SdmmcHostPrefs speed_capabilities =
      metadata->speed_capabilities().value_or(fuchsia_hardware_sdmmc::SdmmcHostPrefs{0});
  bool enable_cache = metadata->enable_cache().value_or(true);
  bool removable = metadata->removable().value_or(false);
  uint32_t max_command_packing = metadata->max_command_packing().value_or(kMaxCommandPacking);
  bool use_fidl = metadata->use_fidl().value_or(true);
  bool vccq_off_with_controller_off = metadata->vccq_off_with_controller_off().value_or(false);

  return zx::ok(fuchsia_hardware_sdmmc::SdmmcMetadata{{
      .max_frequency = max_frequency,
      .speed_capabilities = speed_capabilities,
      .enable_cache = enable_cache,
      .removable = removable,
      .max_command_packing = max_command_packing,
      .use_fidl = use_fidl,
      .vccq_off_with_controller_off = vccq_off_with_controller_off,
  }});
}

zx_status_t SdmmcRootDevice::Init(const fuchsia_hardware_sdmmc::SdmmcMetadata& metadata) {
  auto sdmmc = std::make_unique<SdmmcDevice>(this, metadata);

  // Probe for SDIO first, then SD/MMC.
  zx::result<std::unique_ptr<SdmmcDevice>> result =
      MaybeAddDevice<SdioControllerDevice>("sdio", std::move(sdmmc), metadata);
  if (result.is_error()) {
    return result.status_value();
  }
  if (*result == nullptr) {
    return ZX_OK;
  }
  result = MaybeAddDevice<SdmmcBlockDevice>("block", std::move(*result), metadata);
  if (result.is_error()) {
    return result.status_value();
  }
  if (*result == nullptr) {
    return ZX_OK;
  }

  if (metadata.removable().value()) {
    // This controller is connected to a removable card slot, and no card was inserted. Indicate
    // success so that our device remains available.
    // TODO(https://fxbug.dev/42080592): Enable detection of card insert/removal after
    // initialization.
    FDF_LOGL(INFO, logger(), "failed to probe removable device");
    return ZX_OK;
  }

  // Failure to probe a hardwired device is unexpected. Reply with an error code so that our device
  // gets removed.
  FDF_LOGL(ERROR, logger(), "failed to probe irremovable device");
  return ZX_ERR_NOT_FOUND;
}

}  // namespace sdmmc

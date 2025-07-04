// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <lib/ddk/metadata.h>
#include <lib/ddk/platform-defs.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <zircon/compiler.h>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/hardware/i2c/cpp/bind.h>
#include <bind/fuchsia/i2c/cpp/bind.h>
#include <soc/aml-t931/t931-hw.h>

#include "post-init.h"
#include "src/ui/backlight/drivers/ti-lp8556/ti-lp8556Metadata.h"

namespace sherlock {
namespace fpbus = fuchsia_hardware_platform_bus;

static const std::vector<fpbus::Mmio> backlight_mmios{
    {{
        .base = T931_GPIO_AO_BASE,
        .length = T931_GPIO_AO_LENGTH,
    }},
};

constexpr double kMaxBrightnessInNits = 350.0;

TiLp8556Metadata kDeviceMetadata = {
    .panel_id = 0,
    // `allow_set_current_scale` is true iff the driver is on factory build.
    // Currently we assume the `post-init` driver is not on factory builds.
    .allow_set_current_scale = false,
    .registers =
        {
            // Registers
            0x01, 0x85,  // Device Control
                         // EPROM
            0xa2, 0x20,  // CFG2
            0xa3, 0x32,  // CFG3
            0xa5, 0x04,  // CFG5
            0xa7, 0xf4,  // CFG7
            0xa9, 0x60,  // CFG9
            0xae, 0x09,  // CFGE
        },
    .register_count = 14,
};

static const std::vector<fpbus::Metadata> backlight_metadata{
    {{
        .id = std::to_string(DEVICE_METADATA_BACKLIGHT_MAX_BRIGHTNESS_NITS),
        .data = std::vector<uint8_t>(
            reinterpret_cast<const uint8_t*>(&kMaxBrightnessInNits),
            reinterpret_cast<const uint8_t*>(&kMaxBrightnessInNits) + sizeof(kMaxBrightnessInNits)),
    }},
    {{
        .id = std::to_string(DEVICE_METADATA_PRIVATE),
        .data = std::vector<uint8_t>(
            reinterpret_cast<const uint8_t*>(&kDeviceMetadata),
            reinterpret_cast<const uint8_t*>(&kDeviceMetadata) + sizeof(kDeviceMetadata)),
    }},
};

static const fpbus::Node backlight_dev = []() {
  fpbus::Node dev = {};
  dev.name() = "backlight";
  dev.vid() = PDEV_VID_TI;
  dev.pid() = PDEV_PID_TI_LP8556;
  dev.did() = PDEV_DID_TI_BACKLIGHT;
  dev.metadata() = backlight_metadata;
  dev.mmio() = backlight_mmios;
  return dev;
}();

zx::result<> PostInit::InitBacklight() {
  fidl::Arena<> fidl_arena;
  fdf::Arena arena('BACK');

  auto bind_rules = std::vector{
      fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_i2c::SERVICE,
                               bind_fuchsia_hardware_i2c::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeAcceptBindRule2(bind_fuchsia::I2C_BUS_ID, bind_fuchsia_i2c::BIND_I2C_BUS_ID_I2C_3),
      fdf::MakeAcceptBindRule2(bind_fuchsia::I2C_ADDRESS,
                               bind_fuchsia_i2c::BIND_I2C_ADDRESS_BACKLIGHT),
  };

  auto properties = std::vector{
      fdf::MakeProperty2(bind_fuchsia_hardware_i2c::SERVICE,
                         bind_fuchsia_hardware_i2c::SERVICE_ZIRCONTRANSPORT),
  };

  auto parents = std::vector{
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = bind_rules,
          .properties = properties,
      }},
  };

  auto composite_node_spec =
      fuchsia_driver_framework::CompositeNodeSpec{{.name = "backlight", .parents2 = parents}};

  auto result = pbus_.buffer(arena)->AddCompositeNodeSpec(
      fidl::ToWire(fidl_arena, backlight_dev), fidl::ToWire(fidl_arena, composite_node_spec));
  if (!result.ok()) {
    FDF_LOG(ERROR, "%s: AddCompositeNodeSpec Backlight(backlight) request failed: %s", __func__,
            result.FormatDescription().data());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    FDF_LOG(ERROR, "%s: AddCompositeNodeSpec Backlight(backlight) failed: %s", __func__,
            zx_status_get_string(result->error_value()));
    return result->take_error();
  }
  return zx::ok();
}

}  // namespace sherlock

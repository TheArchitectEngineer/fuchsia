// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <lib/ddk/binding.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/platform-defs.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>

#include <cstdint>

#include <bind/fuchsia/amlogic/platform/cpp/bind.h>
#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/hardware/tee/cpp/bind.h>
#include <bind/fuchsia/platform/cpp/bind.h>

#include "nelson.h"

namespace fdf {
using namespace fuchsia_driver_framework;
}  // namespace fdf

namespace nelson {
namespace fpbus = fuchsia_hardware_platform_bus;

static const std::vector<fpbus::Bti> nelson_secure_mem_btis{
    {{
        .iommu_index = 0,
        .bti_id = BTI_AML_SECURE_MEM,
    }},
};

static const fpbus::Node secure_mem_dev = []() {
  fpbus::Node dev = {};
  dev.name() = "aml-secure-mem";
  dev.vid() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_VID_AMLOGIC;
  dev.pid() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_PID_S905D2;
  dev.did() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_DID_SECURE_MEM;
  dev.bti() = nelson_secure_mem_btis;
  return dev;
}();

zx_status_t Nelson::SecureMemInit() {
  fidl::Arena<> fidl_arena;
  fdf::Arena arena('SECU');
  std::vector<fdf::ParentSpec2> parents = {
      {
          {
              {
                  fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_tee::SERVICE,
                                           bind_fuchsia_hardware_tee::SERVICE_ZIRCONTRANSPORT),
              },
              {
                  fdf::MakeProperty2(bind_fuchsia_hardware_tee::SERVICE,
                                     bind_fuchsia_hardware_tee::SERVICE_ZIRCONTRANSPORT),
              },
          },
      },
  };

  auto result = pbus_.buffer(arena)->AddCompositeNodeSpec(
      fidl::ToWire(fidl_arena, secure_mem_dev),
      fidl::ToWire(fidl_arena, fuchsia_driver_framework::CompositeNodeSpec{
                                   {.name = "aml_securemem", .parents2 = parents}}));
  if (!result.ok()) {
    zxlogf(ERROR, "AddCompositeNodeSpec SecureMem(secure_mem_dev) request failed: %s",
           result.FormatDescription().data());
    return result.status();
  }
  if (result->is_error()) {
    zxlogf(ERROR, "AddCompositeNodeSpec SecureMem(secure_mem_dev) failed: %s",
           zx_status_get_string(result->error_value()));
    return result->error_value();
  }
  return ZX_OK;
}

}  // namespace nelson

// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.phy/cpp/fidl.h>
#include <lib/ddk/binding.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/metadata.h>
#include <lib/ddk/platform-defs.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/mmio/mmio.h>
#include <lib/zbi-format/zbi.h>
#include <lib/zircon-internal/align.h>
#include <stdlib.h>

#include <bind/fuchsia/amlogic/platform/cpp/bind.h>
#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/hardware/registers/cpp/bind.h>
#include <bind/fuchsia/hardware/usb/phy/cpp/bind.h>
#include <bind/fuchsia/platform/cpp/bind.h>
#include <bind/fuchsia/register/cpp/bind.h>
#include <bind/fuchsia/usb/phy/cpp/bind.h>
#include <soc/aml-common/aml-registers.h>
#include <soc/aml-s905d3/s905d3-hw.h>
#include <usb/cdc.h>
#include <usb/dwc2/metadata.h>
#include <usb/usb.h>

#include "nelson.h"

namespace fdf {
using namespace fuchsia_driver_framework;
}  // namespace fdf

namespace nelson {
namespace fpbus = fuchsia_hardware_platform_bus;

static const std::vector<fpbus::Mmio> dwc2_mmios{
    {{
        .base = S905D3_USB1_BASE,
        .length = S905D3_USB1_LENGTH,
    }},
};

static const std::vector<fpbus::Irq> dwc2_irqs{
    {{
        .irq = S905D3_USB1_IRQ,
        .mode = fpbus::ZirconInterruptMode::kEdgeHigh,
    }},
};

static const std::vector<fpbus::Bti> dwc2_btis{
    {{
        .iommu_index = 0,
        .bti_id = BTI_USB,
    }},
};

// Metadata for DWC2 driver.
static const dwc2_metadata_t dwc2_metadata = {
    .dma_burst_len = DWC2_DMA_BURST_INCR8,
    .usb_turnaround_time = 9,
    .rx_fifo_size = 256,   // for all OUT endpoints.
    .nptx_fifo_size = 32,  // for endpoint zero IN direction.
    .tx_fifo_sizes =
        {
            128,  // for CDC ethernet bulk IN.
            4,    // for CDC ethernet interrupt IN.
            128,  // for test function bulk IN.
            16,   // for test function interrupt IN.
        },
};

static const std::vector<fpbus::BootMetadata> usb_boot_metadata{
    {{
        // Use Bluetooth MAC address for USB ethernet as well.
        .zbi_type = ZBI_TYPE_DRV_MAC_ADDRESS,
        .zbi_extra = MACADDR_BLUETOOTH,
    }},
    {{
        // Advertise serial number over USB
        .zbi_type = ZBI_TYPE_SERIAL_NUMBER,
        .zbi_extra = 0,
    }},
};

static const std::vector<fpbus::Mmio> xhci_mmios{
    {{
        .base = S905D3_USB0_BASE,
        .length = S905D3_USB0_LENGTH,
    }},
};

static const std::vector<fpbus::Irq> xhci_irqs{
    {{
        .irq = S905D3_USB0_IRQ,
        .mode = fpbus::ZirconInterruptMode::kLevelHigh,
    }},
};

static const std::vector<fpbus::Bti> usb_btis{
    {{
        .iommu_index = 0,
        .bti_id = BTI_USB,
    }},
};

static const fpbus::Node xhci_dev = []() {
  fpbus::Node dev = {};
  dev.name() = "xhci";
  dev.vid() = PDEV_VID_GENERIC;
  dev.pid() = PDEV_PID_GENERIC;
  dev.did() = PDEV_DID_USB_XHCI_COMPOSITE;
  dev.mmio() = xhci_mmios;
  dev.irq() = xhci_irqs;
  dev.bti() = usb_btis;
  return dev;
}();

static const std::vector<fpbus::Mmio> usb_phy_mmios{
    {{
        .base = S905D3_USBCTRL_BASE,
        .length = S905D3_USBCTRL_LENGTH,
    }},
    {{
        .base = S905D3_USBPHY20_BASE,
        .length = S905D3_USBPHY20_LENGTH,
    }},
    {{
        .base = S905D3_USBPHY21_BASE,
        .length = S905D3_USBPHY21_LENGTH,
    }},
    {{
        .base = S905D3_POWER_BASE,
        .length = S905D3_POWER_LENGTH,
    }},
    {{
        .base = S905D3_SLEEP_BASE,
        .length = S905D3_SLEEP_LENGTH,
    }},
};

static const std::vector<fpbus::Irq> usb_phy_irqs{
    {{
        .irq = S905D3_USB_IDDIG_IRQ,
        .mode = fpbus::ZirconInterruptMode::kEdgeHigh,
    }},
};

zx_status_t AddUsbPhyComposite(fdf::WireSyncClient<fpbus::PlatformBus>& pbus,
                               fidl::AnyArena& fidl_arena, fdf::Arena& arena) {
  static const std::vector<fuchsia_hardware_usb_phy::UsbPhyMode> kUsbPhyModes = {
      {{.protocol = fuchsia_hardware_usb_phy::ProtocolVersion::kUsb20,
        .dr_mode = fuchsia_hardware_usb_phy::Mode::kHost,
        .is_otg_capable = false}},
      {{.protocol = fuchsia_hardware_usb_phy::ProtocolVersion::kUsb20,
        .dr_mode = fuchsia_hardware_usb_phy::Mode::kPeripheral,
        .is_otg_capable = true}},
  };

  static const fuchsia_hardware_usb_phy::Metadata kMetadata{
      {.usb_phy_modes = kUsbPhyModes, .phy_type = fuchsia_hardware_usb_phy::AmlogicPhyType::kG12A}};

  fit::result persisted_metadata = fidl::Persist(kMetadata);
  if (!persisted_metadata.is_ok()) {
    zxlogf(ERROR, "Failed to persist metadata: %s",
           persisted_metadata.error_value().FormatDescription().c_str());
    return persisted_metadata.error_value().status();
  }

  std::vector<fpbus::Metadata> usb_phy_metadata{
      // TODO(b/408003904): Remove once DEVICE_METADATA_USB_MODE is no longer used.
      {{
          .id = std::to_string(DEVICE_METADATA_USB_MODE),
          .data = persisted_metadata.value(),
      }},
      {{
          .id = fuchsia_hardware_usb_phy::Metadata::kSerializableName,
          .data = std::move(persisted_metadata.value()),
      }},
  };

  fpbus::Node usb_phy_dev{{
      .name = "aml-usb-phy",
      .vid = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_VID_AMLOGIC,
      .pid = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_PID_S905D3,
      .did = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_DID_USB_PHY_V2,
      .mmio = usb_phy_mmios,
      .irq = usb_phy_irqs,
      .bti = usb_btis,
      .metadata = usb_phy_metadata,
  }};

  const std::vector<fdf::BindRule2> kResetRegisterRules = std::vector{
      fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_registers::SERVICE,
                               bind_fuchsia_hardware_registers::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeAcceptBindRule2(bind_fuchsia_register::NAME,
                               bind_fuchsia_amlogic_platform::NAME_REGISTER_USB_PHY_V2_RESET),
  };

  const std::vector<fdf::NodeProperty2> kResetRegisterProperties = std::vector{
      fdf::MakeProperty2(bind_fuchsia_hardware_registers::SERVICE,
                         bind_fuchsia_hardware_registers::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeProperty2(bind_fuchsia_register::NAME,
                         bind_fuchsia_amlogic_platform::NAME_REGISTER_USB_PHY_V2_RESET),
  };

  std::vector<fdf::ParentSpec2> parents{{kResetRegisterRules, kResetRegisterProperties}};
  auto result = pbus.buffer(arena)->AddCompositeNodeSpec(
      fidl::ToWire(fidl_arena, usb_phy_dev),
      fidl::ToWire(fidl_arena, fuchsia_driver_framework::CompositeNodeSpec{
                                   {.name = "aml_usb_phy", .parents2 = parents}}));
  if (!result.ok()) {
    zxlogf(ERROR, "AddCompositeNodeSpec Usb(usb_phy_dev) request failed: %s",
           result.FormatDescription().data());
    return result.status();
  }
  if (result->is_error()) {
    zxlogf(ERROR, "AddCompositeNodeSpec Usb(usb_phy_dev) failed: %s",
           zx_status_get_string(result->error_value()));
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t AddDwc2Composite(fdf::WireSyncClient<fpbus::PlatformBus>& pbus,
                             fidl::AnyArena& fidl_arena, fdf::Arena& arena,
                             std::vector<fpbus::Metadata> usb_metadata) {
  const fpbus::Node dwc2_dev = [&]() {
    fpbus::Node dev = {};
    dev.name() = "dwc2";
    dev.vid() = bind_fuchsia_platform::BIND_PLATFORM_DEV_VID_GENERIC;
    dev.pid() = bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC;
    dev.did() = bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_USB_DWC2;
    dev.mmio() = dwc2_mmios;
    dev.irq() = dwc2_irqs;
    dev.bti() = dwc2_btis;
    dev.metadata() = std::move(usb_metadata);
    dev.boot_metadata() = usb_boot_metadata;
    return dev;
  }();

  const std::vector<fdf::BindRule2> kDwc2PhyRules = std::vector{
      fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_usb_phy::SERVICE,
                               bind_fuchsia_hardware_usb_phy::SERVICE_DRIVERTRANSPORT),
      fdf::MakeAcceptBindRule2(bind_fuchsia::PLATFORM_DEV_VID,
                               bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC),
      fdf::MakeAcceptBindRule2(bind_fuchsia::PLATFORM_DEV_PID,
                               bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC),
      fdf::MakeAcceptBindRule2(bind_fuchsia::PLATFORM_DEV_DID,
                               bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_USB_DWC2),
  };

  const std::vector<fdf::NodeProperty2> kDwc2PhyProperties = std::vector{
      fdf::MakeProperty2(bind_fuchsia_hardware_usb_phy::SERVICE,
                         bind_fuchsia_hardware_usb_phy::SERVICE_DRIVERTRANSPORT),
      fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_VID,
                         bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC),
      fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_PID,
                         bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC),
      fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_DID,
                         bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_USB_DWC2),
  };

  const std::vector<fdf::ParentSpec2> kDwc2Parents{{kDwc2PhyRules, kDwc2PhyProperties}};
  auto result = pbus.buffer(arena)->AddCompositeNodeSpec(
      fidl::ToWire(fidl_arena, dwc2_dev),
      fidl::ToWire(fidl_arena, fuchsia_driver_framework::CompositeNodeSpec{
                                   {.name = "dwc2_phy", .parents2 = kDwc2Parents}}));
  if (!result.ok()) {
    zxlogf(ERROR, "AddCompositeNodeSpec Usb(dwc2_phy) request failed: %s",
           result.FormatDescription().data());
    return result.status();
  }
  if (result->is_error()) {
    zxlogf(ERROR, "AddCompositeNodeSpec Usb(dwc2_phy) failed: %s",
           zx_status_get_string(result->error_value()));
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t AddXhciComposite(fdf::WireSyncClient<fpbus::PlatformBus>& pbus,
                             fidl::AnyArena& fidl_arena, fdf::Arena& arena) {
  const std::vector<fuchsia_driver_framework::BindRule2> kXhciCompositeRules = {
      fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_usb_phy::SERVICE,
                               bind_fuchsia_hardware_usb_phy::SERVICE_DRIVERTRANSPORT),
      fdf::MakeAcceptBindRule2(bind_fuchsia::PLATFORM_DEV_VID,
                               bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC),
      fdf::MakeAcceptBindRule2(bind_fuchsia::PLATFORM_DEV_PID,
                               bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC),
      fdf::MakeAcceptBindRule2(bind_fuchsia::PLATFORM_DEV_DID,
                               bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_XHCI),
  };
  const std::vector<fuchsia_driver_framework::NodeProperty2> kXhciCompositeProperties = {
      fdf::MakeProperty2(bind_fuchsia_hardware_usb_phy::SERVICE,
                         bind_fuchsia_hardware_usb_phy::SERVICE_DRIVERTRANSPORT),
      fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_VID,
                         bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC),
      fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_PID,
                         bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC),
      fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_DID,
                         bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_XHCI),
  };

  const std::vector<fuchsia_driver_framework::ParentSpec2> kXhciParents = {
      fuchsia_driver_framework::ParentSpec2{
          {.bind_rules = kXhciCompositeRules, .properties = kXhciCompositeProperties}}};
  auto result = pbus.buffer(arena)->AddCompositeNodeSpec(
      fidl::ToWire(fidl_arena, xhci_dev),
      fidl::ToWire(fidl_arena, fuchsia_driver_framework::CompositeNodeSpec{
                                   {.name = "xhci-phy", .parents2 = kXhciParents}}));
  if (!result.ok()) {
    zxlogf(ERROR, "AddCompositeNodeSpec Usb(xhci-phy) request failed: %s",
           result.FormatDescription().data());
    return result.status();
  }
  if (result->is_error()) {
    zxlogf(ERROR, "AddCompositeNodeSpec Usb(xhci-phy) failed: %s",
           zx_status_get_string(result->error_value()));
    return result->error_value();
  }
  return ZX_OK;
}

zx_status_t Nelson::UsbInit() {
  fidl::Arena<> fidl_arena;
  fdf::Arena arena('USB_');

  auto status = AddUsbPhyComposite(pbus_, fidl_arena, arena);
  if (status != ZX_OK) {
    zxlogf(ERROR, "AddUsbPhyComposite failed: %d", status);
    return status;
  }

  // Add XHCI and DWC2 to the same devhost as the aml-usb-phy.
  status = AddXhciComposite(pbus_, fidl_arena, arena);
  if (status != ZX_OK) {
    return status;
  }

  const std::vector<fpbus::Metadata> usb_metadata{
      {{
          .id = std::to_string(DEVICE_METADATA_PRIVATE),
          .data = std::vector<uint8_t>(
              reinterpret_cast<const uint8_t*>(&dwc2_metadata),
              reinterpret_cast<const uint8_t*>(&dwc2_metadata) + sizeof(dwc2_metadata)),
      }},
  };

  return AddDwc2Composite(pbus_, fidl_arena, arena, std::move(usb_metadata));
}

}  // namespace nelson

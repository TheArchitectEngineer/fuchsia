// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bus/drivers/pci/kpci.h"

#include <fuchsia/hardware/pciroot/cpp/banjo.h>
#include <lib/async-loop/loop.h>
#include <lib/ddk/binding_driver.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/ddk/platform-defs.h>
#include <lib/fidl/cpp/wire/connect_service.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/fidl.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/pci.h>
#include <zircon/syscalls/resource.h>
#include <zircon/types.h>

#include <memory>

#include <bind/fuchsia/acpi/cpp/bind.h>
#include <bind/fuchsia/cpp/bind.h>
#include <ddktl/device.h>

#include "src/devices/bus/drivers/pci/composite.h"

namespace fpci = ::fuchsia_hardware_pci;

namespace pci {

// Some functions used by both the Banjo and FIDL implementations are abstracted
// out and defined here.

zx_status_t pci_get_bar(kpci_device* device, uint32_t bar_id, pci_bar_t* out_res) {
  if (bar_id >= ZX_PCI_MAX_BAR_REGS) {
    return ZX_ERR_INVALID_ARGS;
  }

  zx_handle_t handle = ZX_HANDLE_INVALID;
  zx_pci_bar_t bar{};
  zx_status_t st = zx_pci_get_bar(device->handle, bar_id, &bar, &handle);
  if (st == ZX_OK) {
    out_res->bar_id = bar_id;
    out_res->size = bar.size;
    out_res->type = bar.type;
    if (out_res->type == PCI_BAR_TYPE_IO) {
      char name[] = "kPCI IO";
      st = zx_resource_create(get_ioport_resource(device->zxdev), ZX_RSRC_KIND_IOPORT, bar.addr,
                              bar.size, name, sizeof(name), &handle);
      out_res->result.io.address = bar.addr;
      out_res->result.io.resource = handle;
    } else {
      out_res->result.vmo = handle;
    }
  }

  return st;
}

void pci_get_interrupt_modes(kpci_device* device, pci_interrupt_modes_t* out_modes) {
  pci_interrupt_modes_t modes{};
  uint32_t count = 0;
  zx_pci_query_irq_mode(device->handle, PCI_INTERRUPT_MODE_LEGACY, &count);
  modes.has_legacy = !!count;
  zx_pci_query_irq_mode(device->handle, PCI_INTERRUPT_MODE_MSI, &count);
  modes.msi_count = static_cast<uint8_t>(count);
  zx_pci_query_irq_mode(device->handle, PCI_INTERRUPT_MODE_MSI_X, &count);
  modes.msix_count = static_cast<uint16_t>(count);
  *out_modes = modes;
}

void pci_get_device_info(kpci_device* device, pci_device_info_t* out_info) {
  memcpy(out_info, &device->info, sizeof(*out_info));
}

zx_status_t pci_get_next_capability(kpci_device* device, uint8_t cap_id, uint8_t offset,
                                    uint8_t* out_offset) {
  // If we're looking for the first capability then we read from the offset
  // since it contains 0x34 which ppints to the start of the list. Otherwise, we
  // have an existing capability's offset and need to advance one byte to its
  // next pointer.
  if (offset != fidl::ToUnderlying(fuchsia_hardware_pci::Config::kCapabilitiesPtr)) {
    offset++;
  }

  // Walk the capability list looking for the type requested.  limit acts as a
  // barrier in case of an invalid capability pointer list that causes us to
  // iterate forever otherwise.
  uint8_t limit = 64;
  uint32_t cap_offset = 0;
  zx_pci_config_read(device->handle, offset, sizeof(uint8_t), &cap_offset);
  while (cap_offset != 0 && cap_offset != 0xFF && limit--) {
    zx_status_t st;
    uint32_t type_id = 0;
    if ((st = zx_pci_config_read(device->handle, static_cast<uint16_t>(cap_offset), sizeof(uint8_t),
                                 &type_id)) != ZX_OK) {
      zxlogf(ERROR, "%s: error reading type from cap offset %#x: %d", __func__, cap_offset, st);
      return st;
    }

    if (type_id == cap_id) {
      *out_offset = static_cast<uint8_t>(cap_offset);
      return ZX_OK;
    }

    // We didn't find the right type, move on, but ensure we're still within the
    // first 256 bytes of standard config space.
    if (cap_offset >= UINT8_MAX) {
      zxlogf(ERROR, "%s: %#x is an invalid capability offset!", __func__, cap_offset);
      break;
    }
    if ((st = zx_pci_config_read(device->handle, static_cast<uint16_t>(cap_offset + 1),
                                 sizeof(uint8_t), &cap_offset)) != ZX_OK) {
      zxlogf(ERROR, "%s: error reading next cap from cap offset %#x: %d", __func__, cap_offset + 1,
             st);
      break;
    }
  }

  return ZX_ERR_NOT_FOUND;
}

zx_status_t pci_get_bti(kpci_device* device, uint32_t index, zx::bti* out_bti) {
  uint32_t bdf = (static_cast<uint32_t>(device->info.bus_id) << 8) |
                 (static_cast<uint32_t>(device->info.dev_id) << 3) | device->info.func_id;
  if (device->pciroot.ops) {
    return pciroot_get_bti(&device->pciroot, bdf, index, out_bti->reset_and_get_address());
  }

  if (device->pdev.is_valid()) {
    // TODO(teisenbe): This isn't quite right. We need to develop a way to
    // resolve which BTI should go to downstream. However, we don't currently
    // support any SMMUs for ARM, so this will work for now.
    zx::result bti = device->pdev.GetBti(0);
    if (bti.is_error()) {
      return bti.status_value();
    }
    *out_bti = std::move(bti.value());
    return ZX_OK;
  }

  return ZX_ERR_NOT_SUPPORTED;
}

template <typename T>
zx_status_t ReadConfig(zx_handle_t device, uint16_t offset, T* out_value) {
  uint32_t value;
  zx_status_t st = zx_pci_config_read(device, offset, sizeof(T), &value);
  if (st == ZX_OK) {
    *out_value = static_cast<T>(value);
  }
  return st;
}

// Initializes the upper half of a pci / pci.proxy devhost pair.
static zx_status_t pci_init_child(zx_device_t* parent, uint32_t index,
                                  pci_platform_info_t* plat_info) {
  zx_pcie_device_info_t info;
  zx_handle_t handle;

  if (!parent) {
    return ZX_ERR_BAD_STATE;
  }

  // This is a legacy function to get the 'nth' device on a bus.
  zx_status_t status = zx_pci_get_nth_device(get_irq_resource(parent), index, &info, &handle);
  if (status != ZX_OK) {
    return status;
  }

  kpci_device device{
      .handle = handle,
      .index = index,
      .info = {.vendor_id = info.vendor_id,
               .device_id = info.device_id,
               .base_class = info.base_class,
               .sub_class = info.sub_class,
               .program_interface = info.program_interface,
               .revision_id = info.revision_id,
               .bus_id = info.bus_id,
               .dev_id = info.dev_id,
               .func_id = info.func_id},
  };

  // Store the PCIROOT protocol for use with get_bti in the pci protocol It is
  // not fatal if this fails, but bti protocol methods will not work.
  device_get_protocol(parent, ZX_PROTOCOL_PCIROOT, &device.pciroot);
  zx::result pdev =
      ddk::Device<void>::DdkConnectFidlProtocol<fuchsia_hardware_platform_device::Service::Device>(
          parent);
  device.pdev = fdf::PDev{std::move(pdev.value())};

  bool uses_acpi = false;
  for (size_t i = 0; i < plat_info->acpi_bdfs_count; i++) {
    const pci_bdf_t* bdf = &plat_info->acpi_bdfs_list[i];
    if (bdf->bus_id == device.info.bus_id && bdf->device_id == device.info.dev_id &&
        bdf->function_id == device.info.func_id) {
      uses_acpi = true;
      break;
    }
  }

  snprintf(device.name, sizeof(device.name), "%02x:%02x.%1x", device.info.bus_id,
           device.info.dev_id, device.info.func_id);
  status = KernelPci::CreateComposite(parent, std::move(device), uses_acpi);
  if (status != ZX_OK) {
    zxlogf(ERROR, "failed to create FIDL kPCI for %#02x:%#02x.%1x (%#04x:%#04x)", info.bus_id,
           info.dev_id, info.func_id, info.vendor_id, info.device_id);
    return status;
  }

  return status;
}

zx_status_t KernelPci::CreateComposite(zx_device_t* parent, kpci_device device, bool uses_acpi) {
  auto pci_bind_topo = static_cast<uint32_t>(
      BIND_PCI_TOPO_PACK(device.info.bus_id, device.info.dev_id, device.info.func_id));
  zx_device_str_prop_t pci_device_props[] = {
      ddk::MakeStrProperty(bind_fuchsia::PCI_VID, static_cast<uint32_t>(device.info.vendor_id)),
      ddk::MakeStrProperty(bind_fuchsia::PCI_DID, static_cast<uint32_t>(device.info.device_id)),
      ddk::MakeStrProperty(bind_fuchsia::PCI_CLASS, static_cast<uint32_t>(device.info.base_class)),
      ddk::MakeStrProperty(bind_fuchsia::PCI_SUBCLASS,
                           static_cast<uint32_t>(device.info.sub_class)),
      ddk::MakeStrProperty(bind_fuchsia::PCI_INTERFACE,
                           static_cast<uint32_t>(device.info.program_interface)),
      ddk::MakeStrProperty(bind_fuchsia::PCI_REVISION,
                           static_cast<uint32_t>(device.info.revision_id)),
      ddk::MakeStrProperty(bind_fuchsia::PCI_TOPO, pci_bind_topo),
  };

  async_dispatcher_t* dispatcher =
      fdf_dispatcher_get_async_dispatcher(fdf_dispatcher_get_current_dispatcher());
  auto kpci = std::make_unique<KernelPci>(parent, std::move(device), dispatcher);

  auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
  if (endpoints.is_error()) {
    return endpoints.status_value();
  }
  zx_status_t status = kpci->SetUpOutgoingDirectory(std::move(endpoints->server));

  std::array offers = {
      fpci::Service::Name,
  };

  char device_name[ZX_DEVICE_NAME_MAX];
  // The underscore at the end of the name indicates a FIDL PCI device, rather
  // than Banjo.
  snprintf(device_name, sizeof(device_name), "%s_", device.name);
  status = kpci->DdkAdd(ddk::DeviceAddArgs(device_name)
                            .set_flags(DEVICE_ADD_MUST_ISOLATE)
                            .set_str_props(pci_device_props)
                            .set_fidl_service_offers(offers)
                            .set_outgoing_dir(endpoints->client.TakeChannel()));
  if (status != ZX_OK) {
    return status;
  }
  auto kpci_unowned = kpci.release();

  auto pci_info = CompositeInfo{
      .vendor_id = device.info.vendor_id,
      .device_id = device.info.device_id,
      .class_id = device.info.base_class,
      .subclass = device.info.sub_class,
      .program_interface = device.info.program_interface,
      .revision_id = device.info.revision_id,
      .bus_id = device.info.bus_id,
      .dev_id = device.info.dev_id,
      .func_id = device.info.func_id,
      .has_acpi = uses_acpi,
  };

  char spec_name[8];
  snprintf(spec_name, sizeof(spec_name), "%02x_%02x_%01x", device.info.bus_id, device.info.dev_id,
           device.info.func_id);
  status = kpci_unowned->DdkAddCompositeNodeSpec(spec_name, CreateCompositeNodeSpec(pci_info));
  return status;
}

KernelPci::KernelPci(zx_device_t* parent, kpci_device device, async_dispatcher_t* dispatcher)
    : KernelPciType(parent),
      device_(std::move(device)),
      dispatcher_(dispatcher),
      outgoing_(dispatcher) {}

void KernelPci::DdkRelease() {
  if (device_.handle != ZX_HANDLE_INVALID) {
    zx_handle_close(device_.handle);
  }

  delete this;
}

void KernelPci::GetBar(GetBarRequestView request, GetBarCompleter::Sync& completer) {
  pci_bar_t bar;
  zx_status_t status = pci_get_bar(&device_, request->bar_id, &bar);
  if (status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }

  if (bar.type == PCI_BAR_TYPE_IO) {
    fidl::Arena arena;
    completer.ReplySuccess(
        {.bar_id = request->bar_id,
         .size = bar.size,
         .result = fpci::wire::BarResult::WithIo(
             arena, fpci::wire::IoBar{.address = bar.result.io.address,
                                      .resource = zx::resource(bar.result.io.resource)})});
  } else {
    completer.ReplySuccess({.bar_id = request->bar_id,
                            .size = bar.size,
                            .result = fpci::wire::BarResult::WithVmo(zx::vmo(bar.result.vmo))});
  }
}

void KernelPci::SetBusMastering(SetBusMasteringRequestView request,
                                SetBusMasteringCompleter::Sync& completer) {
  zx_status_t status = zx_pci_enable_bus_master(device_.handle, request->enabled);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::ResetDevice(ResetDeviceCompleter::Sync& completer) {
  zx_status_t status = zx_pci_reset_device(device_.handle);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::AckInterrupt(AckInterruptCompleter::Sync& completer) { completer.ReplySuccess(); }

void KernelPci::MapInterrupt(MapInterruptRequestView request,
                             MapInterruptCompleter::Sync& completer) {
  zx::interrupt out_interrupt;
  zx_status_t status = zx_pci_map_interrupt(device_.handle, request->which_irq,
                                            out_interrupt.reset_and_get_address());
  if (status == ZX_OK) {
    completer.ReplySuccess(std::move(out_interrupt));
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::GetInterruptModes(GetInterruptModesCompleter::Sync& completer) {
  pci_interrupt_modes_t out_modes;
  pci_get_interrupt_modes(&device_, &out_modes);
  completer.Reply(fpci::wire::InterruptModes{
      .has_legacy = out_modes.has_legacy,
      .msi_count = out_modes.msi_count,
      .msix_count = out_modes.msix_count,
  });
}

void KernelPci::SetInterruptMode(SetInterruptModeRequestView request,
                                 SetInterruptModeCompleter::Sync& completer) {
  zx_status_t status = zx_pci_set_irq_mode(device_.handle, fidl::ToUnderlying(request->mode),
                                           request->requested_irq_count);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::GetDeviceInfo(GetDeviceInfoCompleter::Sync& completer) {
  pci_device_info_t out_info;
  pci_get_device_info(&device_, &out_info);
  completer.Reply(fpci::wire::DeviceInfo{
      .vendor_id = out_info.vendor_id,
      .device_id = out_info.device_id,
      .base_class = out_info.base_class,
      .sub_class = out_info.sub_class,
      .program_interface = out_info.program_interface,
      .revision_id = out_info.revision_id,
      .bus_id = out_info.bus_id,
      .dev_id = out_info.dev_id,
      .func_id = out_info.func_id,
  });
}

void KernelPci::ReadConfig8(ReadConfig8RequestView request, ReadConfig8Completer::Sync& completer) {
  uint8_t out_value;
  zx_status_t status = ReadConfig(device_.handle, request->offset, &out_value);
  if (status == ZX_OK) {
    completer.ReplySuccess(out_value);
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::ReadConfig16(ReadConfig16RequestView request,
                             ReadConfig16Completer::Sync& completer) {
  uint16_t out_value;
  zx_status_t status = ReadConfig(device_.handle, request->offset, &out_value);
  if (status == ZX_OK) {
    completer.ReplySuccess(out_value);
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::ReadConfig32(ReadConfig32RequestView request,
                             ReadConfig32Completer::Sync& completer) {
  uint32_t out_value;
  zx_status_t status = ReadConfig(device_.handle, request->offset, &out_value);
  if (status == ZX_OK) {
    completer.ReplySuccess(out_value);
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::WriteConfig8(WriteConfig8RequestView request,
                             WriteConfig8Completer::Sync& completer) {
  zx_status_t status =
      zx_pci_config_write(device_.handle, request->offset, sizeof(request->value), request->value);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::WriteConfig16(WriteConfig16RequestView request,
                              WriteConfig16Completer::Sync& completer) {
  zx_status_t status =
      zx_pci_config_write(device_.handle, request->offset, sizeof(request->value), request->value);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::WriteConfig32(WriteConfig32RequestView request,
                              WriteConfig32Completer::Sync& completer) {
  zx_status_t status =
      zx_pci_config_write(device_.handle, request->offset, sizeof(request->value), request->value);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void KernelPci::GetCapabilities(GetCapabilitiesRequestView request,
                                GetCapabilitiesCompleter::Sync& completer) {
  std::vector<uint8_t> capabilities;
  uint8_t offset = fidl::ToUnderlying(fuchsia_hardware_pci::Config::kCapabilitiesPtr);
  uint8_t out_offset;
  while (true) {
    zx_status_t status =
        pci_get_next_capability(&device_, fidl::ToUnderlying(request->id), offset, &out_offset);
    if (status == ZX_ERR_NOT_FOUND) {
      break;
    } else if (status != ZX_OK) {
      completer.Close(status);
      return;
    }

    capabilities.push_back(out_offset);
    offset = out_offset;
  }
  completer.Reply(fidl::VectorView<uint8_t>::FromExternal(capabilities));
}

void KernelPci::GetExtendedCapabilities(GetExtendedCapabilitiesRequestView request,
                                        GetExtendedCapabilitiesCompleter::Sync& completer) {
  completer.Close(ZX_ERR_NOT_SUPPORTED);
}

void KernelPci::GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) {
  zx::bti out_bti;
  zx_status_t status = pci_get_bti(&device_, request->index, &out_bti);
  if (status == ZX_OK) {
    completer.ReplySuccess(std::move(out_bti));
  } else {
    completer.ReplyError(status);
  }
}

zx_status_t KernelPci::SetUpOutgoingDirectory(fidl::ServerEnd<fuchsia_io::Directory> server_end) {
  zx::result status = outgoing_.AddService<fuchsia_hardware_pci::Service>(
      fuchsia_hardware_pci::Service::InstanceHandler({
          .device = bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure),
      }));

  if (status.is_error()) {
    return status.status_value();
  }

  return outgoing_.Serve(std::move(server_end)).status_value();
}

static zx_status_t pci_drv_bind(void* ctx, zx_device_t* parent) {
  pci_platform_info_t platform_info{};
  pciroot_protocol_t pciroot;
  zx_status_t result = device_get_protocol(parent, ZX_PROTOCOL_PCIROOT, &pciroot);
  if (result == ZX_OK) {
    result = pciroot_get_pci_platform_info(&pciroot, &platform_info);
  }
  // Walk PCI devices to create their upper half devices until we hit the end
  for (uint32_t index = 0;; index++) {
    if (pci_init_child(parent, index, &platform_info) != ZX_OK) {
      break;
    }
  }
  return ZX_OK;
}

}  // namespace pci

static zx_driver_ops_t kpci_driver_ops = {
    .version = DRIVER_OPS_VERSION,
    .bind = pci::pci_drv_bind,
};

ZIRCON_DRIVER(pci, kpci_driver_ops, "zircon", "0.1");

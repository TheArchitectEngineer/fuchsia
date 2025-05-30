// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/intel-display/dp-display.h"

#include <lib/driver/logging/cpp/logger.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/result.h>
#include <lib/zx/time.h>
#include <zircon/assert.h>
#include <zircon/errors.h>

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <limits>
#include <optional>

#include <fbl/string_printf.h>

#include "src/graphics/display/drivers/intel-display/ddi-physical-layer-manager.h"
#include "src/graphics/display/drivers/intel-display/dpll.h"
#include "src/graphics/display/drivers/intel-display/edid-reader.h"
#include "src/graphics/display/drivers/intel-display/hardware-common.h"
#include "src/graphics/display/drivers/intel-display/intel-display.h"
#include "src/graphics/display/drivers/intel-display/pch-engine.h"
#include "src/graphics/display/drivers/intel-display/pci-ids.h"
#include "src/graphics/display/drivers/intel-display/pipe.h"
#include "src/graphics/display/drivers/intel-display/registers-ddi-phy-tiger-lake.h"
#include "src/graphics/display/drivers/intel-display/registers-ddi.h"
#include "src/graphics/display/drivers/intel-display/registers-transcoder.h"
#include "src/graphics/display/drivers/intel-display/registers-typec.h"
#include "src/graphics/display/lib/api-types/cpp/display-timing.h"
#include "src/graphics/display/lib/driver-utils/poll-until.h"

namespace intel_display {
namespace {

constexpr uint32_t kBitsPerPixel = 24;  // kPixelFormat

// Recommended DDI buffer translation programming values

struct DdiPhyConfigEntry {
  uint32_t entry2;
  uint32_t entry1;
};

// The tables below have the values recommended by the documentation.
//
// Kaby Lake: IHD-OS-KBL-Vol 12-1.17 pages 187-190
// Skylake: IHD-OS-SKL-Vol 12-05.16 pages 181-183
//
// TODO(https://fxbug.dev/42059656): Per-entry Iboost values.

constexpr DdiPhyConfigEntry kPhyConfigDpSkylakeHs[9] = {
    {0x000000a0, 0x00002016}, {0x0000009b, 0x00005012}, {0x00000088, 0x00007011},
    {0x000000c0, 0x80009010}, {0x0000009b, 0x00002016}, {0x00000088, 0x00005012},
    {0x000000c0, 0x80007011}, {0x000000df, 0x00002016}, {0x000000c0, 0x80005012},
};

constexpr DdiPhyConfigEntry kPhyConfigDpSkylakeY[9] = {
    {0x000000a2, 0x00000018}, {0x00000088, 0x00005012}, {0x000000cd, 0x80007011},
    {0x000000c0, 0x80009010}, {0x0000009d, 0x00000018}, {0x000000c0, 0x80005012},
    {0x000000c0, 0x80007011}, {0x00000088, 0x00000018}, {0x000000c0, 0x80005012},
};

constexpr DdiPhyConfigEntry kPhyConfigDpSkylakeU[9] = {
    {0x000000a2, 0x0000201b}, {0x00000088, 0x00005012}, {0x000000cd, 0x80007011},
    {0x000000c0, 0x80009010}, {0x0000009d, 0x0000201b}, {0x000000c0, 0x80005012},
    {0x000000c0, 0x80007011}, {0x00000088, 0x00002016}, {0x000000c0, 0x80005012},
};

constexpr DdiPhyConfigEntry kPhyConfigDpKabyLakeHs[9] = {
    {0x000000a0, 0x00002016}, {0x0000009b, 0x00005012}, {0x00000088, 0x00007011},
    {0x000000c0, 0x80009010}, {0x0000009b, 0x00002016}, {0x00000088, 0x00005012},
    {0x000000c0, 0x80007011}, {0x00000097, 0x00002016}, {0x000000c0, 0x80005012},
};

constexpr DdiPhyConfigEntry kPhyConfigDpKabyLakeY[9] = {
    {0x000000a1, 0x00001017}, {0x00000088, 0x00005012}, {0x000000cd, 0x80007011},
    {0x000000c0, 0x8000800f}, {0x0000009d, 0x00001017}, {0x000000c0, 0x80005012},
    {0x000000c0, 0x80007011}, {0x0000004c, 0x00001017}, {0x000000c0, 0x80005012},
};

constexpr DdiPhyConfigEntry kPhyConfigDpKabyLakeU[9] = {
    {0x000000a1, 0x0000201b}, {0x00000088, 0x00005012}, {0x000000cd, 0x80007011},
    {0x000000c0, 0x80009010}, {0x0000009d, 0x0000201b}, {0x000000c0, 0x80005012},
    {0x000000c0, 0x80007011}, {0x0000004f, 0x00002016}, {0x000000c0, 0x80005012},
};

constexpr DdiPhyConfigEntry kPhyConfigEdpKabyLakeHs[10] = {
    {0x000000a8, 0x00000018}, {0x000000a9, 0x00004013}, {0x000000a2, 0x00007011},
    {0x0000009c, 0x00009010}, {0x000000a9, 0x00000018}, {0x000000a2, 0x00006013},
    {0x000000a6, 0x00007011}, {0x000000ab, 0x00000018}, {0x0000009f, 0x00007013},
    {0x000000df, 0x00000018},
};

constexpr DdiPhyConfigEntry kPhyConfigEdpKabyLakeY[10] = {
    {0x000000a8, 0x00000018}, {0x000000ab, 0x00004013}, {0x000000a4, 0x00007011},
    {0x000000df, 0x00009010}, {0x000000aa, 0x00000018}, {0x000000a4, 0x00006013},
    {0x0000009d, 0x00007011}, {0x000000a0, 0x00000018}, {0x000000df, 0x00006012},
    {0x0000008a, 0x00000018},
};

constexpr DdiPhyConfigEntry kPhyConfigEdpKabyLakeU[10] = {
    {0x000000a8, 0x00000018}, {0x000000a9, 0x00004013}, {0x000000a2, 0x00007011},
    {0x0000009c, 0x00009010}, {0x000000a9, 0x00000018}, {0x000000a2, 0x00006013},
    {0x000000a6, 0x00007011}, {0x000000ab, 0x00002016}, {0x0000009f, 0x00005013},
    {0x000000df, 0x00000018},
};

cpp20::span<const DdiPhyConfigEntry> GetDpPhyConfigEntries(uint16_t device_id, uint8_t* i_boost) {
  if (is_skl(device_id)) {
    if (is_skl_u(device_id)) {
      *i_boost = 0x1;
      return kPhyConfigDpSkylakeU;
    }
    if (is_skl_y(device_id)) {
      *i_boost = 0x3;
      return kPhyConfigDpSkylakeY;
    }
    *i_boost = 0x1;
    return kPhyConfigDpSkylakeHs;
  }
  if (is_kbl(device_id)) {
    if (is_kbl_u(device_id)) {
      *i_boost = 0x1;
      return kPhyConfigDpKabyLakeU;
    }
    if (is_kbl_y(device_id)) {
      *i_boost = 0x3;
      return kPhyConfigDpKabyLakeY;
    }
    *i_boost = 0x3;
    return kPhyConfigDpKabyLakeHs;
  }

  fdf::error("Unsupported intel-display device id: {:x}", device_id);
  *i_boost = 0;
  return {};
}

cpp20::span<const DdiPhyConfigEntry> GetEdpPhyConfigEntries(uint16_t device_id, uint8_t* i_boost) {
  *i_boost = 0x0;
  if (is_skl_u(device_id) || is_kbl_u(device_id)) {
    return kPhyConfigEdpKabyLakeU;
  }
  if (is_skl_y(device_id) || is_kbl_y(device_id)) {
    return kPhyConfigEdpKabyLakeY;
  }
  return kPhyConfigEdpKabyLakeHs;
}

// DisplayPort 2.1 supports up to 4 main link lanes.
//
// VESA DisplayPort (DP) Standard. Version 2.1. 10 October, 2022.
// Section 2.1.1 "Number of Lanes and Per-lane Data Rate in SST and MST Modes".
constexpr int kMaxDisplayPortLaneCount = 4;

// Must match `kPixelFormatTypes` defined in intel-display.cc.
constexpr fuchsia_images2_pixel_format_enum_value_t kBanjoSupportedPixelFormatsArray[] = {
    static_cast<fuchsia_images2_pixel_format_enum_value_t>(
        fuchsia_images2::wire::PixelFormat::kB8G8R8A8),
    static_cast<fuchsia_images2_pixel_format_enum_value_t>(
        fuchsia_images2::wire::PixelFormat::kR8G8B8A8),
};

constexpr cpp20::span<const fuchsia_images2_pixel_format_enum_value_t> kBanjoSupportedPixelFormats(
    kBanjoSupportedPixelFormatsArray);

}  // namespace

bool DpDisplay::EnsureEdpPanelIsPoweredOn() {
  // Fix the panel configuration, if necessary.
  const PchPanelParameters panel_parameters = pch_engine_->PanelParameters();
  PchPanelParameters fixed_panel_parameters = panel_parameters;
  fixed_panel_parameters.Fix();
  if (panel_parameters != fixed_panel_parameters) {
    fdf::warn("Incorrect PCH configuration for eDP panel. Re-configuring.");
  }
  pch_engine_->SetPanelParameters(fixed_panel_parameters);
  fdf::trace("Setting eDP backlight brightness to {:f}", backlight_brightness_);
  pch_engine_->SetPanelBrightness(backlight_brightness_);
  fdf::trace("eDP panel configured.");

  // Power up the panel, if necessary.
  PchPanelPowerTarget power_target = pch_engine_->PanelPowerTarget();

  // The boot firmware might have left `force_power_on` set to true. To avoid
  // turning the panel off and on (and get the associated HPD interrupts), we
  // need to leave `force_power_on` as-is while we perform PCH-managed panel
  // power sequencing. Once the PCH keeps the panel on, we can set
  // `force_power_on` to false.
  power_target.power_on = true;

  // At least one Tiger Lake laptop panel fails to light up if we don't keep the
  // PWM counter disabled through the panel power sequence.
  power_target.brightness_pwm_counter_on = false;
  pch_engine_->SetPanelPowerTarget(power_target);

  // The Atlas panel takes more time to power up than required in the eDP and
  // SPWG Notebook Panel standards.
  //
  // The generous timeout is chosen because we really don't want to give up too
  // early and leave the user with a non-working system, if there's any hope.
  // The waiting code polls the panel state every few ms, so we don't waste too
  // much time if the panel wakes up early / on time.
  static constexpr int kPowerUpTimeoutUs = 1'000'000;
  if (!pch_engine_->WaitForPanelPowerState(PchPanelPowerState::kPoweredUp, kPowerUpTimeoutUs)) {
    fdf::error("Failed to enable panel!");
    pch_engine_->Log();
    return false;
  }

  // The PCH panel power sequence has completed. Now it's safe to set
  // `force_power_on` to false, if it was true. The PCH will keep the panel
  // powered on.
  power_target.backlight_on = true;
  power_target.brightness_pwm_counter_on = true;
  power_target.force_power_on = false;
  pch_engine_->SetPanelPowerTarget(power_target);

  fdf::trace("eDP panel powered on.");
  return true;
}

bool DpDisplay::DpcdWrite(uint32_t addr, const uint8_t* buf, size_t size) {
  return dp_aux_channel_->DpcdWrite(addr, buf, size);
}

bool DpDisplay::DpcdRead(uint32_t addr, uint8_t* buf, size_t size) {
  return dp_aux_channel_->DpcdRead(addr, buf, size);
}

// Link training functions

// Tell the sink device to start link training.
bool DpDisplay::DpcdRequestLinkTraining(const dpcd::TrainingPatternSet& tp_set,
                                        const dpcd::TrainingLaneSet lane[]) {
  // The DisplayPort spec says that we are supposed to write these
  // registers with a single operation: "The AUX CH burst write must be
  // used for writing to TRAINING_LANEx_SET bytes of the enabled lanes."
  // (From section 3.5.1.3, "Link Training", in v1.1a.)
  uint8_t reg_bytes[1 + kMaxDisplayPortLaneCount];
  reg_bytes[0] = static_cast<uint8_t>(tp_set.reg_value());
  for (unsigned i = 0; i < dp_lane_count_; i++) {
    reg_bytes[i + 1] = static_cast<uint8_t>(lane[i].reg_value());
  }
  constexpr int kAddr = dpcd::DPCD_TRAINING_PATTERN_SET;
  static_assert(kAddr + 1 == dpcd::DPCD_TRAINING_LANE0_SET, "");
  static_assert(kAddr + 2 == dpcd::DPCD_TRAINING_LANE1_SET, "");
  static_assert(kAddr + 3 == dpcd::DPCD_TRAINING_LANE2_SET, "");
  static_assert(kAddr + 4 == dpcd::DPCD_TRAINING_LANE3_SET, "");

  if (!DpcdWrite(kAddr, reg_bytes, 1 + dp_lane_count_)) {
    fdf::error("Failure setting TRAINING_PATTERN_SET");
    return false;
  }

  return true;
}

template <uint32_t addr, typename T>
bool DpDisplay::DpcdReadPairedRegs(hwreg::RegisterBase<T, typename T::ValueType>* regs) {
  static_assert(addr == dpcd::DPCD_LANE0_1_STATUS || addr == dpcd::DPCD_ADJUST_REQUEST_LANE0_1,
                "Bad register address");
  constexpr int kMaximumRegisterSize = 2;
  uint32_t num_bytes = dp_lane_count_ == 4 ? 2 : 1;
  uint8_t reg_byte[kMaximumRegisterSize];
  if (!DpcdRead(addr, reg_byte, num_bytes)) {
    fdf::error("Failure reading addr {}", addr);
    return false;
  }

  for (unsigned i = 0; i < dp_lane_count_; i++) {
    regs[i].set_reg_value(reg_byte[i / 2]);
  }

  return true;
}

bool DpDisplay::DpcdHandleAdjustRequest(dpcd::TrainingLaneSet* training,
                                        dpcd::AdjustRequestLane* adjust) {
  bool voltage_changed = false;
  uint8_t voltage_swing = 0;
  uint8_t pre_emphasis = 0;
  for (int lane_index = 0; lane_index < dp_lane_count_; ++lane_index) {
    if (adjust[lane_index].voltage_swing(lane_index).get() > voltage_swing) {
      // The cast is lossless because voltage_swing() is a 2-bit field.
      voltage_swing = static_cast<uint8_t>(adjust[lane_index].voltage_swing(lane_index).get());
    }
    if (adjust[lane_index].pre_emphasis(lane_index).get() > pre_emphasis) {
      // The cast is lossless because pre-emphasis() is a 2-bit field.
      pre_emphasis = static_cast<uint8_t>(adjust[lane_index].pre_emphasis(lane_index).get());
    }
  }

  // In the Recommended buffer translation programming for DisplayPort from the intel display
  // doc, the max voltage swing is 2/3 for DP/eDP and the max (voltage swing + pre-emphasis) is
  // 3. According to the v1.1a of the DP docs, if v + pe is too large then v should be reduced
  // to the highest supported value for the pe level (section 3.5.1.3)
  static constexpr uint32_t kMaxVoltageSwingPlusPreEmphasis = 3;
  if (voltage_swing + pre_emphasis > kMaxVoltageSwingPlusPreEmphasis) {
    voltage_swing = static_cast<uint8_t>(kMaxVoltageSwingPlusPreEmphasis - pre_emphasis);
  }
  const uint8_t max_port_voltage = controller()->igd_opregion().IsLowVoltageEdp(ddi_id()) ? 3 : 2;
  if (voltage_swing > max_port_voltage) {
    voltage_swing = max_port_voltage;
  }

  for (int lane_index = 0; lane_index < dp_lane_count_; lane_index++) {
    voltage_changed |= (training[lane_index].voltage_swing_set() != voltage_swing);
    training[lane_index].set_voltage_swing_set(voltage_swing);
    training[lane_index].set_max_swing_reached(voltage_swing == max_port_voltage);
    training[lane_index].set_pre_emphasis_set(pre_emphasis);
    training[lane_index].set_max_pre_emphasis_set(pre_emphasis + voltage_swing ==
                                                  kMaxVoltageSwingPlusPreEmphasis);
  }

  // Compute the index into the PHY configuration table.
  static constexpr int kFirstEntryForVoltageSwingLevel[] = {0, 4, 7, 9};

  // The array access is safe because `voltage_swing` + `pre_emphasis` is at
  // most 3. For the same reason, each (voltage_swing, pre_emphasis) index will
  // result in a different entry
  const int phy_config_index = kFirstEntryForVoltageSwingLevel[voltage_swing] + pre_emphasis;
  ZX_ASSERT(phy_config_index < 10);
  if (phy_config_index == 9) {
    // Entry 9 in the PHY configuration table is only usable for DisplayPort on
    // DDIs A and E, to support eDP displays. On DDIs B-D, entry 9 is dedicated
    // to HDMI.
    //
    // Voltage swing level 3 is only valid for eDP, so we should be on DDI A or
    // E, and should be servicing an eDP port.
    ZX_ASSERT(controller()->igd_opregion().IsLowVoltageEdp(ddi_id()));
    ZX_ASSERT(ddi_id() == 0 || ddi_id() == 4);
  }

  if (is_tgl(controller()->device_id())) {
    ConfigureVoltageSwingTigerLake(phy_config_index);
  } else {
    ConfigureVoltageSwingKabyLake(phy_config_index);
  }

  return voltage_changed;
}

void DpDisplay::ConfigureVoltageSwingKabyLake(size_t phy_config_index) {
  ZX_DEBUG_ASSERT_MSG(phy_config_index <= std::numeric_limits<uint32_t>::max(),
                      "%zu overflows uint32_t", phy_config_index);
  registers::DdiRegs ddi_regs(ddi_id());
  auto buffer_control = ddi_regs.BufferControl().ReadFrom(mmio_space());
  buffer_control.set_display_port_phy_config_kaby_lake(static_cast<uint32_t>(phy_config_index));
  buffer_control.WriteTo(mmio_space());
}

void DpDisplay::ConfigureVoltageSwingTigerLake(size_t phy_config_index) {
  switch (ddi_id()) {
    case DdiId::DDI_TC_1:
    case DdiId::DDI_TC_2:
    case DdiId::DDI_TC_3:
    case DdiId::DDI_TC_4:
    case DdiId::DDI_TC_5:
    case DdiId::DDI_TC_6:
      ConfigureVoltageSwingTypeCTigerLake(phy_config_index);
      return;
    case DdiId::DDI_A:
    case DdiId::DDI_B:
    case DdiId::DDI_C:
      ConfigureVoltageSwingComboTigerLake(phy_config_index);
      return;
    default:
      ZX_DEBUG_ASSERT_MSG(false, "Unreachable");
      return;
  }
}

void DpDisplay::ConfigureVoltageSwingTypeCTigerLake(size_t phy_config_index) {
  // This table is from "Voltage Swing Programming Sequence > DP Voltage Swing
  // Table" Section of Intel Display Programming Manual. It contains control
  // register fields for each Voltage Swing Config.
  //
  // Tiger Lake: IHD-OS-TGL-Vol 12-1.22-Rev 2.0
  struct VoltageSwingConfig {
    uint32_t vswing_control = 0;
    uint32_t preshoot_control = 0;
    uint32_t de_emphasis_control = 0;
  };
  constexpr VoltageSwingConfig kVoltageSwingConfigTable[] = {
      {.vswing_control = 0x7, .preshoot_control = 0x0, .de_emphasis_control = 0x0},
      {.vswing_control = 0x5, .preshoot_control = 0x0, .de_emphasis_control = 0x5},
      {.vswing_control = 0x2, .preshoot_control = 0x0, .de_emphasis_control = 0xB},
      // Assume HBR2 is always used for Voltage Swing Level 0, Pre-emphasis 3
      {.vswing_control = 0x0, .preshoot_control = 0x0, .de_emphasis_control = 0x19},
      {.vswing_control = 0x5, .preshoot_control = 0x0, .de_emphasis_control = 0x0},
      {.vswing_control = 0x2, .preshoot_control = 0x0, .de_emphasis_control = 0x8},
      {.vswing_control = 0x0, .preshoot_control = 0x0, .de_emphasis_control = 0x14},
      {.vswing_control = 0x2, .preshoot_control = 0x0, .de_emphasis_control = 0x0},
      {.vswing_control = 0x0, .preshoot_control = 0x0, .de_emphasis_control = 0xB},
      {.vswing_control = 0x0, .preshoot_control = 0x0, .de_emphasis_control = 0x0},
  };

  for (auto tx_lane : {0, 1}) {
    // Flush PMD_LANE_SUS register if display owns this PHY lane.
    registers::DekelTransmitterPmdLaneSus::GetForLaneDdi(tx_lane, ddi_id())
        .FromValue(0)
        .WriteTo(mmio_space());

    // Update DisplayPort control registers with appropriate voltage swing and
    // de-emphasis levels from the table.
    auto display_port_control_0 =
        registers::DekelTransmitterDisplayPortControl0::GetForLaneDdi(tx_lane, ddi_id())
            .ReadFrom(mmio_space());
    display_port_control_0
        .set_voltage_swing_control_level_transmitter_1(
            kVoltageSwingConfigTable[phy_config_index].vswing_control)
        .set_preshoot_coefficient_transmitter_1(
            kVoltageSwingConfigTable[phy_config_index].preshoot_control)
        .set_de_emphasis_coefficient_transmitter_1(
            kVoltageSwingConfigTable[phy_config_index].de_emphasis_control)
        .WriteTo(mmio_space());

    auto display_port_control_1 =
        registers::DekelTransmitterDisplayPortControl1::GetForLaneDdi(tx_lane, ddi_id())
            .ReadFrom(mmio_space());
    display_port_control_1
        .set_voltage_swing_control_level_transmitter_2(
            kVoltageSwingConfigTable[phy_config_index].vswing_control)
        .set_preshoot_coefficient_transmitter_2(
            kVoltageSwingConfigTable[phy_config_index].preshoot_control)
        .set_de_emphasis_coefficient_transmitter_2(
            kVoltageSwingConfigTable[phy_config_index].de_emphasis_control)
        .WriteTo(mmio_space());

    auto display_port_control_2 =
        registers::DekelTransmitterDisplayPortControl2::GetForLaneDdi(tx_lane, ddi_id())
            .ReadFrom(mmio_space());
    display_port_control_2.set_display_port_20bit_mode_supported(0).WriteTo(mmio_space());
  }
}

void DpDisplay::ConfigureVoltageSwingComboTigerLake(size_t phy_config_index) {
  // This implements the "Digital Display Interface" > "Combo PHY DDI Buffer" >
  // "Voltage Swing Programming Sequence" section in the display PRMs.
  //
  // Tiger Lake: IHD-OS-TGL-Vol 12-1.22-Rev2.0 pages 392-395
  // DG1: IHD-OS-DG1-Vol 12-2.21 pages 338-342
  // Ice Lake: IHD-OS-ICLLP-Vol 12-1.22-Rev2.0 pages 335-339

  fdf::trace("Voltage Swing for DDI {}, Link rate {} MHz, PHY config: {}", ddi_id(),
             dp_link_rate_mhz_, static_cast<int>(phy_config_index));
  fdf::trace("Logging pre-configuration register state for debugging");

  static constexpr registers::PortLane kMainLinkLanes[] = {
      registers::PortLane::kMainLinkLane0, registers::PortLane::kMainLinkLane1,
      registers::PortLane::kMainLinkLane2, registers::PortLane::kMainLinkLane3};
  for (registers::PortLane lane : kMainLinkLanes) {
    auto physical_coding1 =
        registers::PortPhysicalCoding1::GetForDdiLane(ddi_id(), lane).ReadFrom(mmio_space());
    const int lane_index =
        static_cast<int>(lane) - static_cast<int>(registers::PortLane::kMainLinkLane0);
    fdf::trace("DDI {} Lane {} PORT_PCS_DW1: {:08x}, common mode keeper: {}", ddi_id(), lane_index,
               physical_coding1.reg_value(),
               physical_coding1.common_mode_keeper_enabled() ? "enabled" : "disabled");
    physical_coding1.set_common_mode_keeper_enabled(true).WriteTo(mmio_space());
  }

  cpp20::span<const bool> load_generation;
  if (dp_link_rate_mhz_ >= 6'000) {
    static constexpr bool kHighSpeedLoadGeneration[] = {false, false, false, false};
    load_generation = kHighSpeedLoadGeneration;
  } else if (dp_lane_count_ == 4) {
    static constexpr bool kLowSpeedFullLinkLoadGeneration[] = {false, true, true, true};
    load_generation = kLowSpeedFullLinkLoadGeneration;
  } else {
    static constexpr bool kPartialLinkLoadGeneration[] = {false, true, true, false};
    load_generation = kPartialLinkLoadGeneration;
  }
  for (registers::PortLane lane : kMainLinkLanes) {
    auto lane_equalization = registers::PortTransmitterEqualization::GetForDdiLane(ddi_id(), lane)
                                 .ReadFrom(mmio_space());
    const int lane_index =
        static_cast<int>(lane) - static_cast<int>(registers::PortLane::kMainLinkLane0);
    fdf::trace(
        "DDI {} Lane {} PORT_TX_DW4: {:08x}, load generation select: {}, equalization "
        "C0: {:02x} C1: {:02x} C2: {:02x}",
        ddi_id(), lane_index, lane_equalization.reg_value(),
        lane_equalization.load_generation_select(), lane_equalization.cursor_coefficient(),
        lane_equalization.post_cursor_coefficient1(), lane_equalization.post_cursor_coefficient2());
    lane_equalization.set_load_generation_select(load_generation[lane_index]).WriteTo(mmio_space());
  }

  auto common_lane5 = registers::PortCommonLane5::GetForDdi(ddi_id()).ReadFrom(mmio_space());
  fdf::trace("DDI {} PORT_CL_DW5 {:08x}, suspend clock config {}", ddi_id(),
             common_lane5.reg_value(), common_lane5.suspend_clock_config());
  common_lane5.set_suspend_clock_config(0b11).WriteTo(mmio_space());

  // Lane training must be disabled while we configure new voltage settings into
  // the AFE (Analog Front-End) registers.
  for (registers::PortLane lane : kMainLinkLanes) {
    auto lane_voltage =
        registers::PortTransmitterVoltage::GetForDdiLane(ddi_id(), lane).ReadFrom(mmio_space());
    const int lane_index =
        static_cast<int>(lane) - static_cast<int>(registers::PortLane::kMainLinkLane0);
    fdf::trace(
        "DDI {} Lane {} PORT_TX_DW5: {:08x}, scaling mode select: {}, "
        "terminating resistor select: {}, equalization 3-tap: {} 2-tap: {}, "
        "cursor programming: {}, coefficient polarity: {}",
        ddi_id(), lane_index, lane_voltage.reg_value(), lane_voltage.scaling_mode_select(),
        lane_voltage.terminating_resistor_select(),
        lane_voltage.three_tap_equalization_disabled() ? "disabled" : "enabled",
        lane_voltage.two_tap_equalization_disabled() ? "disabled" : "enabled",
        lane_voltage.cursor_programming_disabled() ? "disabled" : "enabled",
        lane_voltage.coefficient_polarity_disabled() ? "disabled" : "enabled");
    lane_voltage.set_training_enabled(false).WriteTo(mmio_space());
  }

  // The ordering of the fields matches the column order in the "Voltage Swing
  // Programming" table. The post-cursor is omitted because it can be derived by
  // solving the equation cursor + post_cursor = 0x3f. It is not surprising that
  // the coefficients of a 2-tap equalizer add up to (a fixed-point
  // representation of) 1.
  struct ComboSwingConfig {
    uint8_t swing_select;
    uint8_t n_scalar;
    uint8_t cursor;
  };

  cpp20::span<const ComboSwingConfig> swing_configs;
  // TODO(https://fxbug.dev/42065201):
  const int use_edp_voltages = false;
  if (use_edp_voltages) {
    if (dp_link_rate_mhz_ <= 5'400) {  // Up to HBR2
      static constexpr ComboSwingConfig kEmbeddedDisplayPortHbr2Configs[] = {
          // Voltage swing 0, pre-emphasis levels 0-3
          {.swing_select = 0b0000, .n_scalar = 0x7f, .cursor = 0x3f},
          {.swing_select = 0b1000, .n_scalar = 0x7f, .cursor = 0x38},
          {.swing_select = 0b0001, .n_scalar = 0x7f, .cursor = 0x33},
          {.swing_select = 0b1001, .n_scalar = 0x7f, .cursor = 0x31},

          // Voltage swing 1, pre-emphasis levels 0-2
          {.swing_select = 0b1000, .n_scalar = 0x7f, .cursor = 0x3f},
          {.swing_select = 0b0001, .n_scalar = 0x7f, .cursor = 0x38},
          {.swing_select = 0b1001, .n_scalar = 0x7f, .cursor = 0x33},

          // Voltage swing 2, pre-emphasis levels 0-1
          {.swing_select = 0b0001, .n_scalar = 0x7f, .cursor = 0x3f},
          {.swing_select = 0b1001, .n_scalar = 0x7f, .cursor = 0x38},

          // Voltage swing 3, pre-emphasis level 0
          {.swing_select = 0b1001, .n_scalar = 0x7f, .cursor = 0x3f},

          // Optimized config, opt-in via VBT.
          // TODO(https://fxbug.dev/42065768): This entry is currently unused.
          {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x3f},
      };
      swing_configs = kEmbeddedDisplayPortHbr2Configs;
    } else {  // Up to HBR3
      // The "XED Overview" > "Port Configurations" section on
      // IHD-OS-TGL-Vol 12-1.22-Rev2.0 page 113 states that combo PHYs support
      // HBR3, but only for eDP (Embedded DisplayPort). DisplayPort connections
      // can only go up to HBR2.
      static constexpr ComboSwingConfig kEmbeddedDisplayPortHbr3Configs[] = {
          // Voltage swing 0, pre-emphasis levels 0-3
          {.swing_select = 0b1010, .n_scalar = 0x35, .cursor = 0x3f},
          {.swing_select = 0b1010, .n_scalar = 0x4f, .cursor = 0x37},
          {.swing_select = 0b1100, .n_scalar = 0x71, .cursor = 0x2f},
          {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x2b},

          // Voltage swing 1, pre-emphasis levels 0-2
          {.swing_select = 0b1010, .n_scalar = 0x4c, .cursor = 0x3f},
          {.swing_select = 0b1100, .n_scalar = 0x73, .cursor = 0x34},
          {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x2f},

          // Voltage swing 2, pre-emphasis levels 0-1
          {.swing_select = 0b1100, .n_scalar = 0x6c, .cursor = 0x3f},
          {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x35},

          // Voltage swing 3, pre-emphasis level 0
          {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x3f},
      };
      swing_configs = kEmbeddedDisplayPortHbr3Configs;
    }
  } else {
    if (dp_link_rate_mhz_ <= 2'700) {  // Up to HBR
      static constexpr ComboSwingConfig kDisplayPortHbrConfigs[] = {
          // Voltage swing 0, pre-emphasis levels 0-3
          {.swing_select = 0b1010, .n_scalar = 0x32, .cursor = 0x3f},
          {.swing_select = 0b1010, .n_scalar = 0x4f, .cursor = 0x37},
          {.swing_select = 0b1100, .n_scalar = 0x71, .cursor = 0x2f},
          {.swing_select = 0b0110, .n_scalar = 0x7d, .cursor = 0x2b},

          // Voltage swing 1, pre-emphasis levels 0-2
          {.swing_select = 0b1010, .n_scalar = 0x4c, .cursor = 0x3f},
          {.swing_select = 0b1100, .n_scalar = 0x73, .cursor = 0x34},
          {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x2f},

          // Voltage swing 2, pre-emphasis levels 0-1
          {.swing_select = 0b1100, .n_scalar = 0x4c, .cursor = 0x3c},
          {.swing_select = 0b0110, .n_scalar = 0x73, .cursor = 0x35},

          // Voltage swing 3, pre-emphasis level 0
          {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x3f},
      };
      swing_configs = kDisplayPortHbrConfigs;
    } else {  // Up to HBR2
      if (dp_link_rate_mhz_ >= 5'400) {
        // TODO(https://fxbug.dev/42065925): DpDisplay::ComputeDdiPllConfig() should
        // reject configs that would entail HBR3 on DisplayPort. Then we can
        // have a ZX_ASSERT() / ZX_DEBUG_ASSERT() here.
        fdf::warn(
            "Attempting to use unsupported DisplayPort speed on DDI {} which tops out at HBR2",
            ddi_id());
      }

      // The IHD-OS-TGL-Vol 12-1.22-Rev2.0 "Voltage Swing Programming" table on
      // pages 393-395 has an ambiguity -- there are two sets of entries labeled
      // "DP HBR2", without any further explanation.
      //
      // We resolve this ambiguity based on the OpenBSD i915 driver, which (in
      // intel_ddi_buf_trans.c) uses the 2nd set of entries for "U/Y" SKUs, and
      // the 1st set of entries for all other processors.
      //
      // Y SKUs seem to be undocumented / unreleased, since they're not listed
      // in the IHD-OS-TGL-Vol 4-12.21 "Steppings and Device IDs" table on page
      // 9. So, we're using the 2nd set of entries for the U SKUs, and the first
      // set of entries for the H SKUs.
      const uint16_t device_id = controller()->device_id();

      // TODO(https://fxbug.dev/42065924): PCI device ID-based selection is insufficient.
      // Display engines with PCI device ID 0x9a49 may be UP3 or H35 SKUs.
      if (is_tgl_u(device_id)) {
        static constexpr ComboSwingConfig kDisplayPortHbr2UConfigs[] = {
            // Voltage swing 0, pre-emphasis levels 0-3
            {.swing_select = 0b1010, .n_scalar = 0x35, .cursor = 0x3f},
            {.swing_select = 0b1010, .n_scalar = 0x4f, .cursor = 0x36},
            {.swing_select = 0b1100, .n_scalar = 0x60, .cursor = 0x32},
            {.swing_select = 0b1100, .n_scalar = 0x7f, .cursor = 0x2d},

            // Voltage swing 1, pre-emphasis levels 0-2
            {.swing_select = 0b1100, .n_scalar = 0x47, .cursor = 0x3f},
            {.swing_select = 0b1100, .n_scalar = 0x6f, .cursor = 0x36},
            {.swing_select = 0b0110, .n_scalar = 0x7d, .cursor = 0x32},

            // Voltage swing 2, pre-emphasis levels 0-1
            {.swing_select = 0b0110, .n_scalar = 0x60, .cursor = 0x3c},
            {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x34},

            // Voltage swing 3, pre-emphasis level 0
            {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x3f},
        };
        swing_configs = kDisplayPortHbr2UConfigs;
      } else {
        static constexpr ComboSwingConfig kDisplayPortHbr2HConfigs[] = {
            // Voltage swing 0, pre-emphasis levels 0-3
            {.swing_select = 0b1010, .n_scalar = 0x35, .cursor = 0x3f},
            {.swing_select = 0b1010, .n_scalar = 0x4f, .cursor = 0x37},
            {.swing_select = 0b1100, .n_scalar = 0x63, .cursor = 0x2f},
            {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x2b},

            // Voltage swing 1, pre-emphasis levels 0-2
            {.swing_select = 0b1010, .n_scalar = 0x47, .cursor = 0x3f},
            {.swing_select = 0b1100, .n_scalar = 0x63, .cursor = 0x34},
            {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x2f},

            // Voltage swing 2, pre-emphasis levels 0-1
            {.swing_select = 0b1100, .n_scalar = 0x61, .cursor = 0x3c},
            {.swing_select = 0b0110, .n_scalar = 0x7b, .cursor = 0x35},

            // Voltage swing 3, pre-emphasis level 0
            {.swing_select = 0b0110, .n_scalar = 0x7f, .cursor = 0x3f},
        };
        swing_configs = kDisplayPortHbr2HConfigs;
      }
    }
  }

  const ComboSwingConfig& swing_config = swing_configs[phy_config_index];
  for (registers::PortLane lane : kMainLinkLanes) {
    const int lane_index =
        static_cast<int>(lane) - static_cast<int>(registers::PortLane::kMainLinkLane0);

    auto lane_voltage_swing = registers::PortTransmitterVoltageSwing::GetForDdiLane(ddi_id(), lane)
                                  .ReadFrom(mmio_space());
    fdf::trace("DDI {} Lane {} PORT_TX_DW2: {:08x}, Rcomp scalar: {:02x}, Swing select: {}",
               ddi_id(), lane_index, lane_voltage_swing.reg_value(),
               lane_voltage_swing.resistance_compensation_code_scalar(),
               lane_voltage_swing.voltage_swing_select());
    lane_voltage_swing.set_resistance_compensation_code_scalar(0x98)
        .set_voltage_swing_select(swing_config.swing_select)
        .WriteTo(mmio_space());

    auto lane_equalization = registers::PortTransmitterEqualization::GetForDdiLane(ddi_id(), lane)
                                 .ReadFrom(mmio_space());
    lane_equalization.set_cursor_coefficient(swing_config.cursor)
        .set_post_cursor_coefficient1(0x3f - swing_config.cursor)
        .set_post_cursor_coefficient2(0)
        .WriteTo(mmio_space());

    auto lane_voltage =
        registers::PortTransmitterVoltage::GetForDdiLane(ddi_id(), lane).ReadFrom(mmio_space());
    lane_voltage.set_scaling_mode_select(2)
        .set_terminating_resistor_select(6)
        .set_three_tap_equalization_disabled(true)
        .set_two_tap_equalization_disabled(false)
        .set_cursor_programming_disabled(false)
        .set_coefficient_polarity_disabled(false)
        .WriteTo(mmio_space());

    auto lane_n_scalar =
        registers::PortTransmitterNScalar::GetForDdiLane(ddi_id(), lane).ReadFrom(mmio_space());
    fdf::trace("DDI {} Lane {} PORT_TX_DW7: {:08x}, N Scalar: {:02x}", ddi_id(), lane_index,
               lane_n_scalar.reg_value(), lane_n_scalar.n_scalar());
  }

  // Re-enabling training causes the AFE (Analog Front-End) to pick up the new
  // voltage configuration.
  for (registers::PortLane lane : kMainLinkLanes) {
    auto lane_voltage =
        registers::PortTransmitterVoltage::GetForDdiLane(ddi_id(), lane).ReadFrom(mmio_space());
    lane_voltage.set_training_enabled(true);
  }

  // This step follows voltage swing configuration in the "Sequences for
  // DisplayPort" > "Enable Sequence" section in the display engine PRMs.
  auto common_lane_main_link_power =
      registers::PortCommonLaneMainLinkPower::GetForDdi(ddi_id()).ReadFrom(mmio_space());
  fdf::trace(
      "DDI {} PORT_CL_DW10 {:08x}, lanes: 0 {} 1 {} 2 {} 3 {}, eDP power-optimized {} {}, "
      "terminating resistor {} {} Ohm",
      ddi_id(), common_lane_main_link_power.reg_value(),
      common_lane_main_link_power.power_down_lane0() ? "off" : "on",
      common_lane_main_link_power.power_down_lane1() ? "off" : "on",
      common_lane_main_link_power.power_down_lane2() ? "off" : "on",
      common_lane_main_link_power.power_down_lane3() ? "off" : "on",
      common_lane_main_link_power.edp_power_optimized_mode_valid() ? "valid" : "invalid",
      common_lane_main_link_power.edp_power_optimized_mode_enabled() ? "enabled" : "disabled",
      common_lane_main_link_power.terminating_resistor_override_valid() ? "valid" : "invalid",
      (common_lane_main_link_power.terminating_resistor_override() ==
       registers::PortCommonLaneMainLinkPower::TerminatingResistorOverride::k100Ohms)
          ? 100
          : 150);
  if (phy_config_index == 10) {
    common_lane_main_link_power.set_edp_power_optimized_mode_valid(true)
        .set_edp_power_optimized_mode_enabled(true);
  }
  common_lane_main_link_power.set_powered_up_lanes(dp_lane_count_).WriteTo(mmio_space());
}

bool DpDisplay::LinkTrainingSetupTigerLake() {
  ZX_ASSERT(capabilities_);
  ZX_ASSERT(is_tgl(controller()->device_id()));
  ZX_ASSERT_MSG(pipe(), "LinkTrainingSetup: Display doesn't have valid pipe");

  // Follow the "Enable and Train DisplayPort" procedure at Section
  // "Sequences for DisplayPort > Enable Sequence":
  //
  // Tiger Lake: IHD-OS-TGL-Vol 12-1.22-Rev 2.0, Page 144

  // Transcoder must be disabled while doing link training.
  registers::TranscoderRegs transcoder_regs(pipe()->connected_transcoder_id());

  // Our experiments on NUC 11 indicate that the display engine may crash the
  // whole system if the driver sets `enabled_target` to false and writes the
  // transcoder configuration register when the transcoder is already disabled,
  // so we avoid crashing the system by only writing the register when the
  // transcoder is currently enabled.
  auto transcoder_config = transcoder_regs.Config().ReadFrom(mmio_space());
  if (transcoder_config.enabled()) {
    transcoder_config.set_enabled_target(false).WriteTo(mmio_space());
  }

  // Configure "Transcoder Clock Select" to direct the Port clock to the
  // transcoder.
  auto clock_select = transcoder_regs.ClockSelect().ReadFrom(mmio_space());
  clock_select.set_ddi_clock_tiger_lake(ddi_id());
  clock_select.WriteTo(mmio_space());

  // Configure "Transcoder DDI Control" to select DDI and DDI mode.
  auto ddi_control = transcoder_regs.DdiControl().ReadFrom(mmio_space());
  ddi_control.set_ddi_tiger_lake(ddi_id());
  // TODO(https://fxbug.dev/42061773): Support MST (Multi-Stream).
  ddi_control.set_ddi_mode(registers::TranscoderDdiControl::kModeDisplayPortSingleStream);
  ddi_control.WriteTo(mmio_space());

  // Configure and enable "DP Transport Control" register with link training
  // pattern 1 selected
  auto dp_transport_control =
      registers::DpTransportControl::GetForTigerLakeTranscoder(pipe()->connected_transcoder_id())
          .ReadFrom(mmio_space());
  dp_transport_control.set_enabled(true)
      .set_is_multi_stream(false)
      .set_sst_enhanced_framing(capabilities_->enhanced_frame_capability())
      .set_training_pattern(registers::DpTransportControl::kTrainingPattern1)
      .WriteTo(mmio_space());

  // Start link training at the minimum Voltage Swing level.
  ConfigureVoltageSwingTigerLake(/*phy_config_index=*/0);

  // TODO(https://fxbug.dev/42056448): On PRM it mentions that, for COMBO PHY, the driver
  // needs to configure PORT_CL_DW10 Static Power Down to power up the used
  // lanes of the DDI.

  // Configure and enable DDI Buffer.
  auto buffer_control =
      registers::DdiBufferControl::GetForTigerLakeDdi(ddi_id()).ReadFrom(mmio_space());
  buffer_control.set_enabled(true)
      .set_display_port_lane_count(dp_lane_count_)
      .WriteTo(mmio_space());

  // Wait for DDI Buffer to be enabled, timeout after 1 ms.
  if (!display::PollUntil([&] { return !buffer_control.ReadFrom(mmio_space()).is_idle(); },
                          zx::usec(1), 1000)) {
    fdf::error("DDI_BUF_CTL DDI idle status timeout");
    return false;
  }

  // Configure DPCD registers.
  //
  // VESA DP Standard v1.4a Section 3.5.1.2 "Link Training" (Page 618) describes
  // the procedure for link training.
  //
  // This function contains the procedure before starting the link training
  // tasks (Clock recovery and Channel equalization).

  // Configure Link rate / Link bandwidth.
  uint16_t link_rate_reg;
  uint8_t link_rate_val;
  if (dp_link_rate_table_idx_) {
    dpcd::LinkRateSet link_rate_set;
    link_rate_set.set_link_rate_idx(static_cast<uint8_t>(dp_link_rate_table_idx_.value()));
    link_rate_reg = dpcd::DPCD_LINK_RATE_SET;
    link_rate_val = link_rate_set.reg_value();
  } else {
    uint8_t target_bw;
    if (dp_link_rate_mhz_ == 1620) {
      target_bw = dpcd::LinkBw::k1620Mbps;
    } else if (dp_link_rate_mhz_ == 2700) {
      target_bw = dpcd::LinkBw::k2700Mbps;
    } else if (dp_link_rate_mhz_ == 5400) {
      target_bw = dpcd::LinkBw::k5400Mbps;
    } else {
      ZX_ASSERT_MSG(dp_link_rate_mhz_ == 8100, "Unrecognized DP link rate: %d Mbps/lane",
                    dp_link_rate_mhz_);
      target_bw = dpcd::LinkBw::k8100Mbps;
    }

    dpcd::LinkBw bw_setting;
    bw_setting.set_link_bw(target_bw);
    link_rate_reg = dpcd::DPCD_LINK_BW_SET;
    link_rate_val = bw_setting.reg_value();
  }

  // Configure the bandwidth and lane count settings
  dpcd::LaneCount lc_setting;
  lc_setting.set_lane_count_set(dp_lane_count_);
  lc_setting.set_enhanced_frame_enabled(capabilities_->enhanced_frame_capability());
  if (!DpcdWrite(link_rate_reg, &link_rate_val, 1) ||
      !DpcdWrite(dpcd::DPCD_COUNT_SET, lc_setting.reg_value_ptr(), 1)) {
    fdf::error("DP: Link training: failed to configure settings");
    return false;
  }

  // TODO(https://fxbug.dev/42060757): The procedure above doesn't fully match that
  // described in VESA DP Standard v1.4a. For example, DOWNSPREAD_CTRL and
  // MAIN_LINK_CHANNEL_CODING_SET registers are not set.
  return true;
}

bool DpDisplay::LinkTrainingSetupKabyLake() {
  ZX_ASSERT(capabilities_);
  ZX_DEBUG_ASSERT(!is_tgl(controller()->device_id()));

  registers::DdiRegs ddi_regs(ddi_id());

  // Tell the source device to emit the training pattern.
  auto dp_transport_control = ddi_regs.DpTransportControl().ReadFrom(mmio_space());
  dp_transport_control.set_enabled(true)
      .set_is_multi_stream(false)
      .set_sst_enhanced_framing(capabilities_->enhanced_frame_capability())
      .set_training_pattern(registers::DpTransportControl::kTrainingPattern1)
      .WriteTo(mmio_space());

  // Configure DDI PHY parameters (voltage swing and pre-emphasis).
  //
  // Kaby Lake: IHD-OS-KBL-Vol 12-1.17 pages 187-190
  // Skylake: IHD-OS-SKL-Vol 12-05.16 pages 181-183
  // TODO(https://fxbug.dev/42106274): Read the VBT to handle unique motherboard configs for kaby
  // lake
  uint8_t i_boost;
  const cpp20::span<const DdiPhyConfigEntry> entries =
      controller()->igd_opregion().IsLowVoltageEdp(ddi_id())
          ? GetEdpPhyConfigEntries(controller()->device_id(), &i_boost)
          : GetDpPhyConfigEntries(controller()->device_id(), &i_boost);
  const uint8_t i_boost_override = controller()->igd_opregion().GetIBoost(ddi_id(), /*is_dp=*/true);

  for (int entry_index = 0; entry_index < static_cast<int>(entries.size()); ++entry_index) {
    auto phy_config_entry1 =
        registers::DdiPhyConfigEntry1::GetDdiInstance(ddi_id(), entry_index).FromValue(0);
    phy_config_entry1.set_reg_value(entries[entry_index].entry1);
    if (i_boost_override) {
      phy_config_entry1.set_balance_leg_enable(1);
    }
    phy_config_entry1.WriteTo(mmio_space());

    auto phy_config_entry2 =
        registers::DdiPhyConfigEntry2::GetDdiInstance(ddi_id(), entry_index).FromValue(0);
    phy_config_entry2.set_reg_value(entries[entry_index].entry2).WriteTo(mmio_space());
  }

  const uint8_t i_boost_val = i_boost_override ? i_boost_override : i_boost;
  auto balance_control = registers::DdiPhyBalanceControl::Get().ReadFrom(mmio_space());
  balance_control.set_disable_balance_leg(!i_boost && !i_boost_override);
  balance_control.balance_leg_select_for_ddi(ddi_id()).set(i_boost_val);
  if (ddi_id() == DdiId::DDI_A && dp_lane_count_ == 4) {
    balance_control.balance_leg_select_for_ddi(DdiId::DDI_E).set(i_boost_val);
  }
  balance_control.WriteTo(mmio_space());

  // Enable and wait for DDI_BUF_CTL
  auto buffer_control = ddi_regs.BufferControl().ReadFrom(mmio_space());
  buffer_control.set_enabled(true)
      .set_display_port_phy_config_kaby_lake(0)
      .set_display_port_lane_count(dp_lane_count_)
      .WriteTo(mmio_space());
  zx_nanosleep(zx_deadline_after(ZX_USEC(518)));

  uint16_t link_rate_reg;
  uint8_t link_rate_val;
  if (dp_link_rate_table_idx_) {
    dpcd::LinkRateSet link_rate_set;
    link_rate_set.set_link_rate_idx(static_cast<uint8_t>(dp_link_rate_table_idx_.value()));
    link_rate_reg = dpcd::DPCD_LINK_RATE_SET;
    link_rate_val = link_rate_set.reg_value();
  } else {
    uint8_t target_bw;
    if (dp_link_rate_mhz_ == 1620) {
      target_bw = dpcd::LinkBw::k1620Mbps;
    } else if (dp_link_rate_mhz_ == 2700) {
      target_bw = dpcd::LinkBw::k2700Mbps;
    } else if (dp_link_rate_mhz_ == 5400) {
      target_bw = dpcd::LinkBw::k5400Mbps;
    } else {
      ZX_ASSERT_MSG(dp_link_rate_mhz_ == 8100, "Unrecognized DP link rate: %d Mbps/lane",
                    dp_link_rate_mhz_);
      target_bw = dpcd::LinkBw::k8100Mbps;
    }

    dpcd::LinkBw bw_setting;
    bw_setting.set_link_bw(target_bw);
    link_rate_reg = dpcd::DPCD_LINK_BW_SET;
    link_rate_val = bw_setting.reg_value();
  }

  // Configure the bandwidth and lane count settings
  dpcd::LaneCount lc_setting;
  lc_setting.set_lane_count_set(dp_lane_count_);
  lc_setting.set_enhanced_frame_enabled(capabilities_->enhanced_frame_capability());
  if (!DpcdWrite(link_rate_reg, &link_rate_val, 1) ||
      !DpcdWrite(dpcd::DPCD_COUNT_SET, lc_setting.reg_value_ptr(), 1)) {
    fdf::error("DP: Link training: failed to configure settings");
    return false;
  }

  return true;
}

// Number of times to poll with the same voltage level configured, as
// specified by the DisplayPort spec.
static const int kPollsPerVoltageLevel = 5;

bool DpDisplay::LinkTrainingStage1(dpcd::TrainingPatternSet* tp_set, dpcd::TrainingLaneSet* lanes) {
  ZX_ASSERT(capabilities_);

  // Tell the sink device to look for the training pattern.
  tp_set->set_training_pattern_set(tp_set->kTrainingPattern1);
  tp_set->set_scrambling_disable(1);

  dpcd::AdjustRequestLane adjust_req[kMaxDisplayPortLaneCount];
  dpcd::LaneStatus lane_status[kMaxDisplayPortLaneCount];

  int poll_count = 0;
  auto delay =
      capabilities_->dpcd_reg<dpcd::TrainingAuxRdInterval, dpcd::DPCD_TRAINING_AUX_RD_INTERVAL>();
  for (;;) {
    if (!DpcdRequestLinkTraining(*tp_set, lanes)) {
      return false;
    }

    zx_nanosleep(
        zx_deadline_after(ZX_USEC(delay.clock_recovery_delay_us(capabilities_->dpcd_revision()))));

    // Did the sink device receive the signal successfully?
    if (!DpcdReadPairedRegs<dpcd::DPCD_LANE0_1_STATUS, dpcd::LaneStatus>(lane_status)) {
      return false;
    }
    bool done = true;
    for (unsigned i = 0; i < dp_lane_count_; i++) {
      done &= lane_status[i].lane_cr_done(i).get();
    }
    if (done) {
      break;
    }

    for (unsigned i = 0; i < dp_lane_count_; i++) {
      if (lanes[i].max_swing_reached()) {
        fdf::error("DP: Link training: max voltage swing reached");
        return false;
      }
    }

    if (!DpcdReadPairedRegs<dpcd::DPCD_ADJUST_REQUEST_LANE0_1, dpcd::AdjustRequestLane>(
            adjust_req)) {
      return false;
    }

    if (DpcdHandleAdjustRequest(lanes, adjust_req)) {
      poll_count = 0;
    } else if (++poll_count == kPollsPerVoltageLevel) {
      fdf::error("DP: Link training: clock recovery step failed");
      return false;
    }
  }

  return true;
}

bool DpDisplay::LinkTrainingStage2(dpcd::TrainingPatternSet* tp_set, dpcd::TrainingLaneSet* lanes) {
  ZX_ASSERT(capabilities_);

  dpcd::AdjustRequestLane adjust_req[kMaxDisplayPortLaneCount];
  dpcd::LaneStatus lane_status[kMaxDisplayPortLaneCount];

  if (is_tgl(controller()->device_id())) {
    auto dp_transport_control =
        registers::DpTransportControl::GetForTigerLakeTranscoder(pipe()->connected_transcoder_id())
            .ReadFrom(mmio_space());
    dp_transport_control.set_training_pattern(registers::DpTransportControl::kTrainingPattern2);
    dp_transport_control.WriteTo(mmio_space());
  } else {
    registers::DdiRegs ddi_regs(ddi_id());
    auto dp_transport_control = ddi_regs.DpTransportControl().ReadFrom(mmio_space());
    dp_transport_control.set_training_pattern(registers::DpTransportControl::kTrainingPattern2);
    dp_transport_control.WriteTo(mmio_space());
  }

  (*tp_set)
      .set_training_pattern_set(dpcd::TrainingPatternSet::kTrainingPattern2)
      .set_scrambling_disable(1);
  int poll_count = 0;
  auto delay =
      capabilities_->dpcd_reg<dpcd::TrainingAuxRdInterval, dpcd::DPCD_TRAINING_AUX_RD_INTERVAL>();
  for (;;) {
    // lane0_training and lane1_training can change in the loop
    if (!DpcdRequestLinkTraining(*tp_set, lanes)) {
      return false;
    }

    zx_nanosleep(zx_deadline_after(ZX_USEC(delay.channel_eq_delay_us())));

    // Did the sink device receive the signal successfully?
    if (!DpcdReadPairedRegs<dpcd::DPCD_LANE0_1_STATUS, dpcd::LaneStatus>(lane_status)) {
      return false;
    }
    for (unsigned i = 0; i < dp_lane_count_; i++) {
      if (!lane_status[i].lane_cr_done(i).get()) {
        fdf::error("DP: Link training: clock recovery regressed");
        return false;
      }
    }

    bool symbol_lock_done = true;
    bool channel_eq_done = true;
    for (unsigned i = 0; i < dp_lane_count_; i++) {
      symbol_lock_done &= lane_status[i].lane_symbol_locked(i).get();
      channel_eq_done &= lane_status[i].lane_channel_eq_done(i).get();
      // TODO(https://fxbug.dev/42060757): The driver should also check interlane align
      // done bits.
    }
    if (symbol_lock_done && channel_eq_done) {
      break;
    }

    // The training attempt has not succeeded yet.
    if (++poll_count == kPollsPerVoltageLevel) {
      if (!symbol_lock_done) {
        fdf::error("DP: Link training: symbol lock failed");
      }
      if (!channel_eq_done) {
        fdf::error("DP: Link training: channel equalization failed");
      }
      return false;
    }

    if (!DpcdReadPairedRegs<dpcd::DPCD_ADJUST_REQUEST_LANE0_1, dpcd::AdjustRequestLane>(
            adjust_req)) {
      return false;
    }
    DpcdHandleAdjustRequest(lanes, adjust_req);
  }

  if (is_tgl(controller()->device_id())) {
    auto dp_transport_control =
        registers::DpTransportControl::GetForTigerLakeTranscoder(pipe()->connected_transcoder_id())
            .ReadFrom(mmio_space());
    dp_transport_control.set_training_pattern(registers::DpTransportControl::kSendPixelData);
    dp_transport_control.WriteTo(mmio_space());
  } else {
    registers::DdiRegs ddi_regs(ddi_id());
    auto dp_transport_control = ddi_regs.DpTransportControl().ReadFrom(mmio_space());
    dp_transport_control.set_training_pattern(registers::DpTransportControl::kSendPixelData)
        .WriteTo(mmio_space());
    dp_transport_control.WriteTo(mmio_space());
  }

  return true;
}

bool DpDisplay::ProgramDpModeTigerLake() {
  ZX_ASSERT(ddi_id() >= DdiId::DDI_TC_1);
  ZX_ASSERT(ddi_id() <= DdiId::DDI_TC_6);

  auto dp_mode_0 =
      registers::DekelDisplayPortMode::GetForLaneDdi(0, ddi_id()).ReadFrom(mmio_space());
  auto dp_mode_1 =
      registers::DekelDisplayPortMode::GetForLaneDdi(1, ddi_id()).ReadFrom(mmio_space());

  auto pin_assignment = registers::DynamicFlexIoDisplayPortPinAssignment::GetForDdi(ddi_id())
                            .ReadFrom(mmio_space())
                            .pin_assignment_for_ddi(ddi_id());
  if (!pin_assignment.has_value()) {
    fdf::error("Cannot get pin assignment for ddi {}", ddi_id());
    return false;
  }

  // Reset DP lane mode.
  dp_mode_0.set_x1_mode(0).set_x2_mode(0);
  dp_mode_1.set_x1_mode(0).set_x2_mode(0);

  switch (*pin_assignment) {
    using PinAssignment = registers::DynamicFlexIoDisplayPortPinAssignment::PinAssignment;
    case PinAssignment::kNone:  // Fixed/Static
      if (dp_lane_count_ == 1) {
        dp_mode_1.set_x1_mode(1);
      } else {
        dp_mode_0.set_x2_mode(1);
        dp_mode_1.set_x2_mode(1);
      }
      break;
    case PinAssignment::kA:
      if (dp_lane_count_ == 4) {
        dp_mode_0.set_x2_mode(1);
        dp_mode_1.set_x2_mode(1);
      }
      break;
    case PinAssignment::kB:
      if (dp_lane_count_ == 2) {
        dp_mode_0.set_x2_mode(1);
        dp_mode_1.set_x2_mode(1);
      }
      break;
    case PinAssignment::kC:
    case PinAssignment::kE:
      if (dp_lane_count_ == 1) {
        dp_mode_0.set_x1_mode(1);
        dp_mode_1.set_x1_mode(1);
      } else {
        dp_mode_0.set_x2_mode(1);
        dp_mode_1.set_x2_mode(1);
      }
      break;
    case PinAssignment::kD:
    case PinAssignment::kF:
      if (dp_lane_count_ == 1) {
        dp_mode_0.set_x1_mode(1);
        dp_mode_1.set_x1_mode(1);
      } else {
        dp_mode_0.set_x2_mode(1);
        dp_mode_1.set_x2_mode(1);
      }
      break;
  }

  dp_mode_0.WriteTo(mmio_space());
  dp_mode_1.WriteTo(mmio_space());
  return true;
}

bool DpDisplay::DoLinkTraining() {
  // TODO(https://fxbug.dev/42106274): If either of the two training steps fails, we're
  // supposed to try with a reduced bit rate.
  bool result = true;
  if (is_tgl(controller()->device_id())) {
    result &= LinkTrainingSetupTigerLake();
  } else {
    result &= LinkTrainingSetupKabyLake();
  }
  if (result) {
    dpcd::TrainingPatternSet tp_set;
    dpcd::TrainingLaneSet lanes[kMaxDisplayPortLaneCount];
    result &= LinkTrainingStage1(&tp_set, lanes);
    result &= LinkTrainingStage2(&tp_set, lanes);
  }

  // Tell the sink device to end its link training attempt.
  //
  // If link training was successful, we need to do this so that the sink
  // device will accept pixel data from the source device.
  //
  // If link training was not successful, we want to do this so that
  // subsequent link training attempts can work.  If we don't unset this
  // register, subsequent link training attempts can also fail.  (This
  // can be important during development.  The sink device won't
  // necessarily get reset when the computer is reset.  This means that a
  // bad version of the driver can leave the sink device in a state where
  // good versions subsequently don't work.)
  uint32_t addr = dpcd::DPCD_TRAINING_PATTERN_SET;
  uint8_t reg_byte = 0;
  if (!DpcdWrite(addr, &reg_byte, sizeof(reg_byte))) {
    fdf::error("Failure setting TRAINING_PATTERN_SET");
    return false;
  }

  return result;
}

namespace {

// Convert ratio x/y into the form used by the Link/Data M/N ratio registers.
void CalculateRatio(int64_t x, int64_t y, uint32_t* m_out, uint32_t* n_out) {
  // The exact values of N and M shouldn't matter too much.  N and M can be
  // up to 24 bits, and larger values will tend to represent the ratio more
  // accurately. However, large values of N (e.g. 1 << 23) cause some monitors
  // to inexplicably fail. Pick a relatively arbitrary value for N that works
  // well in practice.
  ZX_DEBUG_ASSERT(x >= 0);
  ZX_DEBUG_ASSERT(y > 0);
  *n_out = 1 << 20;
  *m_out = static_cast<uint32_t>(x * *n_out / y);
}

bool IsEdp(Controller* controller, DdiId ddi_id) {
  return controller && controller->igd_opregion().IsEdp(ddi_id);
}

}  // namespace

DpDisplay::DpDisplay(Controller* controller, display::DisplayId id, DdiId ddi_id,
                     DpAuxChannel* dp_aux_channel, PchEngine* pch_engine,
                     DdiReference ddi_reference, inspect::Node* parent_node)
    : DisplayDevice(controller, id, ddi_id, std::move(ddi_reference),
                    IsEdp(controller, ddi_id) ? Type::kEdp : Type::kDp),
      dp_aux_channel_(dp_aux_channel),
      pch_engine_(type() == Type::kEdp ? pch_engine : nullptr) {
  ZX_ASSERT(dp_aux_channel);
  if (type() == Type::kEdp) {
    ZX_ASSERT(pch_engine_ != nullptr);
  } else {
    ZX_ASSERT(pch_engine_ == nullptr);
  }

  inspect_node_ = parent_node->CreateChild(fbl::StringPrintf("dp-display-%lu", id.value()));
  dp_capabilities_node_ = inspect_node_.CreateChild("dpcd-capabilities");
  dp_lane_count_inspect_ = inspect_node_.CreateUint("dp_lane_count", 0);
  dp_link_rate_mhz_inspect_ = inspect_node_.CreateUint("dp_link_rate_mhz", 0);
}

DpDisplay::~DpDisplay() = default;

bool DpDisplay::Query() {
  // For eDP displays, assume that the BIOS has enabled panel power, given
  // that we need to rely on it properly configuring panel power anyway. For
  // general DP displays, the default power state is D0, so we don't have to
  // worry about AUX failures because of power saving mode.
  {
    fpromise::result<DpCapabilities> capabilities = DpCapabilities::Read(dp_aux_channel_);
    if (capabilities.is_error()) {
      return false;
    }

    capabilities.value().PublishToInspect(&dp_capabilities_node_);
    capabilities_ = capabilities.take_value();
  }

  switch (capabilities_->sink_count()) {
    case 0:
      fdf::error(
          "No DisplayPort Sink devices detected on DDI {}. No DisplayDevice will "
          "be created.",
          ddi_id());
      return false;
    case 1:
      break;
    default:
      // TODO(https://fxbug.dev/42106274): Add support for MST.
      fdf::error(
          "Multiple ({}) DisplayPort Sink devices detected on DDI {}. DisplayPort "
          "Multi-Stream Transport is not supported yet.",
          capabilities_->sink_count(), ddi_id());
      return false;
  }

  uint8_t lane_count = capabilities_->max_lane_count();
  if (is_tgl(controller()->device_id())) {
    lane_count =
        std::min(lane_count, ddi_reference()->GetPhysicalLayerInfo().max_allowed_dp_lane_count);
  } else {
    // On Kaby Lake and Skylake, DDI E takes over two of DDI A's four lanes. In
    // other words, if DDI E is enabled, DDI A only has two lanes available. DDI E
    // always has two lanes available.
    //
    // Kaby Lake: IHD-OS-KBL-Vol 12-1.17 "Display Connections" > "DDIs" page 107
    // Skylake: IHD-OS-SKL-Vol 12-05.16 "Display Connections" > "DDIs" page 105
    if (ddi_id() == DdiId::DDI_A || ddi_id() == DdiId::DDI_E) {
      const bool ddi_e_enabled = !registers::DdiRegs(DdiId::DDI_A)
                                      .BufferControl()
                                      .ReadFrom(mmio_space())
                                      .ddi_e_disabled_kaby_lake();
      if (ddi_e_enabled) {
        lane_count = std::min<uint8_t>(lane_count, 2);
      }
    }
  }

  ZX_DEBUG_ASSERT(lane_count <= kMaxDisplayPortLaneCount);
  dp_lane_count_ = lane_count;
  dp_lane_count_inspect_.Set(lane_count);

  ZX_ASSERT(!dp_link_rate_table_idx_.has_value());
  ZX_ASSERT(!capabilities_->supported_link_rates_mbps().empty());

  zx::result<fbl::Vector<uint8_t>> read_extended_edid_result =
      ReadExtendedEdid(fit::bind_member<&DpAuxChannel::ReadEdidBlock>(dp_aux_channel_));
  if (read_extended_edid_result.is_error()) {
    fdf::error("Failed to read E-EDID: {}", read_extended_edid_result);
    return false;
  }
  edid_bytes_ = std::move(read_extended_edid_result).value();

  uint8_t last = static_cast<uint8_t>(capabilities_->supported_link_rates_mbps().size() - 1);
  fdf::info("Found {} monitor (max link rate: {} MHz, lane count: {})",
            (type() == Type::kEdp ? "eDP" : "DP"), capabilities_->supported_link_rates_mbps()[last],
            dp_lane_count_);

  return true;
}

bool DpDisplay::InitDdi() {
  ZX_ASSERT(capabilities_);

  if (type() == Type::kEdp) {
    if (!EnsureEdpPanelIsPoweredOn()) {
      return false;
    }
  }

  if (capabilities_->dpcd_revision() >= dpcd::Revision::k1_1) {
    // If the device is in a low power state, the first write can fail. It should be ready
    // within 1ms, but try a few extra times to be safe.
    dpcd::SetPower set_pwr;
    set_pwr.set_set_power_state(set_pwr.kOn);
    int count = 0;
    while (!DpcdWrite(dpcd::DPCD_SET_POWER, set_pwr.reg_value_ptr(), 1) && ++count < 5) {
      zx_nanosleep(zx_deadline_after(ZX_MSEC(1)));
    }
    if (count >= 5) {
      fdf::error("Failed to set dp power state");
      return ZX_ERR_INTERNAL;
    }
  }

  // Note that we always initialize the port and train the links regardless of
  // the display status.
  //
  // It is tempting to avoid port initialization and link training if the
  // DPCD_INTERLANE_ALIGN_DONE bit of DPCD_LANE_ALIGN_STATUS_UPDATED register
  // is set to 1.
  //
  // One could hope to skip this step when using a connection that has already
  // been configured by the boot firmware. However, since we reset DDIs, it is
  // not safe to skip training.

  // 3.b. Program DFLEXDPMLE.DPMLETC* to maximum number of lanes allowed as determined by
  // FIA and panel lane count.
  if (is_tgl(controller()->device_id()) && ddi_id() >= DdiId::DDI_TC_1 &&
      ddi_id() <= DdiId::DDI_TC_6) {
    auto main_link_lane_enabled =
        registers::DynamicFlexIoDisplayPortMainLinkLaneEnabled::GetForDdi(ddi_id()).ReadFrom(
            mmio_space());
    switch (dp_lane_count_) {
      case 1:
        main_link_lane_enabled.set_enabled_display_port_main_link_lane_bits(ddi_id(), 0b0001);
        break;
      case 2:
        // 1100b cannot be used with Type-C Alt connections.
        main_link_lane_enabled.set_enabled_display_port_main_link_lane_bits(ddi_id(), 0b0011);
        break;
      case 4:
        main_link_lane_enabled.set_enabled_display_port_main_link_lane_bits(ddi_id(), 0b1111);
        break;
      default:
        ZX_DEBUG_ASSERT(false);
    }
    main_link_lane_enabled.WriteTo(mmio_space());
  }

  // Determine the current link rate if one hasn't been assigned.
  if (dp_link_rate_mhz_ == 0) {
    ZX_ASSERT(!capabilities_->supported_link_rates_mbps().empty());

    // Pick the maximum supported link rate.
    uint8_t index = static_cast<uint8_t>(capabilities_->supported_link_rates_mbps().size() - 1);
    uint32_t lane_link_rate_mbps = capabilities_->supported_link_rates_mbps()[index];

    // When there are 4 lanes, the link training failure rate when using 5.4GHz
    // link rate is very high. So we limit the maximum link rate here.
    if (dp_lane_count_ == 4) {
      lane_link_rate_mbps = std::min(2700u, lane_link_rate_mbps);
    }

    fdf::info("Selected maximum supported DisplayPort link rate: {} Mbps/lane",
              lane_link_rate_mbps);
    SetLinkRate(lane_link_rate_mbps);
    if (capabilities_->use_link_rate_table()) {
      dp_link_rate_table_idx_ = index;
    }
  }

  const DdiPllConfig pll_config = DdiPllConfig{
      .ddi_clock_khz = static_cast<int32_t>((dp_link_rate_mhz_ * 1'000) / 2),
      .spread_spectrum_clocking = false,
      .admits_display_port = true,
      .admits_hdmi = false,
  };

  // 4. Enable Port PLL
  DisplayPll* dpll =
      controller()->dpll_manager()->SetDdiPllConfig(ddi_id(), type() == Type::kEdp, pll_config);
  if (dpll == nullptr) {
    fdf::error("Cannot find an available DPLL for DP display on DDI {}", ddi_id());
    return false;
  }

  // 5. Enable power for this DDI.
  controller()->power()->SetDdiIoPowerState(ddi_id(), /* enable */ true);
  if (!display::PollUntil([&] { return controller()->power()->GetDdiIoPowerState(ddi_id()); },
                          zx::usec(1), 20)) {
    fdf::error("Failed to enable IO power for ddi");
    return false;
  }

  // 6. Program DP mode
  // This step only applies to Type-C DDIs in non-Thunderbolt mode.
  const auto phy_info = ddi_reference()->GetPhysicalLayerInfo();
  if (is_tgl(controller()->device_id()) && phy_info.ddi_type == DdiPhysicalLayer::DdiType::kTypeC &&
      phy_info.connection_type != DdiPhysicalLayer::ConnectionType::kTypeCThunderbolt &&
      !ProgramDpModeTigerLake()) {
    fdf::error("DDI {}: Cannot program DP mode", ddi_id());
    return false;
  }

  // 7. Do link training
  if (!DoLinkTraining()) {
    fdf::error("DDI {}: DisplayPort link training failed", ddi_id());
    return false;
  }

  return true;
}

bool DpDisplay::InitWithDdiPllConfig(const DdiPllConfig& pll_config) {
  if (pll_config.IsEmpty()) {
    return false;
  }

  ZX_DEBUG_ASSERT(pll_config.admits_display_port);
  if (!pll_config.admits_display_port) {
    fdf::error("DpDisplay::InitWithDdiPllConfig() - incompatible PLL configuration");
    return false;
  }

  Pipe* pipe = controller()->pipe_manager()->RequestPipeFromHardwareState(*this, mmio_space());
  if (pipe == nullptr) {
    fdf::error("Failed loading pipe from register!");
    return false;
  }
  set_pipe(pipe);

  // Some display (e.g. eDP) may have already been configured by the bootloader with a
  // link clock. Assign the link rate based on the already enabled DPLL.
  if (dp_link_rate_mhz_ == 0) {
    int32_t dp_link_rate_mhz = (pll_config.ddi_clock_khz * 2) / 1'000;
    // Since the link rate is read from the register directly, we can guarantee
    // that it is always valid.
    fdf::info("Selected pre-configured DisplayPort link rate: {} Mbps/lane", dp_link_rate_mhz);
    SetLinkRate(dp_link_rate_mhz);
  }
  return true;
}

DdiPllConfig DpDisplay::ComputeDdiPllConfig(int32_t pixel_clock_khz) {
  return DdiPllConfig{
      .ddi_clock_khz = static_cast<int32_t>(static_cast<int32_t>(dp_link_rate_mhz_) * 500),
      .spread_spectrum_clocking = false,
      .admits_display_port = true,
      .admits_hdmi = false,
  };
}

bool DpDisplay::DdiModeset(const display::DisplayTiming& mode) { return true; }

bool DpDisplay::PipeConfigPreamble(const display::DisplayTiming& mode, PipeId pipe_id,
                                   TranscoderId transcoder_id) {
  registers::TranscoderRegs transcoder_regs(transcoder_id);

  // Transcoder should be disabled first before reconfiguring the transcoder
  // clock. Will be re-enabled at `PipeConfigEpilogue()`.
  auto transcoder_config = transcoder_regs.Config().ReadFrom(mmio_space());
  transcoder_config.set_enabled(false).WriteTo(mmio_space());
  transcoder_config.ReadFrom(mmio_space());

  // Step "Enable Planes, Pipe, and Transcoder" in the "Sequences for
  // DisplayPort" > "Enable Sequence" section of Intel's display documentation.
  //
  // Tiger Lake: IHD-OS-TGL-Vol 12-1.22-Rev2.0 page 144
  // Kaby Lake: IHD-OS-KBL-Vol 12-1.17 page 114
  // Skylake: IHD-OS-SKL-Vol 12-05.16 page 112
  if (is_tgl(controller()->device_id())) {
    // On Tiger Lake, the transcoder clock for SST (Single-Stream) mode is set
    // during the "Enable and Train DisplayPort" step (done before this method
    // is called). This is because Tiger Lake transcoders contain the
    // DisplayPort Transport modules used for link training.
    auto clock_select = transcoder_regs.ClockSelect().ReadFrom(mmio_space());
    const std::optional<DdiId> ddi_clock_source = clock_select.ddi_clock_tiger_lake();
    if (!ddi_clock_source.has_value()) {
      fdf::error("Transcoder {} clock source not set after DisplayPort training", transcoder_id);
      return false;
    }
    if (*ddi_clock_source != ddi_id()) {
      fdf::error("Transcoder {} clock set to DDI {} instead of {} after DisplayPort training.",
                 transcoder_id, ddi_id(), *ddi_clock_source);
      return false;
    }
  } else {
    // On Kaby Lake and Skylake, the transcoder clock input must be set during
    // the pipe, plane and transcoder enablement stage.
    if (transcoder_id != TranscoderId::TRANSCODER_EDP) {
      auto clock_select = transcoder_regs.ClockSelect().ReadFrom(mmio_space());
      clock_select.set_ddi_clock_kaby_lake(ddi_id());
      clock_select.WriteTo(mmio_space());
    }
  }

  // Pixel clock rate: The rate at which pixels are sent, in pixels per
  // second, divided by 1000 (kHz).
  const int64_t pixel_clock_rate_khz = mode.pixel_clock_frequency_hz / 1'000;

  // This is the rate at which bits are sent on a single DisplayPort
  // lane, in raw bits per second, divided by 1000 (kbps).
  int64_t link_raw_bit_rate_kbps = dp_link_rate_mhz_ * int64_t{1000};

  // Link symbol rate: The rate at which link symbols are sent on a
  // single DisplayPort lane, in symbols per second, divided by 1000 (kHz).
  //
  // A link symbol is 10 raw bits (using 8b/10b encoding, which usually encodes
  // an 8-bit data byte).
  int64_t link_symbol_rate_khz = link_raw_bit_rate_kbps / 10;

  // Configure ratios between pixel clock/bit rate and symbol clock/bit rate
  uint32_t link_m;
  uint32_t link_n;
  CalculateRatio(pixel_clock_rate_khz, link_symbol_rate_khz, &link_m, &link_n);

  // Computing the M/N ratios is covered in the "Transcoder" > "Transcoder MN
  // Values" section in the PRMs. The current implementation covers the
  // straight-forward case - no reduced horizontal blanking, no DSC (Display
  // Stream Compression), no FEC (Forward Error Correction).
  //
  // Tiger Lake: IHD-OS-TGL-Vol 12-1.22-Rev2.0 pages 330-332
  // Kaby Lake: IHD-OS-KBL-Vol 12-1.17 pages 174-176
  // Skylake: IHD-OS-SKL-Vol 12-05.16 page 171-172

  int64_t pixel_bit_rate_kbps = pixel_clock_rate_khz * kBitsPerPixel;
  int64_t total_link_bit_rate_kbps = link_symbol_rate_khz * 8 * dp_lane_count_;

  ZX_DEBUG_ASSERT(pixel_bit_rate_kbps <=
                  total_link_bit_rate_kbps);  // Should be caught by CheckPixelRate

  uint32_t data_m;
  uint32_t data_n;
  CalculateRatio(pixel_bit_rate_kbps, total_link_bit_rate_kbps, &data_m, &data_n);

  auto data_m_reg = transcoder_regs.DataM().FromValue(0);
  data_m_reg.set_payload_size(64);  // The default TU size is 64.
  data_m_reg.set_m(data_m);
  data_m_reg.WriteTo(mmio_space());

  transcoder_regs.DataN().FromValue(0).set_n(data_n).WriteTo(mmio_space());
  transcoder_regs.LinkM().FromValue(0).set_m(link_m).WriteTo(mmio_space());
  transcoder_regs.LinkN().FromValue(0).set_n(link_n).WriteTo(mmio_space());

  return true;
}

bool DpDisplay::PipeConfigEpilogue(const display::DisplayTiming& mode, PipeId pipe_id,
                                   TranscoderId transcoder_id) {
  registers::TranscoderRegs transcoder_regs(transcoder_id);
  auto main_stream_attribute_misc = transcoder_regs.MainStreamAttributeMisc().FromValue(0);
  main_stream_attribute_misc.set_video_stream_clock_sync_with_link_clock(true)
      .set_colorimetry_in_vsc_sdp(false)
      .set_colorimetry_top_bit(0);

  // TODO(https://fxbug.dev/42166519): Decide the color model / pixel format based on pipe
  //                        configuration and display capabilities.
  main_stream_attribute_misc
      .set_bits_per_component_select(registers::DisplayPortMsaBitsPerComponent::k8Bpc)
      .set_colorimetry_select(registers::DisplayPortMsaColorimetry::kRgbUnspecifiedLegacy)
      .WriteTo(mmio_space());

  auto transcoder_ddi_control = transcoder_regs.DdiControl().ReadFrom(mmio_space());
  transcoder_ddi_control.set_enabled(true);

  // The EDP transcoder ignores the DDI select field, because it's always
  // connected to DDI A. Since the field is ignored (as opposed to reserved),
  // it's still OK to set it. We set it to None, because it seems less misleadng
  // than setting it to one of the other DDIs.
  const std::optional<DdiId> transcoder_ddi =
      (transcoder_id == TranscoderId::TRANSCODER_EDP) ? std::nullopt : std::make_optional(ddi_id());
  if (is_tgl(controller()->device_id())) {
    ZX_DEBUG_ASSERT_MSG(transcoder_id != TranscoderId::TRANSCODER_EDP,
                        "The EDP transcoder does not exist on this display engine");
    transcoder_ddi_control.set_ddi_tiger_lake(transcoder_ddi);
  } else {
    ZX_DEBUG_ASSERT_MSG(transcoder_id != TranscoderId::TRANSCODER_EDP || ddi_id() == DdiId::DDI_A,
                        "The EDP transcoder is attached to DDI A");
    transcoder_ddi_control.set_ddi_kaby_lake(transcoder_ddi);
  }

  // TODO(https://fxbug.dev/42166519): Decide the color model / pixel format based on pipe
  //                        configuration and display capabilities.
  transcoder_ddi_control.set_ddi_mode(registers::TranscoderDdiControl::kModeDisplayPortSingleStream)
      .set_bits_per_color(registers::TranscoderDdiControl::k8bpc)
      .set_vsync_polarity_not_inverted(mode.vsync_polarity == display::SyncPolarity::kPositive)
      .set_hsync_polarity_not_inverted(mode.hsync_polarity == display::SyncPolarity::kPositive);

  if (!is_tgl(controller()->device_id())) {
    // Fields that only exist on Kaby Lake and Skylake.
    transcoder_ddi_control.set_is_port_sync_secondary_kaby_lake(false);
  }

  // The input pipe field is ignored on all transcoders except for EDP (on Kaby
  // Lake and Skylake) and DSI (on Tiger Lake, not yet supported by our driver).
  // Since the field is ignored (as opposed to reserved), it's OK to still set
  // it everywhere.
  transcoder_ddi_control.set_input_pipe_id(pipe_id);

  transcoder_ddi_control.set_allocate_display_port_virtual_circuit_payload(false)
      .set_display_port_lane_count(dp_lane_count_)
      .WriteTo(mmio_space());

  auto transcoder_config = transcoder_regs.Config().FromValue(0);
  transcoder_config.set_enabled_target(true)
      .set_interlaced_display(mode.fields_per_frame == display::FieldsPerFrame::kInterlaced)
      .WriteTo(mmio_space());

  return true;
}

bool DpDisplay::InitBacklightHw() {
  if (capabilities_ && capabilities_->backlight_aux_brightness()) {
    dpcd::EdpBacklightModeSet mode;
    mode.set_brightness_ctrl_mode(mode.kAux);
    if (!DpcdWrite(dpcd::DPCD_EDP_BACKLIGHT_MODE_SET, mode.reg_value_ptr(), 1)) {
      fdf::error("Failed to init backlight");
      return false;
    }
  }
  return true;
}

bool DpDisplay::SetBacklightOn(bool backlight_on) {
  if (type() != Type::kEdp) {
    return true;
  }

  if (capabilities_ && capabilities_->backlight_aux_power()) {
    dpcd::EdpDisplayCtrl ctrl;
    ctrl.set_backlight_enable(backlight_on);
    if (!DpcdWrite(dpcd::DPCD_EDP_DISPLAY_CTRL, ctrl.reg_value_ptr(), 1)) {
      fdf::error("Failed to enable backlight");
      return false;
    }
  } else {
    pch_engine_->SetPanelPowerTarget({
        .power_on = true,
        .backlight_on = backlight_on,
        .force_power_on = false,
        .brightness_pwm_counter_on = backlight_on,
    });
  }

  return !backlight_on || SetBacklightBrightness(backlight_brightness_);
}

bool DpDisplay::IsBacklightOn() {
  // If there is no embedded display, return false.
  if (type() != Type::kEdp) {
    return false;
  }

  if (capabilities_ && capabilities_->backlight_aux_power()) {
    dpcd::EdpDisplayCtrl ctrl;

    if (!DpcdRead(dpcd::DPCD_EDP_DISPLAY_CTRL, ctrl.reg_value_ptr(), 1)) {
      fdf::error("Failed to read backlight");
      return false;
    }

    return ctrl.backlight_enable();
  } else {
    return pch_engine_->PanelPowerTarget().backlight_on;
  }
}

bool DpDisplay::SetBacklightBrightness(double val) {
  if (type() != Type::kEdp) {
    return true;
  }

  backlight_brightness_ = std::max(val, controller()->igd_opregion().GetMinBacklightBrightness());
  backlight_brightness_ = std::min(backlight_brightness_, 1.0);

  if (capabilities_ && capabilities_->backlight_aux_brightness()) {
    uint16_t percent = static_cast<uint16_t>(0xffff * backlight_brightness_ + .5);

    uint8_t lsb = static_cast<uint8_t>(percent & 0xff);
    uint8_t msb = static_cast<uint8_t>(percent >> 8);
    if (!DpcdWrite(dpcd::DPCD_EDP_BACKLIGHT_BRIGHTNESS_MSB, &msb, 1) ||
        !DpcdWrite(dpcd::DPCD_EDP_BACKLIGHT_BRIGHTNESS_LSB, &lsb, 1)) {
      fdf::error("Failed to set backlight brightness");
      return false;
    }
  } else {
    pch_engine_->SetPanelBrightness(val);
  }

  return true;
}

double DpDisplay::GetBacklightBrightness() {
  if (!HasBacklight()) {
    return 0;
  }

  double percent = 0;

  if (capabilities_ && capabilities_->backlight_aux_brightness()) {
    uint8_t lsb;
    uint8_t msb;
    if (!DpcdRead(dpcd::DPCD_EDP_BACKLIGHT_BRIGHTNESS_MSB, &msb, 1) ||
        !DpcdRead(dpcd::DPCD_EDP_BACKLIGHT_BRIGHTNESS_LSB, &lsb, 1)) {
      fdf::error("Failed to read backlight brightness");
      return 0;
    }

    uint16_t brightness = static_cast<uint16_t>((lsb & 0xff) | (msb << 8));

    percent = (brightness * 1.0f) / 0xffff;

  } else {
    percent = pch_engine_->PanelBrightness();
  }

  return percent;
}

bool DpDisplay::HandleHotplug(bool long_pulse) {
  if (!long_pulse) {
    // On short pulse, query the panel and then proceed as required by panel

    dpcd::SinkCount sink_count;
    if (!DpcdRead(dpcd::DPCD_SINK_COUNT, sink_count.reg_value_ptr(), 1)) {
      fdf::warn("Failed to read sink count on hotplug");
      return false;
    }

    // The pulse was from a downstream monitor being connected
    // TODO(https://fxbug.dev/42106274): Add support for MST
    if (sink_count.count() > 1) {
      return true;
    }

    // The pulse was from a downstream monitor disconnecting
    if (sink_count.count() == 0) {
      return false;
    }

    dpcd::LaneAlignStatusUpdate status;
    if (!DpcdRead(dpcd::DPCD_LANE_ALIGN_STATUS_UPDATED, status.reg_value_ptr(), 1)) {
      fdf::warn("Failed to read align status on hotplug");
      return false;
    }

    if (status.interlane_align_done()) {
      fdf::debug("HPD event for trained link");
      return true;
    }

    return DoLinkTraining();
  }

  // Handle long pulse.
  //
  // On Tiger Lake Type C ports, if the hotplug interrupt has a long pulse,
  // it should read DFlex DP Scratch Pad register to find the port live state,
  // and connect / disconnect the display accordingly.
  //
  // Tiger Lake: IHD-OS-TGL-Vol 12-1.22-Rev 2.0, Page 203, "HPD Interrupt
  //             Sequence"
  if (is_tgl(controller()->device_id()) && ddi_id() >= DdiId::DDI_TC_1 &&
      ddi_id() <= DdiId::DDI_TC_6) {
    auto dp_sp = registers::DynamicFlexIoScratchPad::GetForDdi(ddi_id()).ReadFrom(mmio_space());
    auto type_c_live_state = dp_sp.type_c_live_state(ddi_id());

    // The device has been already connected when `HandleHotplug` is called.
    // If live state is non-zero, keep the existing connection; otherwise
    // return false to disconnect the display.
    return type_c_live_state !=
           registers::DynamicFlexIoScratchPad::TypeCLiveState::kNoHotplugDisplay;
  }

  // On other platforms, a long pulse indicates that the hotplug status is
  // toggled. So we disconnect the existing display.
  return false;
}

bool DpDisplay::HasBacklight() { return type() == Type::kEdp; }

zx::result<> DpDisplay::SetBacklightState(bool power, double brightness) {
  SetBacklightOn(power);

  brightness = std::max(brightness, 0.0);
  brightness = std::min(brightness, 1.0);

  double range = 1.0f - controller()->igd_opregion().GetMinBacklightBrightness();
  if (!SetBacklightBrightness((range * brightness) +
                              controller()->igd_opregion().GetMinBacklightBrightness())) {
    return zx::error(ZX_ERR_IO);
  }
  return zx::success();
}

zx::result<fuchsia_hardware_backlight::wire::State> DpDisplay::GetBacklightState() {
  return zx::success(fuchsia_hardware_backlight::wire::State{
      .backlight_on = IsBacklightOn(),
      .brightness = GetBacklightBrightness(),
  });
}

void DpDisplay::SetLinkRate(uint32_t value) {
  dp_link_rate_mhz_ = value;
  dp_link_rate_mhz_inspect_.Set(value);
}

bool DpDisplay::CheckPixelRate(int64_t pixel_rate_hz) {
  int64_t bit_rate_hz = (dp_link_rate_mhz_ * int64_t{1'000'000}) * dp_lane_count_;
  // Multiply by 8/10 because of 8b/10b encoding
  int64_t max_pixel_rate_hz = (bit_rate_hz * 8 / 10) / kBitsPerPixel;
  return pixel_rate_hz >= 0 && pixel_rate_hz <= max_pixel_rate_hz;
}

int32_t DpDisplay::LoadPixelRateForTranscoderKhz(TranscoderId transcoder_id) {
  registers::TranscoderRegs transcoder_regs(transcoder_id);
  const uint32_t data_m = transcoder_regs.DataM().ReadFrom(mmio_space()).m();
  const uint32_t data_n = transcoder_regs.DataN().ReadFrom(mmio_space()).n();

  double dp_link_rate_khz = dp_link_rate_mhz_ * 1000.0;
  double total_link_bit_rate_khz = dp_link_rate_khz * (8.0 / 10.0) * dp_lane_count_;
  double pixel_clock_rate_khz = (data_m * total_link_bit_rate_khz) / (data_n * kBitsPerPixel);
  return static_cast<int32_t>(round(pixel_clock_rate_khz));
}

raw_display_info_t DpDisplay::CreateRawDisplayInfo() {
  return raw_display_info_t{
      .display_id = display::ToBanjoDisplayId(id()),
      .preferred_modes_list = nullptr,
      .preferred_modes_count = 0,
      .edid_bytes_list = edid_bytes_.data(),
      .edid_bytes_count = edid_bytes_.size(),
      .pixel_formats_list = kBanjoSupportedPixelFormats.data(),
      .pixel_formats_count = kBanjoSupportedPixelFormats.size(),
  };
}

}  // namespace intel_display

// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ti-lp8556.h"

#include <fidl/fuchsia.hardware.adhoc.lp8556/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/ddk/metadata.h>
#include <lib/ddk/platform-defs.h>
#include <lib/device-protocol/display-panel.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/mock-mmio/cpp/region.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/mock-i2c/mock-i2c-gtest.h>

#include <cmath>

#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <mock-mmio-reg/mock-mmio-reg.h>

#include "src/devices/testing/mock-ddk/mock-device.h"
#include "src/lib/testing/predicates/status.h"

namespace ti {

constexpr uint32_t kMmioRegSize = sizeof(uint32_t);
constexpr uint32_t kMmioRegCount = (kAOBrightnessStickyReg + kMmioRegSize) / kMmioRegSize;

class Lp8556DeviceTest : public ::testing::Test {
 public:
  Lp8556DeviceTest()
      : mock_regs_(mock_mmio::Region(kMmioRegSize, kMmioRegCount)),
        fake_parent_(MockDevice::FakeRootParent()),
        loop_(&kAsyncLoopConfigNeverAttachToThread),
        i2c_loop_(&kAsyncLoopConfigNeverAttachToThread) {}

  void SetUp() override {
    fdf::MmioBuffer mmio(mock_regs_.GetMmioBuffer());

    auto i2c_endpoints = fidl::CreateEndpoints<fuchsia_hardware_i2c::Device>();
    fidl::BindServer(i2c_loop_.dispatcher(), std::move(i2c_endpoints->server), &mock_i2c_);

    fbl::AllocChecker ac;
    dev_ = fbl::make_unique_checked<Lp8556Device>(
        &ac, fake_parent_.get(), std::move(i2c_endpoints->client), std::move(mmio));
    ASSERT_TRUE(ac.check());

    zx::result server = fidl::CreateEndpoints(&client_);
    ASSERT_OK(server);
    fidl::BindServer(loop_.dispatcher(), std::move(server.value()), dev_.get());

    ASSERT_OK(loop_.StartThread("lp8556-client-thread"));
    ASSERT_OK(i2c_loop_.StartThread("mock-i2c-driver-thread"));
  }

  void TestLifecycle() {
    EXPECT_OK(dev_->DdkAdd("ti-lp8556"));
    EXPECT_EQ(fake_parent_->child_count(), 1u);
    dev_->DdkAsyncRemove();
    EXPECT_OK(mock_ddk::ReleaseFlaggedDevices(fake_parent_.get()));  // Calls DdkRelease() on dev_.
    [[maybe_unused]] auto ptr = dev_.release();
    EXPECT_EQ(fake_parent_->child_count(), 0u);
  }

  void VerifyGetBrightness(bool power, double brightness) {
    bool pwr;
    double brt;
    EXPECT_OK(dev_->GetBacklightState(&pwr, &brt));
    EXPECT_EQ(pwr, power);
    EXPECT_EQ(brt, brightness);
  }

  void VerifySetBrightness(bool power, double brightness) {
    if (brightness != dev_->GetDeviceBrightness()) {
      uint16_t brightness_reg_value =
          static_cast<uint16_t>(ceil(brightness * kBrightnessRegMaxValue));
      mock_i2c_.ExpectWriteStop({kBacklightBrightnessLsbReg,
                                 static_cast<uint8_t>(brightness_reg_value & kBrightnessLsbMask)});
      // An I2C bus read is a write of the address followed by a read of the data.
      mock_i2c_.ExpectWrite({kBacklightBrightnessMsbReg}).ExpectReadStop({0});
      mock_i2c_.ExpectWriteStop(
          {kBacklightBrightnessMsbReg,
           static_cast<uint8_t>(
               ((brightness_reg_value & kBrightnessMsbMask) >> kBrightnessMsbShift) &
               kBrightnessMsbByteMask)});

      auto sticky_reg = BrightnessStickyReg::Get().FromValue(0);
      sticky_reg.set_brightness(brightness_reg_value & kBrightnessRegMask);
      sticky_reg.set_is_valid(1);

      mock_regs_[BrightnessStickyReg::Get().addr()].ExpectWrite(sticky_reg.reg_value());
    }

    if (power != dev_->GetDevicePower()) {
      const uint8_t control_value = kDeviceControlDefaultValue | (power ? kBacklightOn : 0);
      mock_i2c_.ExpectWriteStop({kDeviceControlReg, control_value});
      if (power) {
        mock_i2c_.ExpectWriteStop({kCfg2Reg, dev_->GetCfg2()});
      }
    }
    EXPECT_OK(dev_->SetBacklightState(power, brightness));

    ASSERT_NO_FATAL_FAILURE(mock_regs_[BrightnessStickyReg::Get().addr()].VerifyAndClear());
    ASSERT_NO_FATAL_FAILURE(mock_i2c_.VerifyAndClear());
  }

 protected:
  const fidl::ClientEnd<fuchsia_hardware_adhoc_lp8556::Device>& client() const { return client_; }

  mock_i2c::MockI2cGtest mock_i2c_;
  std::unique_ptr<Lp8556Device> dev_;
  mock_mmio::Region mock_regs_;
  std::shared_ptr<MockDevice> fake_parent_;

 private:
  fidl::ClientEnd<fuchsia_hardware_adhoc_lp8556::Device> client_;
  async::Loop loop_;
  async::Loop i2c_loop_;
};

TEST_F(Lp8556DeviceTest, DdkLifecycle) { TestLifecycle(); }

TEST_F(Lp8556DeviceTest, Brightness) {
  VerifySetBrightness(false, 0.0);
  VerifyGetBrightness(false, 0.0);

  VerifySetBrightness(true, 0.5);
  VerifyGetBrightness(true, 0.5);

  VerifySetBrightness(true, 1.0);
  VerifyGetBrightness(true, 1.0);

  VerifySetBrightness(true, 0.0);
  VerifyGetBrightness(true, 0.0);
}

TEST_F(Lp8556DeviceTest, InitRegisters) {
  TiLp8556Metadata kDeviceMetadata = {
      .panel_id = 0,
      .registers =
          {
              // Registers
              0x01, 0x85,  // Device Control
                           // EPROM
              0xa2, 0x30,  // CFG2
              0xa3, 0x32,  // CFG3
              0xa5, 0x54,  // CFG5
              0xa7, 0xf4,  // CFG7
              0xa9, 0x60,  // CFG9
              0xae, 0x09,  // CFGE
          },
      .register_count = 14,
  };
  // constexpr uint8_t kInitialRegisterValues[] = {
  //     0x01, 0x85, 0xa2, 0x30, 0xa3, 0x32, 0xa5, 0x54, 0xa7, 0xf4, 0xa9, 0x60, 0xae, 0x09,
  // };

  fake_parent_->SetMetadata(DEVICE_METADATA_PRIVATE, &kDeviceMetadata, sizeof(kDeviceMetadata));

  mock_i2c_.ExpectWriteStop({0x01, 0x85})
      .ExpectWriteStop({0xa2, 0x30})
      .ExpectWriteStop({0xa3, 0x32})
      .ExpectWriteStop({0xa5, 0x54})
      .ExpectWriteStop({0xa7, 0xf4})
      .ExpectWriteStop({0xa9, 0x60})
      .ExpectWriteStop({0xae, 0x09})
      .ExpectWrite({kCfg2Reg})
      .ExpectReadStop({kCfg2Default})
      .ExpectWrite({kCurrentLsbReg})
      .ExpectReadStop({0x05, 0x4e})
      .ExpectWrite({kBacklightBrightnessLsbReg})
      .ExpectReadStop({0xab, 0x05})
      .ExpectWrite({kDeviceControlReg})
      .ExpectReadStop({0x85})
      .ExpectWrite({kCfgReg})
      .ExpectReadStop({0x01});
  mock_regs_[BrightnessStickyReg::Get().addr()].ExpectRead();

  EXPECT_OK(dev_->Init());

  ASSERT_NO_FATAL_FAILURE(mock_regs_[BrightnessStickyReg::Get().addr()].VerifyAndClear());
  ASSERT_NO_FATAL_FAILURE(mock_i2c_.VerifyAndClear());
}

TEST_F(Lp8556DeviceTest, InitNoRegisters) {
  mock_i2c_.ExpectWrite({kCfg2Reg})
      .ExpectReadStop({kCfg2Default})
      .ExpectWrite({kCurrentLsbReg})
      .ExpectReadStop({0x05, 0x4e})
      .ExpectWrite({kBacklightBrightnessLsbReg})
      .ExpectReadStop({0xab, 0x05})
      .ExpectWrite({kDeviceControlReg})
      .ExpectReadStop({0x85})
      .ExpectWrite({kCfgReg})
      .ExpectReadStop({0x01});
  mock_regs_[BrightnessStickyReg::Get().addr()].ExpectRead();

  EXPECT_OK(dev_->Init());

  ASSERT_NO_FATAL_FAILURE(mock_regs_[BrightnessStickyReg::Get().addr()].VerifyAndClear());
  ASSERT_NO_FATAL_FAILURE(mock_i2c_.VerifyAndClear());
}

TEST_F(Lp8556DeviceTest, InitInvalidRegisters) {
  constexpr uint8_t kInitialRegisterValues[] = {
      0x01, 0x85, 0xa2, 0x30, 0xa3, 0x32, 0xa5, 0x54, 0xa7, 0xf4, 0xa9, 0x60, 0xae,
  };

  fake_parent_->AddProtocol(ZX_PROTOCOL_PDEV, nullptr, nullptr, "pdev");
  fake_parent_->SetMetadata(DEVICE_METADATA_PRIVATE, kInitialRegisterValues,
                            sizeof(kInitialRegisterValues));

  EXPECT_NE(dev_->Init(), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(mock_regs_[BrightnessStickyReg::Get().addr()].VerifyAndClear());
  ASSERT_NO_FATAL_FAILURE(mock_i2c_.VerifyAndClear());
}

TEST_F(Lp8556DeviceTest, InitTooManyRegisters) {
  constexpr uint8_t kInitialRegisterValues[514] = {};

  fake_parent_->AddProtocol(ZX_PROTOCOL_PDEV, nullptr, nullptr, "pdev");
  fake_parent_->SetMetadata(DEVICE_METADATA_PRIVATE, kInitialRegisterValues,
                            sizeof(kInitialRegisterValues));

  EXPECT_NE(dev_->Init(), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(mock_regs_[BrightnessStickyReg::Get().addr()].VerifyAndClear());
  ASSERT_NO_FATAL_FAILURE(mock_i2c_.VerifyAndClear());
}

TEST_F(Lp8556DeviceTest, OverwriteStickyRegister) {
  // constexpr uint8_t kInitialRegisterValues[] = {
  //     kBacklightBrightnessLsbReg,
  //     0xab,
  //     kBacklightBrightnessMsbReg,
  //     0xcd,
  // };

  TiLp8556Metadata kDeviceMetadata = {
      .panel_id = 0,
      .registers =
          {// Registers
           kBacklightBrightnessLsbReg, 0xab, kBacklightBrightnessMsbReg, 0xcd},
      .register_count = 4,
  };

  fake_parent_->AddProtocol(ZX_PROTOCOL_PDEV, nullptr, nullptr, "pdev");
  fake_parent_->SetMetadata(DEVICE_METADATA_PRIVATE, &kDeviceMetadata, sizeof(kDeviceMetadata));

  mock_i2c_.ExpectWriteStop({kBacklightBrightnessLsbReg, 0xab})
      .ExpectWriteStop({kBacklightBrightnessMsbReg, 0xcd})
      .ExpectWrite({kCfg2Reg})
      .ExpectReadStop({kCfg2Default})
      .ExpectWrite({kCurrentLsbReg})
      .ExpectReadStop({0x05, 0x4e})
      .ExpectWrite({kBacklightBrightnessLsbReg})
      .ExpectReadStop({0xab, 0xcd})
      .ExpectWrite({kDeviceControlReg})
      .ExpectReadStop({0x85})
      .ExpectWrite({kCfgReg})
      .ExpectReadStop({0x01});
  mock_regs_[BrightnessStickyReg::Get().addr()].ExpectRead();

  EXPECT_OK(dev_->Init());

  const uint32_t kStickyRegValue =
      BrightnessStickyReg::Get().FromValue(0).set_is_valid(1).set_brightness(0x400).reg_value();
  mock_regs_[BrightnessStickyReg::Get().addr()].ExpectWrite(kStickyRegValue);

  // The DUT should set the brightness to 0.25 by writing 0x0400, starting with the LSB. The MSB
  // register needs to be RMW, so check that the upper four bits are preserved (0xab -> 0xa4).
  mock_i2c_.ExpectWriteStop({kBacklightBrightnessLsbReg, 0x00})
      .ExpectWrite({kBacklightBrightnessMsbReg})
      .ExpectReadStop({0xab})
      .ExpectWriteStop({kBacklightBrightnessMsbReg, 0xa4});

  auto result = fidl::WireCall(client())->SetStateNormalized({true, 0.25});
  EXPECT_TRUE(result.ok());
  EXPECT_FALSE(result->is_error());

  ASSERT_NO_FATAL_FAILURE(mock_regs_[BrightnessStickyReg::Get().addr()].VerifyAndClear());
  ASSERT_NO_FATAL_FAILURE(mock_i2c_.VerifyAndClear());
}

TEST_F(Lp8556DeviceTest, Inspect) {
  mock_i2c_.ExpectWrite({kCfg2Reg})
      .ExpectReadStop({kCfg2Default})
      .ExpectWrite({kCurrentLsbReg})
      .ExpectReadStop({0x05, 0x4e})
      .ExpectWrite({kBacklightBrightnessLsbReg})
      .ExpectReadStop({0xff, 0x0f})
      .ExpectWrite({kDeviceControlReg})
      .ExpectReadStop({0x85})
      .ExpectWrite({kCfgReg})
      .ExpectReadStop({0x01});
  mock_regs_[BrightnessStickyReg::Get().addr()].ExpectRead();

  EXPECT_OK(dev_->Init());

  fpromise::result<inspect::Hierarchy> hierarchy_result = inspect::ReadFromVmo(dev_->InspectVmo());
  ASSERT_TRUE(hierarchy_result.is_ok());

  inspect::Hierarchy hierarchy = std::move(hierarchy_result.value());
  const inspect::Hierarchy* root_node = hierarchy.GetByPath({"ti-lp8556"});
  ASSERT_TRUE(root_node);

  EXPECT_THAT(root_node->node(),
              inspect::testing::PropertyList(testing::AllOf(
                  testing::Contains(inspect::testing::DoubleIs("brightness", 1.0)),
                  testing::Contains(inspect::testing::UintIs("scale", 3589u)),
                  testing::Contains(inspect::testing::UintIs("calibrated_scale", 3589u)),
                  testing::Contains(inspect::testing::BoolIs("power", true)))));

  EXPECT_FALSE(root_node->node().get_property<inspect::UintPropertyValue>("persistent_brightness"));
  EXPECT_FALSE(
      root_node->node().get_property<inspect::DoublePropertyValue>("max_absolute_brightness_nits"));
}
struct IncomingNamespace {
  fdf_fake::FakePDev pdev_server;
  component::OutgoingDirectory outgoing{async_get_default_dispatcher()};
};

TEST_F(Lp8556DeviceTest, GetBackLightPower) {
  TiLp8556Metadata kDeviceMetadata = {
      .panel_id = 2,
      .registers = {},
      .register_count = 0,
  };

  constexpr uint32_t kBootloaderPanelId = 2;  // kBoeFiti9364
  constexpr display::PanelType kPanelType = display::PanelType::kBoeTv070wsmFitipowerJd9364Nelson;

  async::Loop incoming_loop{&kAsyncLoopConfigNoAttachToCurrentThread};
  async_patterns::TestDispatcherBound<IncomingNamespace> incoming{incoming_loop.dispatcher(),
                                                                  std::in_place};
  fdf_fake::FakePDev::Config config;
  config.board_info = fdf::PDev::BoardInfo{
      .pid = PDEV_PID_NELSON,
  };

  auto outgoing_endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
  ASSERT_OK(incoming_loop.StartThread("incoming-ns-thread"));
  incoming.SyncCall([config = std::move(config), server = std::move(outgoing_endpoints.server)](
                        IncomingNamespace* infra) mutable {
    infra->pdev_server.SetConfig(std::move(config));
    ASSERT_OK(infra->outgoing.AddService<fuchsia_hardware_platform_device::Service>(
        infra->pdev_server.GetInstanceHandler()));

    ASSERT_OK(infra->outgoing.Serve(std::move(server)));
  });
  ASSERT_NO_FATAL_FAILURE();
  fake_parent_->AddFidlService(fuchsia_hardware_platform_device::Service::Name,
                               std::move(outgoing_endpoints.client), "pdev");

  fake_parent_->SetMetadata(DEVICE_METADATA_PRIVATE, &kDeviceMetadata, sizeof(kDeviceMetadata));
  fake_parent_->SetMetadata(DEVICE_METADATA_BOARD_PRIVATE, &kBootloaderPanelId,
                            sizeof(kBootloaderPanelId));
  fake_parent_->SetMetadata(DEVICE_METADATA_DISPLAY_PANEL_TYPE, &kPanelType, sizeof(kPanelType));

  mock_i2c_.ExpectWrite({kCfg2Reg})
      .ExpectReadStop({kCfg2Default})
      .ExpectWrite({kCurrentLsbReg})
      .ExpectReadStop({0x42, 0x36})
      .ExpectWrite({kBacklightBrightnessLsbReg})
      .ExpectReadStop({0xab, 0x05})
      .ExpectWrite({kDeviceControlReg})
      .ExpectReadStop({0x85})
      .ExpectWrite({kCfgReg})
      .ExpectReadStop({0x36});
  mock_regs_[BrightnessStickyReg::Get().addr()].ExpectRead();

  EXPECT_OK(dev_->Init());

  VerifySetBrightness(false, 0.0);
  EXPECT_LT(abs(dev_->GetBacklightPower(0) - 0.0141694967), 0.000001f);

  VerifySetBrightness(true, 0.5);
  EXPECT_LT(abs(dev_->GetBacklightPower(2048) - 0.5352831254), 0.000001f);

  VerifySetBrightness(true, 1.0);
  EXPECT_LT(abs(dev_->GetBacklightPower(4095) - 1.0637770353), 0.000001f);
}

TEST_F(Lp8556DeviceTest, GetPowerWatts) {
  TiLp8556Metadata kDeviceMetadata = {
      .panel_id = 2,
      .registers = {},
      .register_count = 0,
  };

  constexpr uint32_t kBootloaderPanelId = 2;  // kBoeFiti9364
  constexpr display::PanelType kPanelType = display::PanelType::kBoeTv070wsmFitipowerJd9364Nelson;

  async::Loop incoming_loop{&kAsyncLoopConfigNoAttachToCurrentThread};
  async_patterns::TestDispatcherBound<IncomingNamespace> incoming{incoming_loop.dispatcher(),
                                                                  std::in_place};
  fdf_fake::FakePDev::Config config;
  config.board_info = fdf::PDev::BoardInfo{
      .pid = PDEV_PID_NELSON,
  };

  auto outgoing_endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
  ASSERT_OK(incoming_loop.StartThread("incoming-ns-thread"));
  incoming.SyncCall([config = std::move(config), server = std::move(outgoing_endpoints.server)](
                        IncomingNamespace* infra) mutable {
    infra->pdev_server.SetConfig(std::move(config));
    ASSERT_OK(infra->outgoing.AddService<fuchsia_hardware_platform_device::Service>(
        infra->pdev_server.GetInstanceHandler()));

    ASSERT_OK(infra->outgoing.Serve(std::move(server)));
  });
  ASSERT_NO_FATAL_FAILURE();
  fake_parent_->AddFidlService(fuchsia_hardware_platform_device::Service::Name,
                               std::move(outgoing_endpoints.client), "pdev");

  fake_parent_->SetMetadata(DEVICE_METADATA_PRIVATE, &kDeviceMetadata, sizeof(kDeviceMetadata));
  fake_parent_->SetMetadata(DEVICE_METADATA_BOARD_PRIVATE, &kBootloaderPanelId,
                            sizeof(kBootloaderPanelId));
  fake_parent_->SetMetadata(DEVICE_METADATA_DISPLAY_PANEL_TYPE, &kPanelType, sizeof(kPanelType));

  mock_i2c_.ExpectWrite({kCfg2Reg})
      .ExpectReadStop({kCfg2Default})
      .ExpectWrite({kCurrentLsbReg})
      .ExpectReadStop({0x42, 0x36})
      .ExpectWrite({kBacklightBrightnessLsbReg})
      .ExpectReadStop({0xab, 0x05})
      .ExpectWrite({kDeviceControlReg})
      .ExpectReadStop({0x85})
      .ExpectWrite({kCfgReg})
      .ExpectReadStop({0x36});
  mock_regs_[BrightnessStickyReg::Get().addr()].ExpectRead();

  EXPECT_OK(dev_->Init());

  VerifySetBrightness(true, 1.0);
  EXPECT_LT(abs(dev_->GetBacklightPower(4095) - 1.0637770353), 0.000001f);

  auto result = fidl::WireCall(client())->GetPowerWatts();
  EXPECT_TRUE(result.ok());
  EXPECT_FALSE(result->is_error());
}

}  // namespace ti

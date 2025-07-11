// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DRIVERS_MSD_ARM_MALI_TESTS_UNIT_TESTS_FAKE_CONNECTION_OWNER_BASE_H_
#define SRC_GRAPHICS_DRIVERS_MSD_ARM_MALI_TESTS_UNIT_TESTS_FAKE_CONNECTION_OWNER_BASE_H_

#include "src/graphics/drivers/msd-arm-mali/src/msd_arm_connection.h"

class FakeConnectionOwnerBase : public MsdArmConnection::Owner {
 public:
  ArmMaliCacheCoherencyStatus NdtGetCacheCoherencyStatus() override {
    return kArmMaliCacheCoherencyNone;
  }
  bool NdtIsProtectedModeSupported() override { return false; }
  void NdtDeregisterConnection() override {}
  void NdtSetCurrentThreadToDefaultPriority() override {}
  std::shared_ptr<DeviceRequest::Reply> NdtPostTask(FitCallbackTask task) override {
    // This implementation runs the callback immediately.
    auto real_task = std::make_unique<DeviceRequest>();
    auto reply = real_task->GetReply();
    reply->Signal(task(nullptr));
    return reply;
  }
  std::thread::id NdtGetDeviceThreadId() override { return std::this_thread::get_id(); }
  msd::MagmaMemoryPressureLevel NdtGetCurrentMemoryPressureLevel() override {
    // Only for testing.
    return msd::MAGMA_MEMORY_PRESSURE_LEVEL_NORMAL;
  }
};

#endif  // SRC_GRAPHICS_DRIVERS_MSD_ARM_MALI_TESTS_UNIT_TESTS_FAKE_CONNECTION_OWNER_BASE_H_

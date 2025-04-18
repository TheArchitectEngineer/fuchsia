// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.runtime.test/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export.h>

namespace fdf {
using namespace fuchsia_driver_framework;
}  // namespace fdf

namespace ft = fuchsia_runtime_test;

namespace {

class LeafDriver : public fdf::DriverBase {
 public:
  LeafDriver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : fdf::DriverBase("leaf", std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override {
    fdf::info("Start hook reached leaf");
    // Test we can block on the dispatcher thread.
    ZX_ASSERT(ZX_OK == DoHandshakeSynchronously());

    auto waiter = incoming()->Connect<ft::Waiter>();
    if (waiter.is_error()) {
      node().reset();
      fdf::info("failed to connect to waiter");
      return waiter.take_error();
    }

    const fidl::WireSharedClient<ft::Waiter> client(std::move(waiter.value()), dispatcher());
    auto result = client.sync()->Ack();
    if (!result.ok()) {
      node().reset();
      fdf::info("failed to ack waiter");
      return zx::error(result.error().status());
    }

    return zx::ok();
  }

 private:
  zx_status_t DoHandshakeSynchronously() {
    ZX_ASSERT((*driver_dispatcher()->options() & FDF_DISPATCHER_OPTION_ALLOW_SYNC_CALLS) ==
              FDF_DISPATCHER_OPTION_ALLOW_SYNC_CALLS);

    auto result = incoming()->Connect<ft::Handshake>();
    if (result.is_error()) {
      return result.status_value();
    }
    const fidl::WireSharedClient<ft::Handshake> client(std::move(*result), dispatcher());
    return client.sync()->Do().status();
  }
};

}  // namespace

FUCHSIA_DRIVER_EXPORT(LeafDriver);

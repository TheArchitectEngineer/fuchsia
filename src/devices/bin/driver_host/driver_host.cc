// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_host/driver_host.h"

#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fidl/fuchsia.system.state/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fdf/cpp/env.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/dlfcn.h>

// The driver runtime libraries use the fdf namespace, but we would also like to use fdf
// as an alias for the fdf FIDL library.
namespace fdf {
using namespace fuchsia_driver_framework;
}  // namespace fdf

namespace fdh = fuchsia_driver_host;

namespace driver_host {

DriverHost::DriverHost(inspect::Inspector& inspector, async::Loop& loop)
    : loop_(loop), crash_listener_(loop.dispatcher(), this) {
  inspector.GetRoot().CreateLazyNode("drivers", [this] { return Inspect(); }, &inspector);
}

fpromise::promise<inspect::Inspector> DriverHost::Inspect() {
  inspect::Inspector inspector;
  auto& root = inspector.GetRoot();
  size_t i = 0;

  std::lock_guard<std::mutex> lock(mutex_);
  for (auto& driver : drivers_) {
    auto child = root.CreateChild("driver-" + std::to_string(++i));
    child.CreateString("url", driver.url(), &inspector);
    inspector.emplace(std::move(child));
  }

  return fpromise::make_ok_promise(std::move(inspector));
}

zx::result<> DriverHost::PublishDriverHost(component::OutgoingDirectory& outgoing_directory) {
  zx::result init_result = crash_listener_.Init();
  if (init_result.is_error()) {
    FX_LOG_KV(ERROR, "Failed to initialize crash listener",
              FX_KV("status_str", init_result.status_string()));
    return init_result.take_error();
  }

  const auto service = [this](fidl::ServerEnd<fdh::DriverHost> request) {
    fidl::BindServer(loop_.dispatcher(), std::move(request), this);
  };
  auto status = outgoing_directory.AddUnmanagedProtocol<fdh::DriverHost>(std::move(service));
  if (status.is_error()) {
    FX_LOG_KV(ERROR, "Failed to add directory entry",
              FX_KV("name", fidl::DiscoverableProtocolName<fdh::DriverHost>),
              FX_KV("status_str", status.status_string()));
  }

  return status;
}

std::optional<const Driver*> DriverHost::ValidateAndGetDriver(const void* driver) {
  if (unlikely(driver == nullptr)) {
    return std::nullopt;
  }

  // Using try_lock since if there is an exception during the destroy hook, the mutex is
  // already taken by |ShutdownDriver|.
  if (mutex_.try_lock()) {
    for (auto& entry : drivers_) {
      if (&entry == driver) {
        mutex_.unlock();
        return &entry;
      }
    }

    mutex_.unlock();
  }

  return std::nullopt;
}

void DriverHost::Start(StartRequest& request, StartCompleter::Sync& completer) {
  auto callback = [this, request = std::move(request.driver()),
                   completer = completer.ToAsync()](zx::result<LoadedDriver> loaded) mutable {
    if (loaded.is_error()) {
      completer.Reply(loaded.take_error());
      return;
    }
    async_dispatcher_t* driver_async_dispatcher = loaded->dispatcher.async_dispatcher();

    // Task to start the driver. Post this to the driver dispatcher thread.
    auto start_task = [this, request = std::move(request), completer = std::move(completer),
                       loaded = std::move(*loaded)]() mutable {
      StartDriver(std::move(loaded.driver), std::move(loaded.start_args),
                  std::move(loaded.dispatcher), std::move(request),
                  [completer = std::move(completer)](zx::result<> status) mutable {
                    completer.Reply(status);
                  });
    };
    async::PostTask(driver_async_dispatcher, std::move(start_task));
  };
  LoadDriver(std::move(request.start_args()), loop_.dispatcher(), std::move(callback));
}

void DriverHost::StartLoadedDriver(StartLoadedDriverRequest& request,
                                   StartLoadedDriverCompleter::Sync& completer) {
  completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
}

void DriverHost::GetProcessInfo(GetProcessInfoCompleter::Sync& completer) {
  zx_info_handle_basic_t info;
  zx_status_t status =
      zx::process::self()->get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    FX_LOG_KV(ERROR, "Failed to get info about process handle",
              FX_KV("status_str", zx_status_get_string(status)));
    completer.Reply(zx::error(status));
    return;
  }
  uint64_t process_koid = info.koid;

  status =
      zx::job::default_job()->get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    FX_LOG_KV(ERROR, "Failed to get info about job handle",
              FX_KV("status_str", zx_status_get_string(status)));
    completer.Reply(zx::error(status));
    return;
  }
  uint64_t job_koid = info.koid;

  status =
      zx::thread::self()->get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    FX_LOG_KV(ERROR, "Failed to get info about main thread handle",
              FX_KV("status_str", zx_status_get_string(status)));
    completer.Reply(zx::error(status));
    return;
  }
  uint64_t main_thread_koid = info.koid;

  completer.Reply(zx::ok(fuchsia_driver_host::ProcessInfo{{
      .job_koid = job_koid,
      .process_koid = process_koid,
      .main_thread_koid = main_thread_koid,
      .threads = {},
      .dispatchers = {},
  }}));
}

void DriverHost::InstallLoader(InstallLoaderRequest& request,
                               InstallLoaderCompleter::Sync& completer) {
  zx::handle old_handle(dl_set_loader_service(request.loader().TakeChannel().release()));
}

void DriverHost::FindDriverCrashInfoByThreadKoid(
    FindDriverCrashInfoByThreadKoidRequest& request,
    FindDriverCrashInfoByThreadKoidCompleter::Sync& completer) {
  std::optional<fuchsia_driver_host::DriverCrashInfo> entry =
      crash_listener_.TakeByTid(request.thread_koid());
  if (!entry) {
    completer.Reply(zx::error(ZX_ERR_NOT_FOUND));
    return;
  }

  completer.Reply(zx::ok(std::move(entry.value())));
}

void DriverHost::StartDriver(fbl::RefPtr<Driver> driver,
                             fuchsia_driver_framework::DriverStartArgs start_args,
                             fdf::Dispatcher dispatcher,
                             fidl::ServerEnd<fuchsia_driver_host::Driver> request,
                             fit::callback<void(zx::result<>)> cb) {
  // We have to add the driver to this list before calling Start in order to have an accurate
  // count of how many drivers exist in this driver host.
  {
    std::lock_guard<std::mutex> lock(mutex_);
    drivers_.push_back(driver);
  }

  auto start_callback = [this, driver, cb = std::move(cb),
                         request = std::move(request)](zx::result<> status) mutable {
    if (status.is_error()) {
      FX_LOG_KV(ERROR, "Failed to start driver", FX_KV("url", driver->url().data()),
                FX_KV("status_str", status.status_string()));
      // If we fail to start the driver, we need to initiate shutting down the driver and
      // dispatchers.
      ShutdownDriver(driver.get(), {});
      cb(status);
      return;
    }

    FX_LOG_KV(INFO, "Started driver", FX_KV("url", driver->url().data()));
    auto unbind_callback = [this](Driver* driver, fidl::UnbindInfo info,
                                  fidl::ServerEnd<fdh::Driver> server) {
      if (!info.is_user_initiated()) {
        FX_LOG_KV(WARNING, "Unexpected stop of driver", FX_KV("url", driver->url().data()),
                  FX_KV("status_str", info.FormatDescription().data()));
      }
      ShutdownDriver(driver, std::move(server));
    };
    auto binding = fidl::BindServer(loop_.dispatcher(), std::move(request), driver.get(),
                                    std::move(unbind_callback));
    driver->set_binding(std::move(binding));
    cb(zx::ok());
  };
  driver->Start(driver, std::move(start_args), std::move(dispatcher), std::move(start_callback));
}

void DriverHost::ShutdownDriver(Driver* driver, fidl::ServerEnd<fdh::Driver> server) {
  // This will begin shutdown of the driver's client.
  driver->ShutdownClient();
  // Request the driver runtime shutdown all dispatchers owned by the driver.
  // Once we get the callback, we will stop the driver.
  auto driver_shutdown = std::make_unique<fdf_env::DriverShutdown>();
  auto driver_shutdown_ptr = driver_shutdown.get();
  auto shutdown_callback = [this, driver_shutdown = std::move(driver_shutdown), driver,
                            server = std::move(server)](const void* shutdown_driver) mutable {
    ZX_ASSERT(driver == shutdown_driver);

    std::lock_guard<std::mutex> lock(mutex_);
    // This removes the driver's unique_ptr from the list, which will
    // run the destructor and call the driver's Destroy hook.
    drivers_.erase(*driver);

    // The server will not be valid when the shutdown is happening because of a start failure.
    if (server.is_valid()) {
      // Send the epitaph to the driver runner letting it know we stopped
      // the driver correctly.
      server.Close(ZX_OK);
    }

    // If this is the last driver, shutdown the driver host.
    if (drivers_.is_empty()) {
      // We only exit if we're not shutting down in order to match DFv1 behavior.
      // TODO(https://fxbug.dev/42075187): We should always exit driver hosts when we get down to
      // 0 drivers.
      zx::result client = component::Connect<fuchsia_system_state::SystemStateTransition>();
      ZX_ASSERT_MSG(!client.is_error(), "Failed to connect to SystemStateTransition: %s",
                    client.status_string());
      fidl::WireResult result = fidl::WireCall(client.value())->GetTerminationSystemState();
      if (result.ok() == false ||
          result->state == fuchsia_system_state::SystemPowerState::kFullyOn) {
        loop_.Quit();
      }
    }
  };
  // We always expect this call to succeed, as we should be the only entity
  // that attempts to forcibly shutdown drivers.
  ZX_ASSERT(ZX_OK == driver_shutdown_ptr->Begin(driver, std::move(shutdown_callback)));
}

}  // namespace driver_host

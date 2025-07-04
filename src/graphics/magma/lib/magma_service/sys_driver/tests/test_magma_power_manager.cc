// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.power/cpp/fidl.h>
#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/test_base.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/power/cpp/testing/fake_element_control.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/internal/test_environment.h>
#include <lib/driver/testing/cpp/test_node.h>
#include <lib/magma_service/sys_driver/magma_power_manager.h>

#include <gtest/gtest.h>

namespace {

using fdf_power::testing::FakeElementControl;

class FakeSystemActivityGovernor
    : public fidl::testing::TestBase<fuchsia_power_system::ActivityGovernor> {
 public:
  FakeSystemActivityGovernor(zx::event exec_state_opportunistic, zx::event wake_handling_assertive)
      : exec_state_opportunistic_(std::move(exec_state_opportunistic)),
        wake_handling_assertive_(std::move(wake_handling_assertive)) {}

  fidl::ProtocolHandler<fuchsia_power_system::ActivityGovernor> CreateHandler() {
    return bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   fidl::kIgnoreBindingClosure);
  }

  void GetPowerElements(GetPowerElementsCompleter::Sync& completer) override {
    // The wake handling element isn't actually used by the mali driver, but is included for
    // completeness and consistency with the real implementation.
    fuchsia_power_system::PowerElements elements;
    zx::event execution_element;
    exec_state_opportunistic_.duplicate(ZX_RIGHT_SAME_RIGHTS, &execution_element);
    fuchsia_power_system::ExecutionState exec_state = {
        {.opportunistic_dependency_token = std::move(execution_element)}};

    elements = {{.execution_state = std::move(exec_state)}};

    completer.Reply({{std::move(elements)}});
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << name << " is not implemented";
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_system::ActivityGovernor> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  fidl::ServerBindingGroup<fuchsia_power_system::ActivityGovernor> bindings_;

  zx::event exec_state_opportunistic_;
  zx::event wake_handling_assertive_;
};

class FakeLeaseControl : public fidl::Server<fuchsia_power_broker::LeaseControl> {
 public:
  FakeLeaseControl() { fake_lease_control_couunt_++; }
  ~FakeLeaseControl() { fake_lease_control_couunt_--; }
  void WatchStatus(fuchsia_power_broker::LeaseControlWatchStatusRequest& req,
                   WatchStatusCompleter::Sync& completer) override {
    if (req.last_status() != lease_status_)
      completer.Reply(lease_status_);
    else {
      old_completers_.push_back(completer.ToAsync());
    }
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::LeaseControl> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  static fuchsia_power_broker::LeaseStatus lease_status_;

  static uint32_t fake_lease_control_couunt_;
  std::vector<WatchStatusCompleter::Async> old_completers_;
};

fuchsia_power_broker::LeaseStatus FakeLeaseControl::lease_status_ =
    fuchsia_power_broker::LeaseStatus::kPending;
uint32_t FakeLeaseControl::fake_lease_control_couunt_ = 0;

class FakeLessor : public fidl::Server<fuchsia_power_broker::Lessor> {
 public:
  void Lease(fuchsia_power_broker::LessorLeaseRequest& req,
             LeaseCompleter::Sync& completer) override {
    auto [lease_control_client_end, lease_control_server_end] =
        fidl::Endpoints<fuchsia_power_broker::LeaseControl>::Create();

    // Instantiate (fake) lease control implementation.
    auto lease_control_impl = std::make_unique<FakeLeaseControl>();
    lease_control_ = lease_control_impl.get();
    lease_control_binding_ = fidl::BindServer<fuchsia_power_broker::LeaseControl>(
        fdf::Dispatcher::GetCurrent()->async_dispatcher(), std::move(lease_control_server_end),
        std::move(lease_control_impl),
        [](FakeLeaseControl* impl, fidl::UnbindInfo info,
           fidl::ServerEnd<fuchsia_power_broker::LeaseControl> server_end) mutable {});

    completer.Reply(fit::success(std::move(lease_control_client_end)));
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::Lessor> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  FakeLeaseControl* lease_control_;

 private:
  std::optional<fidl::ServerBindingRef<fuchsia_power_broker::LeaseControl>> lease_control_binding_;
};

class PowerElement {
 public:
  explicit PowerElement(
      fidl::ServerBindingRef<fuchsia_power_broker::ElementControl> element_control,
      fidl::ServerBindingRef<fuchsia_power_broker::Lessor> lessor,
      std::optional<fidl::Client<fuchsia_power_broker::ElementRunner>> element_runner)
      : element_control_(std::move(element_control)),
        lessor_(std::move(lessor)),
        element_runner_(std::move(element_runner)) {}

  fidl::ServerBindingRef<fuchsia_power_broker::ElementControl> element_control_;
  fidl::ServerBindingRef<fuchsia_power_broker::Lessor> lessor_;
  std::optional<fidl::Client<fuchsia_power_broker::ElementRunner>> element_runner_;
};

constexpr char kHardwarePowerElementName[] = "my-hardware-element";
constexpr int kPoweredDownPowerLevel = 0;
constexpr int kPoweredUpPowerLevel = 1;

class FakePowerBroker : public fidl::Server<fuchsia_power_broker::Topology> {
 public:
  fidl::ProtocolHandler<fuchsia_power_broker::Topology> CreateHandler() {
    return bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   fidl::kIgnoreBindingClosure);
  }

  void AddElement(fuchsia_power_broker::ElementSchema& req,
                  AddElementCompleter::Sync& completer) override {
    // Get channels from request.
    fidl::ServerEnd<fuchsia_power_broker::Lessor>& lessor_server_end = req.lessor_channel().value();

    // Instantiate (fake) element control implementation.
    ASSERT_TRUE(req.element_control().has_value());
    auto element_control_impl = std::make_unique<FakeElementControl>();
    fidl::ServerBindingRef<fuchsia_power_broker::ElementControl> element_control_binding =
        fidl::BindServer<fuchsia_power_broker::ElementControl>(
            fdf::Dispatcher::GetCurrent()->async_dispatcher(), std::move(*req.element_control()),
            std::move(element_control_impl));

    // Instantiate (fake) lessor implementation.
    auto lessor_impl = std::make_unique<FakeLessor>();
    if (req.element_name() == kHardwarePowerElementName) {
      hardware_power_lessor_ = lessor_impl.get();
    } else {
      ADD_FAILURE() << "Unexpected power element " << req.element_name().value_or("{none}");
    }
    fidl::ServerBindingRef<fuchsia_power_broker::Lessor> lessor_binding =
        fidl::BindServer<fuchsia_power_broker::Lessor>(
            fdf::Dispatcher::GetCurrent()->async_dispatcher(), std::move(lessor_server_end),
            std::move(lessor_impl),
            [](FakeLessor* impl, fidl::UnbindInfo info,
               fidl::ServerEnd<fuchsia_power_broker::Lessor> server_end) mutable {});

    // Make (fake) call to ElementRunner::SetLevel
    std::optional<fidl::Client<fuchsia_power_broker::ElementRunner>> element_runner_client_;
    if (req.element_name() == kHardwarePowerElementName) {
      ASSERT_TRUE(req.element_runner().has_value());
      fidl::Client<fuchsia_power_broker::ElementRunner> element_runner_client(
          std::move(*req.element_runner()), fdf::Dispatcher::GetCurrent()->async_dispatcher());
      element_runner_client->SetLevel({1}).ThenExactlyOnce(
          [&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel>& result) mutable {
            if (!result.is_ok()) {
              ADD_FAILURE() << "SetLevel failed: " << result.error_value();
            }
          });
      element_runner_client_ = std::move(element_runner_client);
    }

    power_element_.emplace_back(std::move(element_control_binding), std::move(lessor_binding),
                                std::move(element_runner_client_));
    completer.Reply(fit::success());
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::Topology> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  FakeLessor* hardware_power_lessor_ = nullptr;

 private:
  fidl::ServerBindingGroup<fuchsia_power_broker::Topology> bindings_;

  std::vector<PowerElement> power_element_;
};

class FakePowerOwner : public msd::PowerElementRunner::Owner {
 public:
  void PostPowerStateChange(int64_t power_state, PowerStateCallback completer) override {
    enabled_calls_.push_back(power_state);
    completer(ZX_OK);
  }
  std::vector<int64_t>& enabled_calls() { return enabled_calls_; }

 private:
  std::vector<int64_t> enabled_calls_;
};

struct IncomingNamespace {
  IncomingNamespace() {
    zx::event::create(0, &exec_opportunistic);
    zx::event::create(0, &wake_assertive);
    zx::event exec_opportunistic_dupe, wake_assertive_dupe;
    EXPECT_EQ(ZX_OK, exec_opportunistic.duplicate(ZX_RIGHT_SAME_RIGHTS, &exec_opportunistic_dupe));
    EXPECT_EQ(ZX_OK, wake_assertive.duplicate(ZX_RIGHT_SAME_RIGHTS, &wake_assertive_dupe));
    system_activity_governor.emplace(std::move(exec_opportunistic_dupe),
                                     std::move(wake_assertive_dupe));
  }

  fdf_testing::TestNode node{"root"};
  fdf_testing::internal::TestEnvironment env{fdf::Dispatcher::GetCurrent()->get()};
  fdf_fake::FakePDev pdev_server;
  zx::event exec_opportunistic, wake_assertive;
  std::optional<FakeSystemActivityGovernor> system_activity_governor;
  FakePowerBroker power_broker;
};

fuchsia_hardware_power::PowerElementConfiguration hardware_power_config() {
  auto transitions_from_off =
      std::vector<fuchsia_hardware_power::Transition>{fuchsia_hardware_power::Transition{{
          .target_level = kPoweredUpPowerLevel,
          .latency_us = 500,
      }}};
  auto transitions_from_on =
      std::vector<fuchsia_hardware_power::Transition>{fuchsia_hardware_power::Transition{{
          .target_level = kPoweredDownPowerLevel,
          .latency_us = 2000,
      }}};
  fuchsia_hardware_power::PowerLevel off = {
      {.level = kPoweredDownPowerLevel, .name = "off", .transitions = transitions_from_off}};
  fuchsia_hardware_power::PowerLevel on = {
      {.level = kPoweredUpPowerLevel, .name = "on", .transitions = transitions_from_on}};
  fuchsia_hardware_power::PowerElement hardware_power = {{
      .name = kHardwarePowerElementName,
      .levels = {{off, on}},
  }};

  fuchsia_hardware_power::LevelTuple on_to_wake_handling = {{
      .child_level = kPoweredUpPowerLevel,
      .parent_level = static_cast<uint8_t>(fuchsia_power_system::ExecutionStateLevel::kSuspending),
  }};
  fuchsia_hardware_power::PowerDependency opportunistic_on_exec_state_wake_handling = {{
      .child = kHardwarePowerElementName,
      .parent = fuchsia_hardware_power::ParentElement::WithSag(
          fuchsia_hardware_power::SagElement::kExecutionState),
      .level_deps = {{on_to_wake_handling}},
      .strength = fuchsia_hardware_power::RequirementType::kOpportunistic,
  }};

  fuchsia_hardware_power::PowerElementConfiguration hardware_power_config = {
      {.element = hardware_power, .dependencies = {{opportunistic_on_exec_state_wake_handling}}}};
  return hardware_power_config;
}

// WARNING: Don't use this test as a template for new tests as it uses the old driver testing
// library.
TEST(PowerElementRunner, Basic) {
  fdf_testing::DriverRuntime runtime;
  runtime.StartBackgroundDispatcher();

  zx::result incoming_directory_endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
  fdf::UnownedSynchronizedDispatcher env_dispatcher{runtime.StartBackgroundDispatcher()};
  async_patterns::TestDispatcherBound<IncomingNamespace> incoming(
      env_dispatcher->async_dispatcher(), std::in_place);
  incoming.SyncCall([&](IncomingNamespace* incoming) mutable {
    EXPECT_TRUE(incoming->env.Initialize(std::move(incoming_directory_endpoints->server)).is_ok());
    fdf_fake::FakePDev::Config config;
    config.use_fake_irq = true;
    config.power_elements = std::vector{hardware_power_config()};
    incoming->pdev_server.SetConfig(std::move(config));
    {
      auto result =
          incoming->env.incoming_directory().AddService<fuchsia_hardware_platform_device::Service>(
              std::move(incoming->pdev_server.GetInstanceHandler(
                  fdf::Dispatcher::GetCurrent()->async_dispatcher())),
              "pdev");
      ASSERT_TRUE(result.is_ok());
    }
    // Serve (fake) system_activity_governor.
    {
      auto result = incoming->env.incoming_directory()
                        .component()
                        .AddUnmanagedProtocol<fuchsia_power_system::ActivityGovernor>(
                            incoming->system_activity_governor->CreateHandler());
      ASSERT_TRUE(result.is_ok());
    }

    // Serve (fake) power_broker.
    {
      auto result = incoming->env.incoming_directory()
                        .component()
                        .AddUnmanagedProtocol<fuchsia_power_broker::Topology>(
                            incoming->power_broker.CreateHandler());
      ASSERT_TRUE(result.is_ok());
    }
  });
  auto entry_incoming = fuchsia_component_runner::ComponentNamespaceEntry(
      {.path = std::string("/"), .directory = std::move(incoming_directory_endpoints->client)});
  std::vector<fuchsia_component_runner::ComponentNamespaceEntry> incoming_namespace;
  incoming_namespace.push_back(std::move(entry_incoming));

  auto fdf_incoming = fdf::Namespace::Create(incoming_namespace);
  ASSERT_TRUE(fdf_incoming.is_ok()) << fdf_incoming.status_string();

  FakePowerOwner owner;

  auto pdev = fdf_incoming->Connect<fuchsia_hardware_platform_device::Service::Device>("pdev");
  ASSERT_EQ(pdev.status_value(), ZX_OK);

  zx::result element = msd::PowerElementRunner::Create(pdev.value(), fdf_incoming.value(),
                                                       kHardwarePowerElementName, owner);
  ASSERT_EQ(element.status_value(), ZX_OK);

  element->EnablePower();

  runtime.RunUntil([&]() { return !owner.enabled_calls().empty(); });

  for (int64_t level : owner.enabled_calls()) {
    // Required power level is 1, so all calls should be to enable the GPU.
    EXPECT_EQ(level, kPoweredUpPowerLevel);
  }

  while (true) {
    bool have_lease_control = incoming.SyncCall([&](IncomingNamespace* incoming) mutable {
      return FakeLeaseControl::fake_lease_control_couunt_ > 0;
    });
    if (have_lease_control) {
      break;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(1));
  }

  element->DisablePower();
  while (true) {
    bool have_lease_control = incoming.SyncCall([&](IncomingNamespace* incoming) mutable {
      return FakeLeaseControl::fake_lease_control_couunt_ > 0;
    });
    if (!have_lease_control) {
      break;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(1));
  }
}

}  // namespace

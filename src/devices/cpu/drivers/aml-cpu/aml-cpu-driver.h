// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_CPU_DRIVERS_AML_CPU_AML_CPU_DRIVER_H_
#define SRC_DEVICES_CPU_DRIVERS_AML_CPU_AML_CPU_DRIVER_H_

#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/devfs/cpp/connector.h>
#include <lib/fit/function.h>

#include "src/devices/cpu/drivers/aml-cpu/aml-cpu.h"

namespace amlogic_cpu {

class AmlCpuPerformanceDomain : public AmlCpu {
 public:
  AmlCpuPerformanceDomain(async_dispatcher_t* dispatcher,
                          const std::vector<operating_point_t>& operating_points,
                          const perf_domain_t& perf_domain, inspect::ComponentInspector& inspect)
      : AmlCpu(operating_points, perf_domain, inspect),
        dispatcher_(dispatcher),
        devfs_connector_(fit::bind_member<&AmlCpuPerformanceDomain::CpuCtrlConnector>(this)) {}

  zx::result<> AddChild(fidl::WireSyncClient<fuchsia_driver_framework::Node>& node);

  void CpuCtrlConnector(fidl::ServerEnd<fuchsia_hardware_cpu_ctrl::Device> server);

 private:
  async_dispatcher_t* dispatcher_;
  fidl::ServerBindingGroup<fuchsia_hardware_cpu_ctrl::Device> bindings_;
  driver_devfs::Connector<fuchsia_hardware_cpu_ctrl::Device> devfs_connector_;
  fidl::WireSyncClient<fuchsia_driver_framework::Node> node_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> controller_;
};

class AmlCpuDriver : public fdf::DriverBase {
 public:
  AmlCpuDriver(fdf::DriverStartArgs start_args,
               fdf::UnownedSynchronizedDispatcher driver_dispatcher);

  zx::result<> Start() override;

  zx::result<std::unique_ptr<AmlCpuPerformanceDomain>> BuildPerformanceDomain(
      const perf_domain_t& perf_domain, const std::vector<operating_point>& pd_op_points,
      const AmlCpuConfiguration& config);
  std::vector<std::unique_ptr<AmlCpuPerformanceDomain>>& performance_domains() {
    return performance_domains_;
  }

 private:
  std::vector<std::unique_ptr<AmlCpuPerformanceDomain>> performance_domains_;
  fidl::WireSyncClient<fuchsia_driver_framework::Node> node_;
};

}  // namespace amlogic_cpu

#endif  // SRC_DEVICES_CPU_DRIVERS_AML_CPU_AML_CPU_DRIVER_H_

// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_KTRACE_PROVIDER_APP_H_
#define SRC_PERFORMANCE_KTRACE_PROVIDER_APP_H_

#include <lib/sys/cpp/component_context.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/trace-provider/provider.h>
#include <lib/trace/observer.h>

#include <fbl/unique_fd.h>

#include "src/lib/fxl/command_line.h"
#include "src/performance/ktrace_provider/device_reader.h"
#include "src/performance/ktrace_provider/log_importer.h"

#ifdef EXPERIMENTAL_KTRACE_STREAMING_ENABLED
constexpr bool kKernelStreamingSupport = EXPERIMENTAL_KTRACE_STREAMING_ENABLED;
#else
constexpr bool kKernelStreamingSupport = false;
#endif

namespace ktrace_provider {

std::vector<trace::KnownCategory> GetKnownCategories();

struct DrainContext {
  DrainContext(zx::time start, trace_prolonged_context_t* context, zx::resource tracing_resource,
               zx::duration poll_period)
      : start(start),
        reader(std::move(tracing_resource)),
        context(context),
        poll_period(poll_period) {}

  // We have a buffer allocated as part of the struct which we don't want to copy or move.
  DrainContext(const DrainContext& other) = delete;
  DrainContext& operator=(const DrainContext& other) = delete;
  DrainContext(DrainContext&& other) = delete;
  DrainContext& operator=(DrainContext&& other) = delete;

  static std::unique_ptr<DrainContext> Create(const zx::resource& tracing_resource,
                                              zx::duration poll_period) {
    trace_prolonged_context_t* context = nullptr;
    if constexpr (!kKernelStreamingSupport) {
      context = trace_acquire_prolonged_context();
      if (context == nullptr) {
        return nullptr;
      }
    }
    zx::resource cloned_resource;
    zx_status_t res = tracing_resource.duplicate(ZX_RIGHT_SAME_RIGHTS, &cloned_resource);
    if (res != ZX_OK) {
      return nullptr;
    }
    return std::make_unique<DrainContext>(zx::clock::get_monotonic(), context,
                                          std::move(cloned_resource), poll_period);
  }

  ~DrainContext() {
    if (context != nullptr) {
      trace_release_prolonged_context(context);
    }
  }
  zx::time start;
  DeviceReader reader;
  trace_prolonged_context_t* context;
  zx::duration poll_period;

  // For kernel streaming, we don't use the DeviceReader, we just do all the data management here.
  static constexpr size_t kChunkSize{size_t{1} * 1024 * 1024 / 8};
  uint64_t buffer_[kChunkSize];
};

class App {
 public:
  explicit App(zx::resource tracing_resource, const fxl::CommandLine& command_line);
  ~App();

 private:
  zx::result<> UpdateState();

  zx::result<> StartKTrace(uint32_t group_mask, trace_buffering_mode_t buffering_mode,
                           bool retain_current_data);
  zx::result<> StopKTrace();

  trace::TraceObserver trace_observer_;
  LogImporter log_importer_;
  uint32_t current_group_mask_ = 0u;
  // This context keeps the trace context alive until we've written our trace
  // records, which doesn't happen until after tracing has stopped.
  trace_prolonged_context_t* context_ = nullptr;
  zx::resource tracing_resource_;

  App(const App&) = delete;
  App(App&&) = delete;
  App& operator=(const App&) = delete;
  App& operator=(App&&) = delete;
};

}  // namespace ktrace_provider

#endif  // SRC_PERFORMANCE_KTRACE_PROVIDER_APP_H_

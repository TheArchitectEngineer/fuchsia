// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/performance/ktrace_provider/app.h"

#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fit/defer.h>
#include <lib/fxt/fields.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace-engine/instrumentation.h>
#include <lib/trace-provider/provider.h>
#include <lib/zx/channel.h>
#include <unistd.h>
#include <zircon/status.h>
#include <zircon/syscalls/log.h>

#include <iterator>

#include "src/performance/ktrace_provider/device_reader.h"

#ifdef EXPERIMENTAL_KTRACE_STREAMING_ENABLED
constexpr bool kKernelStreamingSupport = EXPERIMENTAL_KTRACE_STREAMING_ENABLED;
#else
constexpr bool kKernelStreamingSupport = false;
#endif

namespace ktrace_provider {
namespace {

struct KTraceCategory {
  const char* name;
  uint32_t group;
  const char* description;
};

constexpr KTraceCategory kGroupCategories[] = {
    {"kernel", KTRACE_GRP_ALL, "All ktrace categories"},
    {"kernel:meta", KTRACE_GRP_META, "Thread and process names"},
    {"kernel:memory", KTRACE_GRP_MEMORY,
     "Memory allocations performed by the kernel, such as heap growth."},
    {"kernel:sched", KTRACE_GRP_SCHEDULER, "Process and thread scheduling information"},
    {"kernel:tasks", KTRACE_GRP_TASKS, "<unused>"},
    {"kernel:ipc", KTRACE_GRP_IPC, "Emit an event for each FIDL call"},
    {"kernel:irq", KTRACE_GRP_IRQ, "Emit a duration event for interrupts"},
    {"kernel:probe", KTRACE_GRP_PROBE, "Used for LOCAL_KTRACE events"},
    {"kernel:arch", KTRACE_GRP_ARCH, "Hypervisor vcpus"},
    {"kernel:syscall", KTRACE_GRP_SYSCALL, "Emit an event for each syscall"},
    {"kernel:vm", KTRACE_GRP_VM, "Virtual memory events such as paging, mappings, and accesses"},
    {"kernel:restricted", KTRACE_GRP_RESTRICTED,
     "Duration events for when restricted mode is entered"},
};

// Meta category to retain current contents of ktrace buffer.
constexpr char kRetainCategory[] = "kernel:retain";

constexpr char kLogCategory[] = "log";

template <typename T>
void LogFidlFailure(const char* rqst_name, const fidl::Result<T>& result) {
  if (result.is_error()) {
    FX_LOGS(ERROR) << "Ktrace FIDL " << rqst_name
                   << " failed: " << result.error_value().status_string();
  } else if (result->status() != ZX_OK) {
    FX_PLOGS(ERROR, result->status()) << "Ktrace " << rqst_name << " failed";
  }
}

zx::result<> RequestKtraceStop(const zx::resource& tracing_resource) {
  return zx::make_result(zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_STOP, 0, nullptr));
}

zx::result<> RequestKtraceRewind(const zx::resource& tracing_resource) {
  return zx::make_result(
      zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_REWIND, 0, nullptr));
}

zx::result<> RequestKtraceStart(const zx::resource& tracing_resource,
                                trace_buffering_mode_t buffering_mode, uint32_t group_mask) {
  if constexpr (kKernelStreamingSupport) {
    if (zx_status_t status =
            zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_REWIND, 0, nullptr);
        status != ZX_OK) {
      return zx::error(status);
    }
    return zx::make_result(
        zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_START, group_mask, nullptr));
  }
  // Without kKernelStreamingSupport, ktrace does not support streaming, so for now we preserve
  // the legacy behavior of falling back on one-shot mode.
  switch (buffering_mode) {
    case trace_buffering_mode_t::TRACE_BUFFERING_MODE_STREAMING:
    case trace_buffering_mode_t::TRACE_BUFFERING_MODE_ONESHOT:
      return zx::make_result(
          zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_START, group_mask, nullptr));

    case trace_buffering_mode_t::TRACE_BUFFERING_MODE_CIRCULAR:
      return zx::make_result(zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_START_CIRCULAR,
                                               group_mask, nullptr));
    default:
      return zx::error(ZX_ERR_INVALID_ARGS);
  };
}

void ForwardBuffer(std::unique_ptr<DrainContext> drain_context) {
  if (!drain_context) {
    return;
  }
  trace_context_t* buffer_context = trace_acquire_context();

  if (buffer_context) {
    auto d = fit::defer([buffer_context]() { trace_release_context(buffer_context); });

    // If we have kernel streaming support, instead of reading at an offset, we simply always emit a
    // call to zx_ktrace_read to get the latest data.
    size_t actual;
    if (zx_status_t status =
            zx_ktrace_read(drain_context->reader.Resource().get(), drain_context->buffer_, 0,
                           sizeof(drain_context->buffer_), &actual);
        status != ZX_OK) {
      FX_PLOGS(ERROR, status) << "Failed to read from zx_ktrace open";
      return;
    }
    size_t percent = actual * 100 / sizeof(drain_context->buffer_);
    if (actual == sizeof(drain_context->buffer_)) {
      FX_LOGS(ERROR) << "[ 100% ] Read " << actual << " / " << sizeof(drain_context->buffer_)
                     << " bytes. May have dropped trace data!";
    } else if (percent > 75) {
      FX_LOGS(WARNING) << "[ " << percent << "% ] Read " << actual << " / "
                       << sizeof(drain_context->buffer_) << " bytes";
    }

    // Attempt to adapt our polling interval to the actual buffer rate. Nothing fancy, just reading
    // attempting to poll at a rate that keeps the buffer use at around 25% each time we read. That
    // way, if we'd need a 4x spike of trace data output over the polling interval to overflow the
    // buffer.
    //
    zx_duration_t new_poll =
        (drain_context->poll_period.get() * sizeof(drain_context->buffer_)) / (actual * size_t{4});

    // Clamp the value between 1ms and 100ms.
    //
    // Servicing the buffer takes on the order of 100-200us. Faster than 1ms and we begin hogging a
    // significant amount of CPU.
    //
    // Above 100ms, we're already using a smaller percent of cpu polling the buffer. We don't want
    // it to get too big else we could miss a burst of activity after a long idle period.
    drain_context->poll_period =
        zx::duration(std::clamp(new_poll, zx_duration_t{1'000'000}, zx_duration_t{100'000'000}));

    size_t offset = 0;
    const size_t num_words = actual / 8;
    while (offset < num_words) {
      uint64_t header = drain_context->buffer_[offset];
      size_t record_size_words = fxt::RecordFields::RecordSize::Get<size_t>(header);
      if (void* dst = trace_context_alloc_record(buffer_context, record_size_words * 8);
          dst != nullptr) {
        memcpy(dst, reinterpret_cast<const char*>(drain_context->buffer_ + offset),
               record_size_words * 8);
        offset += record_size_words;
      } else {
        // We could have failed for two reasons: we failed to allocate space in the buffer, in which
        // we should delay and try again, or the trace is done and we shouldn't try again.
        auto state = trace_state();
        if (state == TRACE_STOPPED || state == TRACE_STOPPING) {
          return;
        }
        break;
      }
    }
  }
  switch (trace_state()) {
    case TRACE_STOPPED:
    case TRACE_STOPPING:
      return;
    case TRACE_STARTED:
      break;
  }

  zx::duration poll_period = drain_context->poll_period;
  async::PostDelayedTask(
      async_get_default_dispatcher(),
      [drain_context = std::move(drain_context)]() mutable {
        ForwardBuffer(std::move(drain_context));
      },
      poll_period);
}

}  // namespace

std::vector<trace::KnownCategory> GetKnownCategories() {
  std::vector<trace::KnownCategory> known_categories = {
      {.name = kRetainCategory,
       .description = "Retain the previous contents of the buffer instead of clearing it out"},
  };

  for (const auto& category : kGroupCategories) {
    known_categories.emplace_back(category.name, category.description);
  }

  return known_categories;
}

App::App(zx::resource tracing_resource, const fxl::CommandLine& command_line)
    : tracing_resource_(std::move(tracing_resource)) {
  trace_observer_.Start(async_get_default_dispatcher(), [this] {
    if (zx::result res = UpdateState(); res.is_error()) {
      FX_PLOGS(ERROR, res.error_value()) << "Update state failed";
    }
  });
}

App::~App() = default;

zx::result<> App::UpdateState() {
  uint32_t group_mask = 0;
  bool capture_log = false;
  bool retain_current_data = false;
  if (trace_state() == TRACE_STARTED) {
    size_t num_enabled_categories = 0;
    for (const auto& category : kGroupCategories) {
      if (trace_is_category_enabled(category.name)) {
        group_mask |= category.group;
        ++num_enabled_categories;
      }
    }

    // Avoid capturing log traces in the default case by detecting whether all
    // categories are enabled or not.
    capture_log = trace_is_category_enabled(kLogCategory) &&
                  num_enabled_categories != std::size(kGroupCategories);

    // The default case is everything is enabled, but |kRetainCategory| must be
    // explicitly passed.
    retain_current_data = trace_is_category_enabled(kRetainCategory) &&
                          num_enabled_categories != std::size(kGroupCategories);
  }

  if (current_group_mask_ != group_mask) {
    trace_context_t* ctx = trace_acquire_context();

    if (zx::result res = StopKTrace(); res.is_error()) {
      return res.take_error();
    }
    if (zx::result res =
            StartKTrace(group_mask, trace_context_get_buffering_mode(ctx), retain_current_data);
        res.is_error()) {
      return res.take_error();
    }

    if (ctx != nullptr) {
      trace_release_context(ctx);
    }
  }

  if (capture_log) {
    log_importer_.Start();
  } else {
    log_importer_.Stop();
  }
  return zx::ok();
}

zx::result<> App::StartKTrace(uint32_t group_mask, trace_buffering_mode_t buffering_mode,
                              bool retain_current_data) {
  FX_DCHECK(!context_);
  if (!group_mask) {
    return zx::ok();  // nothing to trace
  }

  FX_LOGS(INFO) << "Starting ktrace";

  context_ = trace_acquire_prolonged_context();
  if (!context_) {
    // Tracing was disabled in the meantime.
    return zx::ok();
  }
  current_group_mask_ = group_mask;

  if (zx::result res = RequestKtraceStop(tracing_resource_); res.is_error()) {
    return res.take_error();
  }
  if (!retain_current_data) {
    if (zx::result res = RequestKtraceRewind(tracing_resource_); res.is_error()) {
      return res.take_error();
    }
  }
  if (zx::result res = RequestKtraceStart(tracing_resource_, buffering_mode, group_mask);
      res.is_error()) {
    return res.take_error();
  }

  if constexpr (kKernelStreamingSupport) {
    // In kernel streaming mode, we need to poll zx_ktrace_read for data while tracing.
    auto drain_context = DrainContext::Create(tracing_resource_, zx::msec(50));
    if (!drain_context) {
      FX_LOGS(ERROR) << "Failed to start reading kernel buffer";
      return zx::error(ZX_ERR_NO_RESOURCES);
    }
    zx_status_t result = async::PostTask(async_get_default_dispatcher(),
                                         [drain_context = std::move(drain_context)]() mutable {
                                           ForwardBuffer(std::move(drain_context));
                                         });
    if (result != ZX_OK) {
      FX_PLOGS(ERROR, result) << "Failed to schedule buffer writer";
      return zx::error(result);
    }
  }

  FX_LOGS(DEBUG) << "Ktrace started";
  return zx::ok();
}

void DrainBuffer(std::unique_ptr<DrainContext> drain_context) {
  if (!drain_context) {
    return;
  }

  trace_context_t* buffer_context = trace_acquire_context();
  auto d = fit::defer([buffer_context]() { trace_release_context(buffer_context); });
  for (std::optional<uint64_t> fxt_header = drain_context->reader.PeekNextHeader();
       fxt_header.has_value(); fxt_header = drain_context->reader.PeekNextHeader()) {
    size_t record_size_bytes = fxt::RecordFields::RecordSize::Get<size_t>(*fxt_header) * 8;
    // We try to be a bit too clever here and check that there is enough space before writing a
    // record to the buffer. If we're in streaming mode, and there isn't space for the record, this
    // will show up as a dropped record even though we retry later. Unfortunately, there isn't
    // currently a good api exposed.
    //
    // TODO(issues.fuchsia.dev/304532640): Investigate a method to allow trace providers to wait on
    // a full buffer
    if (void* dst = trace_context_alloc_record(buffer_context, record_size_bytes); dst != nullptr) {
      const uint64_t* record = drain_context->reader.ReadNextRecord();
      memcpy(dst, reinterpret_cast<const char*>(record), record_size_bytes);
    } else {
      if (trace_context_get_buffering_mode(buffer_context) == TRACE_BUFFERING_MODE_STREAMING) {
        // We are writing out our data on the async loop. Notifying the trace manager to begin
        // saving the data also requires the context and occurs on the loop. If we run out of space,
        // we'll release the loop and reschedule ourself to allow the buffer saving to begin.
        async::PostDelayedTask(
            async_get_default_dispatcher(),
            [drain_context = std::move(drain_context)]() mutable {
              DrainBuffer(std::move(drain_context));
            },
            drain_context->poll_period);
        return;
      }
      // Outside of streaming mode, we aren't going to get more space. We'll need to read in this
      // record and just drop it. Rather than immediately exiting, we allow the loop to continue so
      // that we correctly enumerate all the dropped records for statistical reporting.
      drain_context->reader.ReadNextRecord();
    }
  }

  // Done writing trace data
  size_t bytes_read = drain_context->reader.number_bytes_read();
  zx::duration time_taken = zx::clock::get_monotonic() - drain_context->start;
  double bytes_per_sec = static_cast<double>(bytes_read) /
                         static_cast<double>(std::max(int64_t{1}, time_taken.to_usecs()));
  FX_LOGS(INFO) << "Import of " << drain_context->reader.number_records_read() << " kernel records"
                << "(" << bytes_read << " bytes) took: " << time_taken.to_msecs()
                << "ms. MBytes/sec: " << bytes_per_sec;
  FX_LOGS(DEBUG) << "Ktrace stopped";
}

zx::result<> App::StopKTrace() {
  if (!context_) {
    return zx::ok();  // not currently tracing
  }
  auto d = fit::defer([this]() {
    trace_release_prolonged_context(context_);
    context_ = nullptr;
    current_group_mask_ = 0u;
  });
  FX_DCHECK(current_group_mask_);

  FX_LOGS(INFO) << "Stopping ktrace";

  if (zx::result res = RequestKtraceStop(tracing_resource_); res.is_error()) {
    return res;
  }

  // If we're streaming, we don't need to schedule a flush, we've been flushing the whole time.
  if constexpr (kKernelStreamingSupport) {
    return zx::ok();
  }

  // Once the trace ends, we are memcpy'ing data here and trace_manager is writing the buffer
  // to a socket (likely shared with ffx), the cost to copy the kernel buffer to the trace buffer
  // here pales in comparison to the cost of what trace_manager is doing. We'll poll here with a
  // slight delay, even though all the data is ready, to allow trace_manager to keep up.
  auto drain_context = DrainContext::Create(tracing_resource_, zx::msec(100));
  if (!drain_context) {
    FX_LOGS(ERROR) << "Failed to start reading kernel buffer";
    return zx::error(ZX_ERR_NO_RESOURCES);
  }
  zx_status_t result = async::PostTask(async_get_default_dispatcher(),
                                       [drain_context = std::move(drain_context)]() mutable {
                                         DrainBuffer(std::move(drain_context));
                                       });
  if (result != ZX_OK) {
    FX_PLOGS(ERROR, result) << "Failed to schedule buffer writer";
    return zx::error(result);
  }
  return zx::ok();
}

}  // namespace ktrace_provider

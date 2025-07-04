// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/trace/event.h>
#include <zircon/availability.h>
#include <zircon/syscalls.h>

namespace {

struct EventHelper {
  EventHelper(trace_context_t* context, const char* name_literal) : ticks(zx_ticks_get_boot()) {
    trace_context_register_current_thread(context, &thread_ref);
    trace_context_register_string_literal(context, name_literal, &name_ref);
  }

  trace_ticks_t const ticks;
  trace_thread_ref_t thread_ref;
  trace_string_ref_t name_ref;
};

}  // namespace

// Argument names are temporarily stored in |name_ref.inline_string|.
// Convert them to string references.
void trace_internal_complete_args(trace_context_t* context, trace_arg_t* args, size_t num_args) {
  for (size_t i = 0; i < num_args; ++i) {
    trace_context_register_string_literal(context, args[i].name_ref.inline_string,
                                          &args[i].name_ref);
  }
}

void trace_internal_write_instant_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_scope_t scope, trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_instant_event_record(context, helper.ticks, &helper.thread_ref, category_ref,
                                           &helper.name_ref, scope, args, num_args);
  trace_release_context(context);
}

void trace_internal_write_counter_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_counter_id_t counter_id, trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_counter_event_record(context, helper.ticks, &helper.thread_ref, category_ref,
                                           &helper.name_ref, counter_id, args, num_args);
  trace_release_context(context);
}

void trace_internal_write_duration_begin_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_duration_begin_event_record(context, helper.ticks, &helper.thread_ref,
                                                  category_ref, &helper.name_ref, args, num_args);
  trace_release_context(context);
}

void trace_internal_write_duration_end_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_duration_end_event_record(context, helper.ticks, &helper.thread_ref,
                                                category_ref, &helper.name_ref, args, num_args);
  trace_release_context(context);
}

void trace_internal_write_duration_event_record(const trace_internal_duration_scope_t* scope) {
  trace_string_ref_t category_ref;
  trace_context_t* context =
      trace_acquire_context_for_category(scope->category_literal, &category_ref);
  if (likely(context)) {
    EventHelper helper(context, scope->name_literal);
    trace_internal_complete_args(context, scope->args, scope->num_args);
    trace_context_write_duration_event_record(context, scope->start_time, helper.ticks,
                                              &helper.thread_ref, &category_ref, &helper.name_ref,
                                              scope->args, scope->num_args);
    trace_release_context(context);
  }
}

void trace_internal_write_async_begin_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_async_id_t async_id, trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_async_begin_event_record(context, helper.ticks, &helper.thread_ref,
                                               category_ref, &helper.name_ref, async_id, args,
                                               num_args);
  trace_release_context(context);
}

void trace_internal_write_async_instant_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_async_id_t async_id, trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_async_instant_event_record(context, helper.ticks, &helper.thread_ref,
                                                 category_ref, &helper.name_ref, async_id, args,
                                                 num_args);
  trace_release_context(context);
}

void trace_internal_write_async_end_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_async_id_t async_id, trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_async_end_event_record(context, helper.ticks, &helper.thread_ref,
                                             category_ref, &helper.name_ref, async_id, args,
                                             num_args);
  trace_release_context(context);
}

void trace_internal_write_flow_begin_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_flow_id_t flow_id, trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_flow_begin_event_record(context, helper.ticks, &helper.thread_ref,
                                              category_ref, &helper.name_ref, flow_id, args,
                                              num_args);
  trace_release_context(context);
}

void trace_internal_write_instaflow_begin_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    const char* name_slash_step_literal, trace_flow_id_t flow_id, trace_arg_t* args,
    size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_string_ref_t name_slash_step_ref;
  trace_context_register_string_literal(context, name_slash_step_literal, &name_slash_step_ref);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_duration_begin_event_record(context, helper.ticks, &helper.thread_ref,
                                                  category_ref, &name_slash_step_ref, args,
                                                  num_args);
  trace_context_write_flow_begin_event_record(context, helper.ticks, &helper.thread_ref,
                                              category_ref, &helper.name_ref, flow_id, nullptr, 0);
  trace_context_write_duration_end_event_record(context, helper.ticks, &helper.thread_ref,
                                                category_ref, &name_slash_step_ref, nullptr, 0);
  trace_release_context(context);
}

void trace_internal_write_flow_step_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_flow_id_t flow_id, trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_flow_step_event_record(context, helper.ticks, &helper.thread_ref,
                                             category_ref, &helper.name_ref, flow_id, args,
                                             num_args);
  trace_release_context(context);
}

void trace_internal_write_instaflow_step_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    const char* name_slash_step_literal, trace_flow_id_t flow_id, trace_arg_t* args,
    size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_string_ref_t name_slash_step_ref;
  trace_context_register_string_literal(context, name_slash_step_literal, &name_slash_step_ref);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_duration_begin_event_record(context, helper.ticks, &helper.thread_ref,
                                                  category_ref, &name_slash_step_ref, args,
                                                  num_args);
  trace_context_write_flow_step_event_record(context, helper.ticks, &helper.thread_ref,
                                             category_ref, &helper.name_ref, flow_id, nullptr, 0);
  trace_context_write_duration_end_event_record(context, helper.ticks, &helper.thread_ref,
                                                category_ref, &name_slash_step_ref, nullptr, 0);
  trace_release_context(context);
}

void trace_internal_write_flow_end_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    trace_flow_id_t flow_id, trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_flow_end_event_record(context, helper.ticks, &helper.thread_ref, category_ref,
                                            &helper.name_ref, flow_id, args, num_args);
  trace_release_context(context);
}

void trace_internal_write_instaflow_end_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    const char* name_slash_step_literal, trace_flow_id_t flow_id, trace_arg_t* args,
    size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_string_ref_t name_slash_step_ref;
  trace_context_register_string_literal(context, name_slash_step_literal, &name_slash_step_ref);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_duration_begin_event_record(context, helper.ticks, &helper.thread_ref,
                                                  category_ref, &name_slash_step_ref, args,
                                                  num_args);
  trace_context_write_flow_end_event_record(context, helper.ticks, &helper.thread_ref, category_ref,
                                            &helper.name_ref, flow_id, nullptr, 0);
  trace_context_write_duration_end_event_record(context, helper.ticks, &helper.thread_ref,
                                                category_ref, &name_slash_step_ref, nullptr, 0);
  trace_release_context(context);
}

void trace_internal_write_blob_event_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    const void* blob, size_t blob_size, trace_arg_t* args, size_t num_args) {
  EventHelper helper(context, name_literal);
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_blob_event_record(context, helper.ticks, &helper.thread_ref, category_ref,
                                        &helper.name_ref, blob, blob_size, args, num_args);
  trace_release_context(context);
}

void trace_internal_write_blob_attachment_record_and_release_context(
    trace_context_t* context, const trace_string_ref_t* category_ref, const char* name_literal,
    const void* blob, size_t blob_size) {
  trace_string_ref_t name_ref;
  trace_context_register_string_literal(context, name_literal, &name_ref);
  trace_context_write_blob_attachment_record(context, category_ref, &name_ref, blob, blob_size);
  trace_release_context(context);
}

void trace_internal_write_kernel_object_record_for_handle_and_release_context(
    trace_context_t* context, zx_handle_t handle, trace_arg_t* args, size_t num_args) {
  trace_internal_complete_args(context, args, num_args);
  trace_context_write_kernel_object_record_for_handle(context, handle, args, num_args);
  trace_release_context(context);
}

void trace_internal_write_blob_record_and_release_context(trace_context_t* context,
                                                          trace_blob_type_t type,
                                                          const char* name_literal,
                                                          const void* blob, size_t blob_size) {
  trace_string_ref_t name_ref;
  trace_context_register_string_literal(context, name_literal, &name_ref);
  trace_context_write_blob_record(context, type, &name_ref, blob, blob_size);
  trace_release_context(context);
}

void trace_internal_send_alert_and_release_context(trace_context_t* context,
                                                   const char* alert_name) {
  trace_context_send_alert(context, alert_name);
  trace_release_context(context);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT)
uint64_t trace_internal_time_based_id(void) {
  trace_context_t* ctx = trace_acquire_context();
  if (unlikely(ctx)) {
    trace_thread_ref_t thread_ref;
    trace_context_register_current_thread(ctx, &thread_ref);

    return trace_time_based_id(thread_ref.inline_thread_koid);
  }

  return 0;
}
#endif

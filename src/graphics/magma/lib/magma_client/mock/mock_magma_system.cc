// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/magma/magma.h>
#include <lib/magma/magma_logging.h>
#include <lib/magma/magma_sysmem.h>
#include <lib/magma/platform/platform_buffer.h>
#include <lib/magma/platform/platform_semaphore.h>
#include <lib/magma/util/dlog.h>
#include <lib/magma/util/short_macros.h>

#include <unordered_map>

std::unordered_map<uint32_t, magma::PlatformBuffer*> exported_buffers;
std::unordered_map<uint32_t, magma::PlatformSemaphore*> exported_semaphores;

class MockConnection {
 public:
  uint32_t next_context_id() { return next_context_id_++; }

 private:
  uint32_t next_context_id_ = 1;
};

class MockDevice {};

magma_status_t magma_device_import(uint32_t device_handle, magma_device_t* device_out) {
  *device_out = reinterpret_cast<magma_device_t>(new MockDevice);
  return MAGMA_STATUS_OK;
}

void magma_device_release(magma_device_t device) { delete reinterpret_cast<MockDevice*>(device); }

magma_status_t magma_device_create_connection(magma_device_t device,
                                              magma_connection_t* connection_out) {
  *connection_out = reinterpret_cast<magma_connection_t>(new MockConnection());
  return MAGMA_STATUS_OK;
}

void magma_connection_release(magma_connection_t connection) {
  delete reinterpret_cast<MockConnection*>(connection);
}

magma_status_t magma_connection_get_error(magma_connection_t connection) { return MAGMA_STATUS_OK; }

magma_status_t magma_connection_flush(magma_connection_t connection) { return MAGMA_STATUS_OK; }

magma_status_t magma_device_query(magma_device_t device, uint64_t id, uint32_t* result_buffer_out,
                                  uint64_t* value_out) {
  switch (id) {
    case MAGMA_QUERY_DEVICE_ID:
      *value_out = 0x1916;
      return MAGMA_STATUS_OK;
    case MAGMA_QUERY_VENDOR_PARAM_0:
      *value_out = (23l << 32) | 8;
      return MAGMA_STATUS_OK;
    case MAGMA_QUERY_VENDOR_PARAM_0 + 1:  // gtt size
      *value_out = 1ull << 32;
      return MAGMA_STATUS_OK;
    case MAGMA_QUERY_VENDOR_PARAM_0 + 2:  // extra page count
      *value_out = 0;
      return MAGMA_STATUS_OK;
  }
  return MAGMA_STATUS_INVALID_ARGS;
}

magma_status_t magma_connection_create_context(magma_connection_t connection,
                                               uint32_t* context_id_out) {
  *context_id_out = reinterpret_cast<MockConnection*>(connection)->next_context_id();
  return MAGMA_STATUS_OK;
}

void magma_connection_release_context(magma_connection_t connection, uint32_t context_id) {}

magma_status_t magma_connection_create_buffer(magma_connection_t connection, uint64_t size,
                                              uint64_t* size_out, magma_buffer_t* buffer_out,
                                              magma_buffer_id_t* id_out) {
  auto buffer = magma::PlatformBuffer::Create(size, "magma-alloc");
  *id_out = buffer->id();
  *buffer_out = reinterpret_cast<magma_buffer_t>(buffer.release());
  *size_out = size;
  return MAGMA_STATUS_OK;
}

void magma_connection_release_buffer(magma_connection_t connection, magma_buffer_t buffer) {
  delete reinterpret_cast<magma::PlatformBuffer*>(buffer);
}

magma_status_t magma_buffer_set_cache_policy(magma_buffer_t buffer, magma_cache_policy_t policy) {
  return MAGMA_STATUS_OK;
}

magma_status_t magma_connection_execute_command(magma_connection_t connection, uint32_t context_id,
                                                struct magma_command_descriptor* descriptor) {
  DLOG("magma_execute_command - STUB");
  return MAGMA_STATUS_OK;
}

magma_status_t magma_connection_execute_inline_commands(
    magma_connection_t connection, uint32_t context_id, uint64_t command_count,
    struct magma_inline_command_buffer* command_buffers) {
  DLOG("magma_connection_execute_inline_commands - STUB");
  return MAGMA_STATUS_OK;
}

magma_status_t magma_connection_import_buffer(magma_connection_t connection, uint32_t buffer_handle,
                                              uint64_t* size_out, magma_buffer_t* buffer_out,
                                              magma_buffer_id_t* id_out) {
  auto buffer = reinterpret_cast<magma::PlatformBuffer*>(exported_buffers[buffer_handle]);
  *id_out = buffer->id();
  *size_out = buffer->size();
  *buffer_out = reinterpret_cast<magma_buffer_t>(buffer);
  exported_buffers.erase(buffer_handle);
  return MAGMA_STATUS_OK;
}

void magma_connection_release_semaphore(magma_connection_t connection,
                                        magma_semaphore_t semaphore) {
  delete reinterpret_cast<magma::PlatformSemaphore*>(semaphore);
}

void magma_semaphore_signal(magma_semaphore_t semaphore) {}

void magma_semaphore_reset(magma_semaphore_t semaphore) {}

magma_status_t magma_connection_map_buffer(magma_connection_t connection, uint64_t hw_va,
                                           magma_buffer_t buffer, uint64_t offset, uint64_t length,
                                           uint64_t map_flags) {
  return MAGMA_STATUS_OK;
}

void magma_connection_unmap_buffer(magma_connection_t connection, uint64_t hw_va,
                                   magma_buffer_t buffer) {}

uint32_t magma_connection_get_notification_channel_handle(magma_connection_t connection) {
  return 0;
}

magma_status_t magma_connection_read_notification_channel(magma_connection_t connection,
                                                          void* buffer, uint64_t buffer_size,
                                                          uint64_t* buffer_size_out,
                                                          magma_bool_t* more_data_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_sysmem_connection_import_buffer_collection(
    magma_sysmem_connection_t connection, magma_handle_t handle,
    magma_buffer_collection_t* collection_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_sysmem_connection_import(magma_handle_t channel,
                                              magma_sysmem_connection_t* connection_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

void magma_fuchsia_log(int8_t severity, const char* tag, const char* file, int line,
                       const char* format, va_list va) {
  magma::PlatformLogger::LogLevel level;
  switch (severity) {
    case 0x30:
      level = magma::PlatformLogger::LOG_INFO;
      break;
    case 0x40:
      level = magma::PlatformLogger::LOG_WARNING;
      break;
    case 0x50:
      level = magma::PlatformLogger::LOG_ERROR;
      break;
    default:
      level = magma::PlatformLogger::LOG_INFO;
  }
  magma::PlatformLogger::LogVa(level, file, line, format, va);
}

void magma_sysmem_connection_release(magma_sysmem_connection_t connection) {}

void magma_buffer_collection_release2(magma_buffer_collection_t collection) {}

magma_status_t magma_sysmem_connection_create_buffer_constraints(
    magma_sysmem_connection_t connection,
    const magma_buffer_format_constraints_t* buffer_constraints,
    magma_sysmem_buffer_constraints_t* constraints_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_buffer_constraints_set_format2(
    magma_sysmem_buffer_constraints_t constraints, uint32_t index,
    const magma_image_format_constraints_t* format_constraints) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_buffer_constraints_set_colorspaces2(
    magma_sysmem_buffer_constraints_t constraints, uint32_t index, uint32_t color_space_count,
    const uint32_t* color_spaces) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

void magma_buffer_constraints_release2(magma_sysmem_buffer_constraints_t constraints) {}

magma_status_t magma_buffer_collection_set_constraints2(
    magma_buffer_collection_t collection, magma_sysmem_buffer_constraints_t constraints) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_collection_info_get_coherency_domain(magma_collection_info_t description,
                                                          uint32_t* coherency_domain_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_collection_info_get_plane_info_with_size(
    magma_collection_info_t collection_info, uint32_t width, uint32_t height,
    magma_image_plane_t* image_planes_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

void magma_collection_info_release(magma_collection_info_t collection_info) {}

magma_status_t magma_collection_info_get_format(magma_collection_info_t collection_info,
                                                uint32_t* format_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_collection_info_get_format_modifier(magma_collection_info_t collection_info,
                                                         magma_bool_t* has_format_modifier_out,
                                                         uint64_t* format_modifier_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_collection_info_get_color_space(magma_collection_info_t collection_info,
                                                     uint32_t* color_space_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_buffer_collection_get_collection_info(
    magma_buffer_collection_t collection, magma_collection_info_t* collection_info_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_buffer_collection_get_buffer_handle(magma_buffer_collection_t collection,
                                                         uint32_t index,
                                                         magma_handle_t* buffer_handle_out,
                                                         uint32_t* vmo_offset_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_collection_info_get_format_index(magma_collection_info_t collection_info,
                                                      magma_sysmem_buffer_constraints_t constraints,
                                                      magma_bool_t* format_valid_out,
                                                      uint32_t format_valid_count) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_collection_info_get_buffer_count(magma_collection_info_t description,
                                                      uint32_t* count_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_buffer_get_cache_policy(magma_buffer_t buffer,
                                             magma_cache_policy_t* cache_policy_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_buffer_clean_cache(magma_buffer_t buffer, uint64_t offset, uint64_t size,
                                        magma_cache_operation_t operation) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_initialize_tracing(magma_handle_t channel) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_initialize_logging(magma_handle_t channel) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_poll(magma_poll_item_t* items, uint32_t count, uint64_t timeout_ns) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_enable_performance_counter_access(magma_connection_t connection,
                                                                  magma_handle_t channel) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_enable_performance_counters(magma_connection_t connection,
                                                            uint64_t* counters,
                                                            uint64_t counters_count) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_create_performance_counter_buffer_pool(
    magma_connection_t connection, magma_perf_count_pool_t* pool_out,
    magma_handle_t* notification_handle_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_release_performance_counter_buffer_pool(
    magma_connection_t connection, magma_perf_count_pool_t pool) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_add_performance_counter_buffer_offsets_to_pool(
    magma_connection_t connection, magma_perf_count_pool_t pool, const magma_buffer_offset* offsets,
    uint64_t offset_count) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_remove_performance_counter_buffer_from_pool(
    magma_connection_t connection, magma_perf_count_pool_t pool, magma_buffer_t buffer) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_dump_performance_counters(magma_connection_t connection,
                                                          magma_perf_count_pool_t pool,
                                                          uint32_t trigger_id) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_clear_performance_counters(magma_connection_t connection,
                                                           uint64_t* counters,
                                                           uint64_t counters_count) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_read_performance_counter_completion(
    magma_connection_t connection, magma_perf_count_pool_t pool, uint32_t* trigger_id_out,
    uint64_t* buffer_id_out, uint32_t* buffer_offset_out, uint64_t* time_out,
    uint32_t* result_flags_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_connection_perform_buffer_op(magma_connection_t connection,
                                                  magma_buffer_t buffer, uint32_t options,
                                                  uint64_t start_offset, uint64_t length) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_buffer_get_info(magma_buffer_t buffer, magma_buffer_info_t* info_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

magma_status_t magma_buffer_get_handle(magma_buffer_t buffer, magma_handle_t* handle_out) {
  if (!reinterpret_cast<magma::PlatformBuffer*>(buffer)->duplicate_handle(handle_out))
    return DRET(MAGMA_STATUS_INVALID_ARGS);
  return MAGMA_STATUS_OK;
}

magma_status_t magma_virt_connection_get_image_info(magma_connection_t connection,
                                                    magma_buffer_t image,
                                                    magma_image_info_t* image_info_out) {
  return MAGMA_STATUS_UNIMPLEMENTED;
}

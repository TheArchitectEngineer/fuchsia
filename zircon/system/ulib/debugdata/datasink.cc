// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <inttypes.h>
#include <lib/debugdata/datasink.h>
#include <lib/fzl/vmo-mapper.h>
#include <sys/stat.h>
#include <zircon/status.h>

#include <cstddef>
#include <cstdio>
#include <forward_list>
#include <optional>
#include <string_view>
#include <system_error>
#include <unordered_map>
#include <vector>

#include <fbl/string.h>
#include <fbl/unique_fd.h>

#include "src/lib/fxl/strings/string_printf.h"

#include <profile/InstrProfData.inc>

namespace debugdata {

namespace {

constexpr char kProfileSink[] = "llvm-profile";

using IntPtrT = intptr_t;

enum ValueKind {
#define VALUE_PROF_KIND(Enumerator, Value, Descr) Enumerator = (Value),
#include <profile/InstrProfData.inc>
};

struct __llvm_profile_data {
#define INSTR_PROF_DATA(Type, LLVMType, Name, Initializer) Type Name;
#include <profile/InstrProfData.inc>
};

struct __llvm_profile_header {
#define INSTR_PROF_RAW_HEADER(Type, Name, Initializer) Type Name;
#include <profile/InstrProfData.inc>
};

// llvm_profile_header_v9 and llvm_profile_data_format_v9 define the layout of the
// profiles that have profile version 9.
// TODO(b/42086151): Remove these after Rust toolchain switches to the
// profile version 10 and above.
struct llvm_profile_header_v9 {
  uint64_t Magic;
  uint64_t Version;
  uint64_t BinaryIdsSize;
  uint64_t NumData;
  uint64_t PaddingBytesBeforeCounters;
  uint64_t NumCounters;
  uint64_t PaddingBytesAfterCounters;
  uint64_t NumBitmapBytes;
  uint64_t PaddingBytesAfterBitmapBytes;
  uint64_t NamesSize;
  uint64_t CountersDelta;
  uint64_t BitmapDelta;
  uint64_t NamesDelta;
  uint64_t ValueKindLast;
};

struct llvm_profile_data_format_v9 {
  uint64_t NameRef;
  uint64_t FuncHash;
  IntPtrT CounterPtr;
  IntPtrT BitmapPtr;
  IntPtrT FunctionPointer;
  IntPtrT Values;
  uint32_t NumCounters;
  uint16_t NumValueSites;
  uint32_t NumBitmapBytes;
};

std::error_code ReadFile(const fbl::unique_fd& fd, uint8_t* data, size_t size) {
  auto* buf = data;
  ssize_t count = size;
  off_t off = 0;
  while (count > 0) {
    ssize_t len = pread(fd.get(), buf, count, off);
    if (len <= 0) {
      return std::error_code{errno, std::generic_category()};
    }
    buf += len;
    count -= len;
    off += len;
  }
  return std::error_code{};
}

std::error_code WriteFile(const fbl::unique_fd& fd, const uint8_t* data, size_t size) {
  auto* buf = data;
  ssize_t count = size;
  off_t off = 0;
  while (count > 0) {
    ssize_t len = pwrite(fd.get(), buf, count, off);
    if (len <= 0) {
      return std::error_code{errno, std::generic_category()};
    }
    buf += len;
    count -= len;
    off += len;
  }
  return std::error_code{};
}

std::optional<std::string> GetVMOName(const zx::vmo& vmo) {
  char name[ZX_MAX_NAME_LEN];
  zx_status_t status = vmo.get_property(ZX_PROP_NAME, name, sizeof(name));
  if (status != ZX_OK || name[0] == '\0') {
    zx_info_handle_basic_t info;
    status = vmo.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
    if (status != ZX_OK) {
      return {};
    }
    snprintf(name, sizeof(name), "unnamed.%" PRIu64, info.koid);
  }
  return name;
}

zx_status_t GetVMOSize(const zx::vmo& vmo, uint64_t* size) {
  auto status = vmo.get_prop_content_size(size);
  if (status != ZX_OK || *size == 0) {
    status = vmo.get_size(size);
  }
  return status;
}

fbl::String JoinPath(std::string_view parent, std::string_view child) {
  if (parent.empty()) {
    return fbl::String(child);
  }
  if (child.empty()) {
    return fbl::String(parent);
  }
  if (parent[parent.size() - 1] != '/' && child[0] != '/') {
    return fbl::String::Concat({parent, "/", child});
  }
  if (parent[parent.size() - 1] == '/' && child[0] == '/') {
    return fbl::String::Concat({parent, &child[1]});
  }
  return fbl::String::Concat({parent, child});
}

// TODO(https://fxbug.dev/333945525): Remove this function after Rust toolchain switches to the
// raw profile version 10 and above.
bool ProfilesCompatibleVersion9(const uint8_t* dst, const uint8_t* src) {
  const llvm_profile_header_v9* src_header = reinterpret_cast<const llvm_profile_header_v9*>(src);
  const llvm_profile_header_v9* dst_header = reinterpret_cast<const llvm_profile_header_v9*>(dst);

  if (src_header->NumData != dst_header->NumData ||
      src_header->NumCounters != dst_header->NumCounters ||
      src_header->NamesSize != dst_header->NamesSize)
    return false;

  const llvm_profile_data_format_v9* src_data_start =
      reinterpret_cast<const llvm_profile_data_format_v9*>(src + sizeof(*src_header));
  src_data_start = reinterpret_cast<const llvm_profile_data_format_v9*>(
      reinterpret_cast<const uint8_t*>(src_data_start) + src_header->BinaryIdsSize);
  const llvm_profile_data_format_v9* src_data_end = src_data_start + src_header->NumData;
  const llvm_profile_data_format_v9* dst_data_start =
      reinterpret_cast<const llvm_profile_data_format_v9*>(dst + sizeof(*dst_header));
  dst_data_start = reinterpret_cast<const llvm_profile_data_format_v9*>(
      reinterpret_cast<const uint8_t*>(dst_data_start) + dst_header->BinaryIdsSize);
  const llvm_profile_data_format_v9* dst_data_end = dst_data_start + dst_header->NumData;

  for (const llvm_profile_data_format_v9 *src_data = src_data_start, *dst_data = dst_data_start;
       src_data < src_data_end && dst_data < dst_data_end; ++src_data, ++dst_data) {
    if (src_data->NameRef != dst_data->NameRef || src_data->FuncHash != dst_data->FuncHash ||
        src_data->NumCounters != dst_data->NumCounters)
      return false;
  }

  return true;
}

// Returns true if raw profiles |src| and |dst| are structurally compatible.
bool ProfilesCompatible(const uint8_t* dst, const uint8_t* src) {
  const __llvm_profile_header* src_header = reinterpret_cast<const __llvm_profile_header*>(src);
  const __llvm_profile_header* dst_header = reinterpret_cast<const __llvm_profile_header*>(dst);

  const uint64_t src_header_version = src_header->Version & ~VARIANT_MASK_BYTE_COVERAGE;
  const uint64_t dst_header_version = dst_header->Version & ~VARIANT_MASK_BYTE_COVERAGE;

  if (src_header->Magic != dst_header->Magic || src_header_version != dst_header_version)
    return false;

  // Check that raw profiles use version 9 and above because older versions are not supported.
  ZX_ASSERT(src_header_version >= 9 && dst_header_version >= 9);

  if (src_header_version == 9 && dst_header_version == 9)
    return ProfilesCompatibleVersion9(dst, src);

  if (src_header->NumData != dst_header->NumData ||
      src_header->NumCounters != dst_header->NumCounters ||
      src_header->NamesSize != dst_header->NamesSize)
    return false;

  const __llvm_profile_data* src_data_start =
      reinterpret_cast<const __llvm_profile_data*>(src + sizeof(*src_header));
  src_data_start = reinterpret_cast<const __llvm_profile_data*>(
      reinterpret_cast<const uint8_t*>(src_data_start) + src_header->BinaryIdsSize);
  const __llvm_profile_data* src_data_end = src_data_start + src_header->NumData;
  const __llvm_profile_data* dst_data_start =
      reinterpret_cast<const __llvm_profile_data*>(dst + sizeof(*dst_header));
  dst_data_start = reinterpret_cast<const __llvm_profile_data*>(
      reinterpret_cast<const uint8_t*>(dst_data_start) + dst_header->BinaryIdsSize);
  const __llvm_profile_data* dst_data_end = dst_data_start + dst_header->NumData;

  for (const __llvm_profile_data *src_data = src_data_start, *dst_data = dst_data_start;
       src_data < src_data_end && dst_data < dst_data_end; ++src_data, ++dst_data) {
    if (src_data->NameRef != dst_data->NameRef || src_data->FuncHash != dst_data->FuncHash ||
        src_data->NumCounters != dst_data->NumCounters)
      return false;
  }

  return true;
}

// TODO(https://fxbug.dev/333945525): Remove this function after Rust toolchain switches to the
// raw profile version 10 and above.
template <typename T, template <typename> class Op>
uint8_t* MergeCountersVersion9(uint8_t* dst, const uint8_t* src) {
  const llvm_profile_header_v9* src_header = reinterpret_cast<const llvm_profile_header_v9*>(src);
  const llvm_profile_data_format_v9* src_data_start =
      reinterpret_cast<const llvm_profile_data_format_v9*>(src + sizeof(*src_header));
  src_data_start = reinterpret_cast<const llvm_profile_data_format_v9*>(
      reinterpret_cast<const uint8_t*>(src_data_start) + src_header->BinaryIdsSize);
  const llvm_profile_data_format_v9* src_data_end = src_data_start + src_header->NumData;
  const T* src_counters = reinterpret_cast<const T*>(src_data_end);

  llvm_profile_header_v9* dst_header = reinterpret_cast<llvm_profile_header_v9*>(dst);
  llvm_profile_data_format_v9* dst_data_start =
      reinterpret_cast<llvm_profile_data_format_v9*>(dst + sizeof(*dst_header));
  dst_data_start = reinterpret_cast<llvm_profile_data_format_v9*>(
      reinterpret_cast<uint8_t*>(dst_data_start) + dst_header->BinaryIdsSize);
  llvm_profile_data_format_v9* dst_data_end = dst_data_start + dst_header->NumData;
  T* dst_counters = reinterpret_cast<T*>(dst_data_end);

  constexpr Op<T> op;
  uint64_t NumCounters = src_header->NumCounters;
  for (unsigned i = 0; i < NumCounters; i++) {
    dst_counters[i] = op(dst_counters[i], src_counters[i]);
  }

  return dst;
}

// Merges counters |src| and |dst| into |dst|.
template <typename T, template <typename> class Op>
uint8_t* MergeCounters(uint8_t* dst, const uint8_t* src) {
  const __llvm_profile_header* src_header = reinterpret_cast<const __llvm_profile_header*>(src);
  const __llvm_profile_data* src_data_start =
      reinterpret_cast<const __llvm_profile_data*>(src + sizeof(*src_header));
  src_data_start = reinterpret_cast<const __llvm_profile_data*>(
      reinterpret_cast<const uint8_t*>(src_data_start) + src_header->BinaryIdsSize);
  const __llvm_profile_data* src_data_end = src_data_start + src_header->NumData;
  const T* src_counters = reinterpret_cast<const T*>(src_data_end);

  __llvm_profile_header* dst_header = reinterpret_cast<__llvm_profile_header*>(dst);
  __llvm_profile_data* dst_data_start =
      reinterpret_cast<__llvm_profile_data*>(dst + sizeof(*dst_header));
  dst_data_start = reinterpret_cast<__llvm_profile_data*>(
      reinterpret_cast<uint8_t*>(dst_data_start) + dst_header->BinaryIdsSize);
  __llvm_profile_data* dst_data_end = dst_data_start + dst_header->NumData;
  T* dst_counters = reinterpret_cast<T*>(dst_data_end);

  constexpr Op<T> op;
  uint64_t NumCounters = src_header->NumCounters;
  for (unsigned i = 0; i < NumCounters; i++) {
    dst_counters[i] = op(dst_counters[i], src_counters[i]);
  }

  return dst;
}

// Merges raw profiles |src| and |dst| into |dst|.
//
// Note that this function does not check whether the profiles are compatible.
uint8_t* MergeProfiles(uint8_t* dst, const uint8_t* src) {
  const __llvm_profile_header* src_header = reinterpret_cast<const __llvm_profile_header*>(src);
  const bool single_byte_counters = src_header->Version & VARIANT_MASK_BYTE_COVERAGE;

  if ((src_header->Version & ~VARIANT_MASK_BYTE_COVERAGE) == 9) {
    if (single_byte_counters)
      return MergeCountersVersion9<uint8_t, std::logical_and>(dst, src);
    else
      return MergeCountersVersion9<uint64_t, std::plus>(dst, src);
  }

  if (single_byte_counters)
    return MergeCounters<uint8_t, std::logical_and>(dst, src);
  else
    return MergeCounters<uint64_t, std::plus>(dst, src);
}

// Process all data sink dumps and write to the disk.
std::optional<DumpFile> ProcessDataSinkDump(const std::string& sink_name, const zx::vmo& file_data,
                                            const fbl::unique_fd& data_sink_dir_fd,
                                            DataSinkCallback& error_callback,
                                            DataSinkCallback& warning_callback) {
  zx_status_t status;

  if (mkdirat(data_sink_dir_fd.get(), sink_name.c_str(), 0777) != 0 && errno != EEXIST) {
    error_callback(fxl::StringPrintf("FAILURE: cannot mkdir \"%s\" for data-sink: %s\n",
                                     sink_name.c_str(), strerror(errno)));
    return {};
  }
  fbl::unique_fd sink_dir_fd{
      openat(data_sink_dir_fd.get(), sink_name.c_str(), O_RDONLY | O_DIRECTORY)};
  if (!sink_dir_fd) {
    error_callback(fxl::StringPrintf("FAILURE: cannot open data-sink directory \"%s\": %s\n",
                                     sink_name.c_str(), strerror(errno)));
    return {};
  }

  auto name = GetVMOName(file_data);
  if (!name) {
    error_callback(fxl::StringPrintf("FAILURE: Cannot get a name for the VMO\n"));
    return {};
  }

  uint64_t size;
  status = GetVMOSize(file_data, &size);
  if (status != ZX_OK) {
    error_callback(
        fxl::StringPrintf("FAILURE: Cannot get size of VMO \"%s\" for data-sink \"%s\": %s\n",
                          name->c_str(), sink_name.c_str(), zx_status_get_string(status)));
    return {};
  }

  fzl::VmoMapper mapper;
  if (size > 0) {
    zx_status_t status = mapper.Map(file_data, 0, size, ZX_VM_PERM_READ);
    if (status != ZX_OK) {
      error_callback(fxl::StringPrintf("FAILURE: Cannot map VMO \"%s\" for data-sink \"%s\": %s\n",
                                       name->c_str(), sink_name.c_str(),
                                       zx_status_get_string(status)));
      return {};
    }
  } else {
    warning_callback(fxl::StringPrintf("WARNING: Empty VMO \"%s\" published for data-sink \"%s\"\n",
                                       name->c_str(), sink_name.c_str()));
    return {};
  }

  zx_info_handle_basic_t info;
  status = file_data.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    error_callback(fxl::StringPrintf("FAILURE: Cannot get a basic info for VMO \"%s\": %s\n",
                                     name->c_str(), zx_status_get_string(status)));
    return {};
  }

  char filename[ZX_MAX_NAME_LEN];
  snprintf(filename, sizeof(filename), "%s.%" PRIu64, sink_name.c_str(), info.koid);
  fbl::unique_fd fd{openat(sink_dir_fd.get(), filename, O_WRONLY | O_CREAT | O_EXCL, 0666)};
  if (!fd) {
    error_callback(fxl::StringPrintf("FAILURE: Cannot open data-sink file \"%s\": %s\n", filename,
                                     strerror(errno)));
    return {};
  }
  if (std::error_code ec = WriteFile(fd, reinterpret_cast<uint8_t*>(mapper.start()), size); ec) {
    error_callback(fxl::StringPrintf("FAILURE: Cannot write data to \"%s\": %s\n", filename,
                                     strerror(ec.value())));
    return {};
  }

  return DumpFile{*name, JoinPath(sink_name, filename).c_str()};
}

}  // namespace

void DataSink::ProcessSingleDebugData(const std::string& data_sink, zx::vmo debug_data,
                                      std::optional<std::string> tag,
                                      DataSinkCallback& error_callback,
                                      DataSinkCallback& warning_callback) {
  if (data_sink == kProfileSink) {
    ProcessProfile(debug_data, std::move(tag), error_callback, warning_callback);
  } else {
    auto dump_file = ProcessDataSinkDump(data_sink, debug_data, data_sink_dir_fd_, error_callback,
                                         warning_callback);
    if (dump_file) {
      auto& tag_vec = dump_files_[data_sink][*dump_file];
      if (tag) {
        tag_vec.push_back(std::move(*tag));
      }
    }
  }
}

DataSinkFileMap DataSink::FlushToDirectory(DataSinkCallback& error_callback,
                                           DataSinkCallback& warning_callback) {
  if (mkdirat(data_sink_dir_fd_.get(), kProfileSink, 0777) != 0 && errno != EEXIST) {
    error_callback(fxl::StringPrintf("FAILURE: cannot mkdir \"%s\" for data-sink: %s\n",
                                     kProfileSink, strerror(errno)));
    return {};
  }
  fbl::unique_fd sink_dir_fd{openat(data_sink_dir_fd_.get(), kProfileSink, O_RDONLY | O_DIRECTORY)};
  if (!sink_dir_fd) {
    error_callback(fxl::StringPrintf("FAILURE: cannot open data-sink directory \"%s\": %s\n",
                                     kProfileSink, strerror(errno)));
    return {};
  }

  for (auto& [name, profile] : merged_profiles_) {
    fbl::unique_fd fd{openat(sink_dir_fd.get(), name.c_str(), O_RDWR | O_CREAT, 0666)};
    if (!fd) {
      error_callback(fxl::StringPrintf("FAILURE: Cannot open data-sink file \"%s\": %s\n",
                                       name.c_str(), strerror(errno)));
      return {};
    }
    struct stat stat;
    if (fstat(fd.get(), &stat) == -1) {
      error_callback(fxl::StringPrintf("FAILURE: Cannot stat data-sink file \"%s\": %s\n",
                                       name.c_str(), strerror(errno)));
      return {};
    }
    if (auto file_size = static_cast<uint64_t>(stat.st_size); file_size > 0) {
      // The file already exists. Merge with buffer and write back.
      std::unique_ptr<uint8_t[]> file_buffer = std::make_unique<uint8_t[]>(file_size);
      if (std::error_code ec = ReadFile(fd, file_buffer.get(), file_size); ec) {
        error_callback(fxl::StringPrintf("FAILURE: Cannot read data from \"%s\": %s\n",
                                         name.c_str(), strerror(ec.value())));
        return {};
      }
      if (profile.size != file_size) {
        error_callback(
            fxl::StringPrintf("FAILURE: Mismatch between content sizes for \"%s\": %lu != %lu\n",
                              name.c_str(), profile.size, file_size));
      }
      ZX_ASSERT(profile.size == file_size);

      // Ensure that profiles are structuraly compatible.
      if (!ProfilesCompatible(profile.buffer.get(), file_buffer.get())) {
        error_callback(fxl::StringPrintf("WARNING: Unable to merge profile data: %s\n",
                                         "source profile file is not compatible"));
        return {};
      }
      MergeProfiles(profile.buffer.get(), file_buffer.get());
    }

    if (std::error_code ec = WriteFile(fd, profile.buffer.get(), profile.size); ec) {
      error_callback(fxl::StringPrintf("FAILURE: Cannot write data to \"%s\": %s\n", name.c_str(),
                                       strerror(ec.value())));
      return {};
    }
    dump_files_[kProfileSink].emplace(DumpFile{name, JoinPath(kProfileSink, name).c_str()},
                                      profile.tags);
  }
  DataSinkFileMap result;
  std::swap(result, dump_files_);
  return result;
}

// This function processes all raw profiles that were published via data sink
// in an efficient manner. It merges all profiles from the same binary into a single
// profile. First it groups all VMOs by name which uniquely identifies each
// binary. Then it merges together all VMOs for the same binary. This ensures that at the
// end, we have exactly one profile for each binary in total.
void DataSink::ProcessProfile(const zx::vmo& vmo, std::optional<std::string> tag,
                              DataSinkCallback& error_callback,
                              DataSinkCallback& warning_callback) {
  // Group data by profile name. The name is a hash computed from profile metadata and
  // should be unique across all binaries (modulo hash collisions).
  auto optional_name = GetVMOName(vmo);
  if (!optional_name) {
    error_callback(fxl::StringPrintf("FAILURE: Cannot get a name for the VMO\n"));
    return;
  }
  std::string name = *optional_name;

  zx_status_t status;
  uint64_t vmo_size;
  status = GetVMOSize(vmo, &vmo_size);
  if (status != ZX_OK) {
    error_callback(
        fxl::StringPrintf("FAILURE: Cannot get size of VMO \"%s\" for data-sink \"%s\": %s\n",
                          name.c_str(), kProfileSink, zx_status_get_string(status)));
    return;
  }

  fzl::VmoMapper mapper;
  if (vmo_size > 0) {
    zx_status_t status = mapper.Map(vmo, 0, vmo_size, ZX_VM_PERM_READ);
    if (status != ZX_OK) {
      error_callback(fxl::StringPrintf("FAILURE: Cannot map VMO \"%s\" for data-sink \"%s\": %s\n",
                                       name.c_str(), kProfileSink, zx_status_get_string(status)));
      return;
    }
  } else {
    warning_callback(fxl::StringPrintf("WARNING: Empty VMO \"%s\" published for data-sink \"%s\"\n",
                                       kProfileSink, name.c_str()));
    return;
  }

  auto existing_merged = merged_profiles_.find(name);
  if (existing_merged == merged_profiles_.end()) {
    // new profile name, create a new buffer
    auto it_pair = merged_profiles_.emplace(std::move(name), vmo_size);
    auto& merged_profile = it_pair.first->second;

    memcpy(merged_profile.buffer.get(), mapper.start(), vmo_size);
    if (tag) {
      merged_profile.tags.push_back(std::move(*tag));
    }
  } else {
    auto& merged_profile = existing_merged->second;
    // profile name exists, merge with existing
    if (merged_profile.size != vmo_size) {
      error_callback(
          fxl::StringPrintf("FAILURE: Mismatch between content sizes for \"%s\": %lu != %lu\n",
                            name.c_str(), merged_profile.size, vmo_size));
    }
    ZX_ASSERT(merged_profile.size == vmo_size);

    // Ensure that profiles are structuraly compatible.
    if (!ProfilesCompatible(merged_profile.buffer.get(),
                            reinterpret_cast<const uint8_t*>(mapper.start()))) {
      error_callback(fxl::StringPrintf("WARNING: Unable to merge profile data: %s\n",
                                       "source profile file is not compatible"));
      return;
    }

    if (tag) {
      merged_profile.tags.push_back(std::move(*tag));
    }

    MergeProfiles(merged_profile.buffer.get(), reinterpret_cast<const uint8_t*>(mapper.start()));
  }
}

}  // namespace debugdata

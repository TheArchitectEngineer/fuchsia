// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_DIAGNOSTICS_LOG_MESSAGE_RUST_CPP_LOG_DECODER_LOG_DECODER_H_
#define SRC_LIB_DIAGNOSTICS_LOG_MESSAGE_RUST_CPP_LOG_DECODER_LOG_DECODER_H_

// Warning:
// This file was autogenerated by cbindgen.
// Do not modify this file manually.

#include <zircon/types.h>

#include <cstdarg>
#include <cstdint>
#include <cstdlib>
#include <new>
#include <ostream>

struct CPPLogMessageBuilder;

/// Memory-managed state to be free'd on the Rust side
/// when the log messages are destroyed.
struct ManagedState;

/// Array for FFI purposes between C++ and Rust.
/// If len is 0, ptr is allowed to be nullptr,
/// otherwise, ptr must be valid.
template <typename T>
struct CPPArray {
  /// Number of elements in the array
  uintptr_t len;
  /// Pointer to the first element in the array,
  /// may be null in the case of a 0 length array,
  /// but is not guaranteed to always be null of
  /// len is 0.
  const T *ptr;
};

/// Log message representation for FFI with C++
struct LogMessage {
  /// Severity of a log message.
  uint8_t severity;
  /// Tags in a log message, guaranteed to be non-null.
  CPPArray<CPPArray<uint8_t>> tags;
  /// Process ID from a LogMessage, or 0 if unknown
  uint64_t pid;
  /// Thread ID from a LogMessage, or 0 if unknown
  uint64_t tid;
  /// Number of dropped log messages.
  uint64_t dropped;
  /// The UTF-encoded log message, guaranteed to be valid UTF-8.
  CPPArray<uint8_t> message;
  /// Timestamp on the boot timeline of the log message,
  /// in nanoseconds.
  int64_t timestamp;
  /// Pointer to the builder is owned by this CPPLogMessage.
  /// Dropping this CPPLogMessage will free the builder.
  CPPLogMessageBuilder *builder;
};

/// LogMessages struct containing log messages
/// It is created by calling fuchsia_decode_log_messages_to_struct,
/// and freed by calling fuchsia_free_log_messages.
/// Log messages contain embedded pointers to the bytes from
/// which they were created, so the memory referred to
/// by the LogMessages must not be modified or free'd until
/// the LogMessages are free'd.
struct LogMessages {
  CPPArray<LogMessage *> messages;
  ManagedState *state;
};

extern "C" {

/// # Safety
///
/// Same as for `std::slice::from_raw_parts`. Summarizing in terms of this API:
///
/// - `msg` must be valid for reads for `size`, and it must be properly aligned.
/// - `msg` must point to `size` consecutive u8 values.
/// - The `size` of the slice must be no larger than `isize::MAX`, and adding
///   that size to data must not "wrap around" the address space. See the safety
///   documentation of pointer::offset.
extern "C" char *fuchsia_decode_log_message_to_json(const uint8_t *msg, uintptr_t size);

/// # Safety
///
/// Same as for `std::slice::from_raw_parts`. Summarizing in terms of this API:
///
/// - `msg` must be valid for reads for `size`, and it must be properly aligned.
/// - `msg` must point to `size` consecutive u8 values.
/// - 'msg' must outlive the returned LogMessages struct, and must not be free'd
///   until fuchsia_free_log_messages has been called.
/// - The `size` of the slice must be no larger than `isize::MAX`, and adding
///   that size to data must not "wrap around" the address space. See the safety
///   documentation of pointer::offset.
/// If identity is provided, it must contain a valid moniker and URL.
///
/// The returned LogMessages may be free'd with fuchsia_free_log_messages(log_messages).
/// Free'ing the LogMessages struct does the following, in this order:
/// * Frees memory associated with each individual log message
/// * Frees the bump allocator itself (and everything allocated from it), as well as
/// the message array itself.
extern "C" LogMessages fuchsia_decode_log_messages_to_struct(const uint8_t *msg, uintptr_t size,
                                                             bool expect_extended_attribution);

/// # Safety
///
/// This should only be called with a pointer obtained through
/// `fuchsia_decode_log_message_to_json`.
extern "C" void fuchsia_free_decoded_log_message(char *msg);

/// # Safety
///
/// This should only be called with a pointer obtained through
/// `fuchsia_decode_log_messages_to_struct`.
extern "C" void fuchsia_free_log_messages(LogMessages input);

}  // extern "C"

#endif  // SRC_LIB_DIAGNOSTICS_LOG_MESSAGE_RUST_CPP_LOG_DECODER_LOG_DECODER_H_

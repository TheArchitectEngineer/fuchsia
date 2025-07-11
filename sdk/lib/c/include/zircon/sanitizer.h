// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_SANITIZER_H_
#define ZIRCON_SANITIZER_H_

// Interfaces declared in this file are intended for the use of sanitizer
// runtime library implementation code.  Each sanitizer runtime works only
// with the appropriately sanitized build of libc.  These functions should
// never be called when using the unsanitized libc.  But these names are
// always exported so that the libc ABI is uniform across sanitized and
// unsanitized builds (only unsanitized shared library binaries are used at
// link time, including linking the sanitizer runtime shared libraries).

#include <stdbool.h>
#include <stdint.h>
#include <string.h>
#include <threads.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

__BEGIN_CDECLS

// Forward declaration for <link.h>.
struct dl_phdr_info;

// These are aliases for the functions defined in libc, which are always
// the unsanitized versions.  The sanitizer runtimes can call them by these
// aliases when they are overriding libc's definitions of the unadorned
// symbols.
__typeof(memcpy) __unsanitized_memcpy;
__typeof(memmove) __unsanitized_memmove;
__typeof(memset) __unsanitized_memset;

// The sanitized libc allocates the shadow memory in the appropriate ratio for
// the particular sanitizer (shadow_base == shadow_limit >> SHADOW_SCALE)
// early during startup, before any other address space allocations can occur.
// Shadow memory always starts at address zero:
//     [memory_limit,   UINTPTR_MAX)    Address space reserved by the system.
//     [shadow_limit,   memory_limit)   Address space available to the user.
//     [shadow_base,    shadow_limit)   Shadow memory, preallocated.
//     [0,              shadow_base)    Shadow gap, cannot be allocated.
typedef struct saniziter_shadow_bounds {
  uintptr_t shadow_base;
  uintptr_t shadow_limit;
  uintptr_t memory_limit;
} sanitizer_shadow_bounds_t;

// Returns the shadow bounds for the current process.
sanitizer_shadow_bounds_t __sanitizer_shadow_bounds(void);

// Fill the shadow memory corresponding to [base, base+size) with |value|. The
// threshold is used as a hint to determine when to switch to a more efficient
// mechanism when zero-filling large shadow regions. This assumes that both
// |base| and |size| are aligned to the shadow multiple.
void __sanitizer_fill_shadow(uintptr_t base, size_t size, uint8_t value, size_t threshold);

// Write logging information from the sanitizer runtime.  The buffer
// is expected to be printable text with '\n' ending each line.
// Timestamps and globally unique identifiers of the calling process
// and thread (zx_koid_t) are attached to all messages, so there is no
// need to include those details in the text.  The log of messages
// written with this call automatically includes address and ELF build
// ID details of the program and all shared libraries sufficient to
// translate raw address values into program symbols or source
// locations via a post-processor that has access to the original ELF
// files and their debugging information.  The text can contain markup
// around address values that should be resolved symbolically; see
// TODO(mcgrathr) for the format and details of the post-processor.
void __sanitizer_log_write(const char* buffer, size_t len);

// Runtimes that have binary data to publish (e.g. coverage) use this
// interface.  The name describes the data sink that will receive this
// blob of data; the string is not used after this call returns.  The
// caller creates a VMO (e.g. zx_vmo_create) and passes it in; the VMO
// handle is consumed by this call.  Each particular data sink has its
// own conventions about both the format of the data in the VMO and
// the protocol for when data must be written there.  For some sinks,
// the VMO's data is used immediately.  For other sinks, the caller is
// expected to have the VMO mapped in and be writing more data there
// throughout the life of the process, to be analyzed only after the
// process terminates.  Yet others might use an asynchronous shared
// memory protocol between producer and consumer.  The return value is
// either ZX_HANDLE_INVALID or a Zircon handle whose lifetime is used
// to signal the readiness of the data in the VMO.  This handle can be
// passed to zx_handle_close() to indicate the data is ready to be
// consumed.  Or the handle can safely be leaked by just ignoring the
// return value; the data will be ready when the process exits.  Note
// there is no indication of success or failure returned here (though
// it may be logged).  A value of ZX_HANDLE_INVALID merely indicates
// there is no way to communicate data readiness before process exit.
zx_handle_t __sanitizer_publish_data(const char* sink_name, zx_handle_t vmo);

// Changes protection of the code in the range of len bytes starting
// from addr. The writable argument specifies whether the code should
// be made writable or not. This function is only valid on ranges within
// the caller's own code segment.
// TODO(phosek) removes this when the proper debugging interface exists.
zx_status_t __sanitizer_change_code_protection(uintptr_t addr, size_t len, bool writable);

// This stops all other threads in the process so memory should be quiescent.
// Then it makes callbacks for memory regions containing non-const global
// variables, thread stacks, thread registers, and thread-local storage
// regions (this includes thread_local variables as well as tss_set or
// pthread_setspecific values).  Each callback is optional; no such callbacks
// are made if a null function pointer is given.  The memory region passed to
// each callback can be accessed only during that single callback and might no
// longer be valid once the callback returns.  Then it makes a final callback
// before allowing other threads to resume running normally.  If there are
// problems stopping threads, no memory callbacks will be made and the
// argument to the final callback will get an error code rather than ZX_OK.
//
// NOTE: Users should be very careful of what they do in their callbacks.
// All other threads are suspended, but they could still be holding locks.
// For example, calling `printf` from the callback could cause a deadlock if
// another thread was suspended mid-`printf`. Each callback is meant to scan
// over a region of memory and should not do more than that. Callbacks should
// not use other libc or other library functions other than the simplest things
// like memcpy.
typedef void sanitizer_memory_snapshot_callback_t(void* mem, size_t len, void* arg);
void __sanitizer_memory_snapshot(sanitizer_memory_snapshot_callback_t* globals,
                                 sanitizer_memory_snapshot_callback_t* stacks,
                                 sanitizer_memory_snapshot_callback_t* regs,
                                 sanitizer_memory_snapshot_callback_t* tls,
                                 void (*done)(zx_status_t, void*), void* arg);

// This does a fast, best-effort attempt to collect a backtrace.  It writes PC
// values (return addresses) for up to max_frames call frames into the
// pc_buffer, and returns the number of frames collected.  The first frame
// (pc_buffer[0]) will be the caller of __sanitizer_fast_backtrace (and that's
// the only frame guaranteed to be collected), the second will be that frame's
// caller, and so on.  This is safe even if register and memory state is bogus.
// It's best-effort and results will be imprecise in the face of code that
// doesn't use either shadow-call-stack or frame pointers.
size_t __sanitizer_fast_backtrace(uintptr_t* pc_buffer, size_t max_frames);

// The "hook" interfaces are functions that the sanitizer runtime library
// can define and libc will call.  There are default definitions in libc
// which do nothing, but any other definitions will override those.  These
// declarations use __EXPORT (i.e. explicit STV_DEFAULT) to ensure any user
// definitions are seen by libc even if the user code is being compiled
// with -fvisibility=hidden or equivalent.

// This is called once for each ELF module loaded, including the main executable,
// its shared library dependencies, and modules loaded later via dlopen and their
// dependencies. It's always called after constant initialization, including PT_TLS
// segment initialization and dynamic relocation, have been done for the module and
// its dependencies; but before static constructors or any code from them has run.
// At program startup, this is called for the executable and its dependencies in
// load order, before `__sanitizer_startup_hook` (below) is called. Note that this
// is before general C library initialization, but after the Fuchsia Compiler ABI
// and proper thread stacks are in place.  So while normally-compiled C and C++
// code can be used here, it must not call into any library functions that might
// depend on initialization. For dynamic loading, this will be called before static
// constructors run and thus before dlopen returns.
__EXPORT void __sanitizer_module_loaded(const struct dl_phdr_info* info, size_t size);

// This is called at program startup, with the arguments that will be
// passed to main.  This is called before any other application code,
// including both static constructors and initialization of things like
// fdio and zx_take_startup_handle. It's basically the next thing called
// after `__sanitizer_module_loaded` (above) is called, after libc's most
// basic internal global initialization is complete and the initial thread
// has switched to its real thread stack.  Since not even all of libc's own
// constructors have run yet, this should not call into libc or other library
// code.
__EXPORT void __sanitizer_startup_hook(int argc, char** argv, char** envp, void* stack_base,
                                       size_t stack_size);

// This is called when a new thread has been created but is not yet
// running.  Its C11 thrd_t value has been determined and its stack has
// been allocated.  All that remains is to actually start the thread
// running (which can fail only in catastrophic bug situations).  Its
// return value will be passed to __sanitizer_thread_create_hook, below.
__EXPORT void* __sanitizer_before_thread_create_hook(thrd_t thread, bool detached, const char* name,
                                                     void* stack_base, size_t stack_size);

// This is called after a new thread has been created or creation has
// failed at the final stage; __sanitizer_before_thread_create_hook has
// been called first, and its return value is the first argument here.
// The second argument is what the return value of C11 thrd_create would
// be for this creation attempt (which might have been instigated by
// either thrd_create or pthread_create).  If it's thrd_success, then
// the new thread has now started running.  Otherwise (it's a different
// <threads.h> thrd_* value), thread creation has failed and the thread
// details reported to __sanitizer_before_thread_create_hook will be
// freed without the thread ever starting.
__EXPORT void __sanitizer_thread_create_hook(void* hook, thrd_t thread, int error);

// This is called in each new thread as it starts up.  The argument is
// the same one returned by __sanitizer_before_thread_create_hook and
// previously passed to __sanitizer_thread_create_hook.
__EXPORT void __sanitizer_thread_start_hook(void* hook, thrd_t self);

// This is called in each thread just before it dies.
// All thread-specific destructors have been run.
// The argument is the same one passed to __sanitizer_thread_start_hook.
__EXPORT void __sanitizer_thread_exit_hook(void* hook, thrd_t self);

// This is called with the argument to _exit and its return value
// is the actual exit status for the process.
__EXPORT int __sanitizer_process_exit_hook(int status);

__END_CDECLS

#endif  // ZIRCON_SANITIZER_H_

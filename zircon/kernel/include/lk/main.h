// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_LK_MAIN_H_
#define ZIRCON_KERNEL_INCLUDE_LK_MAIN_H_

#include <sys/types.h>
#include <zircon/compiler.h>

__BEGIN_CDECLS

// Forward declaration; defined in <phys/handoff.h>.
struct PhysHandoff;

// main entry point from boot assembly
void lk_main(PhysHandoff* handoff) __NO_RETURN __EXTERNALLY_VISIBLE;

void lk_secondary_cpu_entry(void);
void lk_init_secondary_cpus(uint secondary_cpu_count);

// Returns true if global ctors have been called.
bool lk_global_constructors_called(void);

__END_CDECLS

#endif  // ZIRCON_KERNEL_INCLUDE_LK_MAIN_H_

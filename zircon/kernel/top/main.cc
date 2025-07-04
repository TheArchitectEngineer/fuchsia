// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/*
 * Main entry point to the OS. Initializes modules in order and creates
 * the default thread.
 */
#include "lk/main.h"

#include <arch.h>
#include <debug.h>
#include <lib/console.h>
#include <lib/counters.h>
#include <lib/cxxabi-dynamic-init/cxxabi-dynamic-init.h>
#include <lib/debuglog.h>
#include <lib/heap.h>
#include <lib/jtrace/jtrace.h>
#include <lib/lockup_detector.h>
#include <lib/userabi/userboot.h>
#include <platform.h>
#include <string.h>
#include <zircon/compiler.h>

#include <dev/init.h>
#include <kernel/cpu.h>
#include <kernel/init.h>
#include <kernel/mutex.h>
#include <kernel/thread.h>
#include <kernel/topology.h>
#include <lk/init.h>
#include <phys/handoff.h>
#include <vm/init.h>
#include <vm/vm.h>

static uint secondary_idle_thread_count;

static int bootstrap2(void* arg);

KCOUNTER(timeline_threading, "boot.timeline.threading")
KCOUNTER(timeline_init, "boot.timeline.init")

static bool lk_global_constructors_called_flag = false;

extern void (*const __init_array_start[])();
extern void (*const __init_array_end[])();

bool lk_global_constructors_called() { return lk_global_constructors_called_flag; }

static void call_constructors() {
  for (void (*const* a)() = __init_array_start; a != __init_array_end; a++) {
    (*a)();
  }

  lk_global_constructors_called_flag = true;
}

namespace cxxabi_dynamic_init::internal {
bool ConstructorsCalled() { return lk_global_constructors_called(); }
}  // namespace cxxabi_dynamic_init::internal

// called from arch code
void lk_main(PhysHandoff* handoff) {
  HandoffFromPhys(handoff);

  // After HandoffFromPhys(), gPhysHandoff should now be set.
  ZX_DEBUG_ASSERT(gPhysHandoff != nullptr);

  // Initialize debug tracing (if enabled) as early as possible. This allows
  // debug tracing to be used before the debug log comes up, and before global
  // constructors are executed.  Note that if debug tracing is configured to be
  // persistent, then trace records will be dropped until we get to the point
  // that the ZBI is processed and our NVRAM location is discovered.
  jtrace_init();

  // get us into some sort of thread context so Thread::Current works.
  thread_init_early();

  // Now that Thread::Current works, jtrace is allowed to capture TIDs and
  // disable preemption while recording entries.
  jtrace_set_after_thread_init_early();

  // bring the debuglog up early so we can safely printf
  dlog_init_early();

  // deal with any static constructors
  call_constructors();

  // we can safely printf now since we have the debuglog, the current thread set
  // which holds (a per-line buffer), and global ctors finished (some of the
  // printf machinery depends on ctors right now).
  // NOTE: botanist depends on this string being printed to serial. If this changes,
  // that code must be changed as well. See https://fxbug.dev/42138089#c20.
  dprintf(ALWAYS, "printing enabled\n");

  // At this point the physmap (set up in start.S) is available and all static
  // constructors (if needed) have been run.

  lk_primary_cpu_init_level(LK_INIT_LEVEL_EARLIEST, LK_INIT_LEVEL_ARCH_EARLY - 1);

  // Carry out any early architecture-specific and platform-specific init
  // required to get the boot CPU and platform into a known state.
  arch_early_init();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_ARCH_EARLY, LK_INIT_LEVEL_PLATFORM_EARLY - 1);

  platform_early_init();
  DriverHandoffEarly(*gPhysHandoff);
  lk_primary_cpu_init_level(LK_INIT_LEVEL_PLATFORM_EARLY, LK_INIT_LEVEL_ARCH_PREVM - 1);

  // At this point, the kernel command line and serial are set up.

  dprintf(INFO, "\nwelcome to Zircon\n\n");
  dprintf(SPEW, "KASLR: Kernel image at %p\n", __executable_start);

  // Perform any additional arch and platform-specific set up that needs to be done
  // before virtual memory or the heap are set up.
  dprintf(SPEW, "initializing arch pre-vm\n");
  arch_prevm_init();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_ARCH_PREVM, LK_INIT_LEVEL_PLATFORM_PREVM - 1);
  dprintf(SPEW, "initializing platform pre-vm\n");
  platform_prevm_init();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_PLATFORM_PREVM, LK_INIT_LEVEL_VM_PREHEAP - 1);

  // perform basic virtual memory setup
  dprintf(SPEW, "initializing vm pre-heap\n");
  vm_init_preheap();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_VM_PREHEAP, LK_INIT_LEVEL_HEAP - 1);

  // bring up the kernel heap
  dprintf(SPEW, "initializing heap\n");
  heap_init();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_HEAP, LK_INIT_LEVEL_VM - 1);

  // enable virtual memory
  dprintf(SPEW, "initializing vm\n");
  vm_init();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_VM, LK_INIT_LEVEL_TOPOLOGY - 1);

  // Initialize the lockup detector, after the platform timer has been
  // configured, but before the topology subsystem has brought up other CPUs.
  dprintf(SPEW, "initializing lockup detector on boot cpu\n");
  lockup_init();
  lockup_percpu_init();

  // initialize the system topology
  dprintf(SPEW, "initializing system topology\n");
  topology_init();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_TOPOLOGY, LK_INIT_LEVEL_KERNEL - 1);

  // initialize other parts of the kernel
  dprintf(SPEW, "initializing kernel\n");
  kernel_init();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_KERNEL, LK_INIT_LEVEL_THREADING - 1);

  // Mark the current CPU as being active, then create a thread to complete
  // system initialization
  dprintf(SPEW, "creating bootstrap completion thread\n");
  Scheduler::SetCurrCpuActive(true);
  Thread* t = Thread::Create("bootstrap2", &bootstrap2, NULL, DEFAULT_PRIORITY);
  // As this thread will initialize per-CPU state, ensure that it runs on the boot CPU.
  t->SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID));
  t->Detach();
  t->Resume();

  // become the idle thread and enable interrupts to start the scheduler
  Thread::Current::BecomeIdle();
}

static int bootstrap2(void*) {
  DEBUG_ASSERT(arch_curr_cpu_num() == BOOT_CPU_ID);

  timeline_threading.Set(current_mono_ticks());

  dprintf(SPEW, "top of bootstrap2()\n");

  // Initialize the rest of the architecture and platform.
  lk_primary_cpu_init_level(LK_INIT_LEVEL_THREADING, LK_INIT_LEVEL_ARCH - 1);

  dprintf(SPEW, "initializing arch\n");
  arch_init();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_ARCH, LK_INIT_LEVEL_PLATFORM - 1);

  dprintf(SPEW, "initializing platform\n");
  platform_init();
  DriverHandoffLate(*gPhysHandoff);
  lk_primary_cpu_init_level(LK_INIT_LEVEL_PLATFORM, LK_INIT_LEVEL_ARCH_LATE - 1);

  // At this point, other cores in the system have been started (though may
  // not yet be online).  Signal that the boot CPU is ready.
  mp_signal_curr_cpu_ready();

  // Perform per-CPU set up on the boot CPU.
  dprintf(SPEW, "initializing late arch\n");
  arch_late_init_percpu();
  lk_primary_cpu_init_level(LK_INIT_LEVEL_ARCH_LATE, LK_INIT_LEVEL_USER - 1);

  // End hand-off before shell initialization, as we want kernel state to be
  // 'finalized' before we run any kernel scripts (e.g., for unit-testing).
  HandoffEnd handoff_end = EndHandoff();

  // Give the kernel shell an opportunity to run. If it exits this function, continue booting.
  kernel_shell_init();

  dprintf(SPEW, "starting user space\n");
  userboot_init(ktl::move(handoff_end));

  dprintf(SPEW, "moving to last init level\n");
  lk_primary_cpu_init_level(LK_INIT_LEVEL_USER, LK_INIT_LEVEL_LAST);

  timeline_init.Set(current_mono_ticks());
  return 0;
}

void lk_secondary_cpu_entry() {
  cpu_num_t cpu = arch_curr_cpu_num();
  DEBUG_ASSERT(cpu != 0);

  if (cpu > secondary_idle_thread_count) {
    dprintf(CRITICAL,
            "Invalid secondary cpu num %u, SMP_MAX_CPUS %d, secondary_idle_thread_count %u\n", cpu,
            SMP_MAX_CPUS, secondary_idle_thread_count);
    return;
  }

  // late CPU initialization for secondary CPUs
  arch_late_init_percpu();

  // secondary cpu initialize from threading level up. 0 to threading was handled in arch
  lk_init_level(LK_INIT_FLAG_SECONDARY_CPUS, LK_INIT_LEVEL_THREADING, LK_INIT_LEVEL_LAST);

  lockup_percpu_init();

  dprintf(SPEW, "entering scheduler on cpu %u\n", cpu);
  thread_secondary_cpu_entry();
}

void lk_init_secondary_cpus(uint secondary_cpu_count) {
  if (secondary_cpu_count >= SMP_MAX_CPUS) {
    dprintf(CRITICAL, "Invalid secondary_cpu_count %u, SMP_MAX_CPUS %d\n", secondary_cpu_count,
            SMP_MAX_CPUS);
    secondary_cpu_count = SMP_MAX_CPUS - 1;
  }

  secondary_idle_thread_count = 0;
  for (uint i = 0; i < secondary_cpu_count; i++) {
    Thread* t = Thread::CreateIdleThread(i + 1);
    if (!t) {
      dprintf(CRITICAL, "could not allocate idle thread %u\n", i + 1);
      break;
    }
    secondary_idle_thread_count++;
  }
}

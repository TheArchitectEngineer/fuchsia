// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>

#include <arch/arch_ops.h>
#include <arch/mp.h>
#include <fbl/alloc_checker.h>
#include <fbl/array.h>
#include <kernel/auto_preempt_disabler.h>
#include <kernel/cpu.h>
#include <kernel/dpc.h>
#include <kernel/event.h>
#include <ktl/array.h>
#include <ktl/atomic.h>
#include <ktl/unique_ptr.h>

#include "tests.h"

#include <ktl/enforce.h>

static bool test_dpc_queue() {
  BEGIN_TEST;

  struct Context {
    Context() : dpc(&SignalEvent, &dpc) {}

    static void SignalEvent(Dpc* dpc) {
      auto* const context = dpc->arg<Context>();
      context->dpc_started = true;

      // DPCs allow interrupts and blocking.
      DEBUG_ASSERT(!arch_ints_disabled());
      DEBUG_ASSERT(!arch_blocking_disallowed());
      DEBUG_ASSERT(context->expected_cpu == arch_curr_cpu_num());

      context->event.Signal();
    }

    Dpc dpc;
    Event event;
    ktl::atomic<cpu_num_t> expected_cpu;
    ktl::atomic<bool> dpc_started;
  };

  static constexpr int kNumDPCs = 72;

  fbl::AllocChecker ac;
  auto context = fbl::MakeArray<Context>(&ac, kNumDPCs);
  ASSERT_TRUE(ac.check());

  // Init all the DPCs and supporting context.
  for (int i = 0; i < kNumDPCs; i++) {
    context[i].dpc_started = false;
  }

  // Fire off DPCs.
  for (int i = 0; i < kNumDPCs; i++) {
    AutoPreemptDisabler preempt_disable;
    interrupt_saved_state_t int_state = arch_interrupt_save();
    context[i].expected_cpu = arch_curr_cpu_num();
    // Use a mix of both general and low-latency.
    const DpcRunner::QueueType type =
        (i % 2) ? DpcRunner::QueueType::General : DpcRunner::QueueType::LowLatency;
    DpcRunner::Enqueue(context[i].dpc, type);
    arch_interrupt_restore(int_state);
  }
  for (int i = 0; i < kNumDPCs; i++) {
    context[i].event.Wait();
  }

  END_TEST;
}

// Test that it's safe to repeatedly queue up the same DPC over and over.
static bool test_dpc_requeue() {
  BEGIN_TEST;

  // Disable preemption to prevent the DCP worker, which is a deadline thread,
  // from immediately preempting the test thread. This also ensures that the
  // test thread remains on the same CPU as the DPC is enqueued on, othewise
  // work stealing can move the test thread to another CPU while the DPC worker
  // executes, resulting in a race between the Dpc destructor in the test thread
  // and the DPC worker.
  AutoPreemptDisabler preempt_disable;

  ktl::atomic<uint64_t> actual_count = 0;
  Dpc dpc_increment([](Dpc* d) { d->arg<ktl::atomic<uint64_t>>()->fetch_add(1); }, &actual_count);

  constexpr uint64_t kNumIterations = 10000;
  uint64_t expected_count = 0;
  for (unsigned i = 0; i < kNumIterations; ++i) {
    // If we queue faster than the DPC worker thread can dequeue, the call may fail with
    // ZX_ERR_ALREADY_EXISTS.  That's OK, we just won't increment |expected_count| in that case.
    zx_status_t status = DpcRunner::Enqueue(dpc_increment);
    if (status == ZX_OK) {
      ++expected_count;
    } else {
      ASSERT_EQ(status, ZX_ERR_ALREADY_EXISTS);
    }
  }

  // There might still be one DPC queued up for execution.  Wait for it to "flush" the queue.
  Event event_flush;
  Dpc dpc_flush([](Dpc* d) { d->arg<Event>()->Signal(); }, &event_flush);
  DpcRunner::Enqueue(dpc_flush);
  event_flush.Wait(Deadline::no_slack(ZX_TIME_INFINITE));

  ASSERT_EQ(actual_count.load(), expected_count);

  END_TEST;
}

UNITTEST_START_TESTCASE(dpc_tests)
UNITTEST("basic test of Dpc::Queue", test_dpc_queue)
UNITTEST("repeatedly queue the same dpc", test_dpc_requeue)
UNITTEST_END_TESTCASE(dpc_tests, "dpc_tests", "Tests of DPCs")

// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_TIMER_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_TIMER_DISPATCHER_H_

#include <sys/types.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <kernel/dpc.h>
#include <kernel/timer.h>
#include <object/dispatcher.h>
#include <object/handle.h>

class TimerDispatcher final : public SoloDispatcher<TimerDispatcher, ZX_DEFAULT_TIMER_RIGHTS> {
 public:
  static zx_status_t Create(uint32_t options, zx_clock_t clock_id,
                            KernelHandle<TimerDispatcher>* handle, zx_rights_t* rights);

  ~TimerDispatcher() final;
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_TIMER; }
  void on_zero_handles() final;

  // Timer specific ops.
  zx_status_t Set(zx_time_t deadline, zx_duration_t slack_amount);
  zx_status_t Cancel();

  // Timer callback.
  void OnTimerFired();

  zx_info_timer_t GetInfo() const;

 private:
  explicit TimerDispatcher(uint32_t options, zx_clock_t clock_id);
  void SetTimerLocked(bool cancel_first) TA_REQ(get_lock());
  bool CancelTimerLocked() TA_REQ(get_lock());

  const uint32_t options_;
  const zx_clock_t clock_id_;
  Dpc timer_dpc_;
  // The deadline should be interpreted as:
  // * zx_instant_mono_t if clock_id_ equals ZX_CLOCK_MONOTONIC.
  // * zx_instant_boot_t if clock_id_ equals ZX_CLOCK_BOOT.
  zx_time_t deadline_ TA_GUARDED(get_lock());
  zx_duration_t slack_amount_ TA_GUARDED(get_lock());
  bool cancel_pending_ TA_GUARDED(get_lock());
  Timer timer_ TA_GUARDED(get_lock());
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_TIMER_DISPATCHER_H_

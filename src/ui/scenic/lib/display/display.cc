// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/display.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.images2/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <zircon/syscalls.h>

#include "src/ui/scenic/lib/utils/logging.h"

namespace scenic_impl {
namespace display {

Display::Display(fuchsia_hardware_display_types::wire::DisplayId id, uint32_t width_in_px,
                 uint32_t height_in_px, uint32_t width_in_mm, uint32_t height_in_mm,
                 std::vector<fuchsia_images2::PixelFormat> pixel_formats,
                 uint32_t maximum_refresh_rate_in_millihertz)
    : vsync_timing_(std::make_shared<scheduling::VsyncTiming>()),
      display_id_(id),
      width_in_px_(width_in_px),
      height_in_px_(height_in_px),
      width_in_mm_(width_in_mm),
      height_in_mm_(height_in_mm),
      pixel_formats_(std::move(pixel_formats)),
      maximum_refresh_rate_in_millihertz_(maximum_refresh_rate_in_millihertz) {
  zx::event::create(0, &ownership_event_);
  device_pixel_ratio_.store({1.f, 1.f});

  // Most displays will have a longer interval.  If so, `OnVsync()` will adjust.
  vsync_timing_->set_vsync_interval(kMinimumVsyncInterval);
}
Display::Display(fuchsia_hardware_display_types::wire::DisplayId id, uint32_t width_in_px,
                 uint32_t height_in_px)
    : Display(id, width_in_px, height_in_px, 0, 0, {fuchsia_images2::PixelFormat::kB8G8R8A8}, 0) {}

void Display::Claim() {
  FX_DCHECK(!claimed_);
  claimed_ = true;
}

void Display::Unclaim() {
  FX_DCHECK(claimed_);
  claimed_ = false;
}

void Display::OnVsync(zx::time timestamp,
                      fuchsia_hardware_display::wire::ConfigStamp applied_config_stamp) {
  // Estimate current vsync interval. Need to include a maximum to mitigate any
  // potential issues during long breaks.
  const zx::duration time_since_last_vsync = timestamp - vsync_timing_->last_vsync_time();
  if (time_since_last_vsync < kMaximumVsyncInterval) {
    vsync_timing_->set_vsync_interval(std::max(kMinimumVsyncInterval, time_since_last_vsync));
  }

  vsync_timing_->set_last_vsync_time(timestamp);

  TRACE_INSTANT("gfx", "Display::OnVsync", TRACE_SCOPE_PROCESS, "Timestamp", timestamp.get(),
                "Vsync interval", vsync_timing_->vsync_interval().get());

  if (vsync_callback_) {
    FLATLAND_VERBOSE_LOG << "Display::OnVsync(): display_id=" << display_id_.value
                         << "  timestamp=" << timestamp.get()
                         << "  applied_config_stamp=" << applied_config_stamp.value
                         << "  ... invoking vsync callback";
    vsync_callback_(timestamp, applied_config_stamp);
  }
}

}  // namespace display
}  // namespace scenic_impl

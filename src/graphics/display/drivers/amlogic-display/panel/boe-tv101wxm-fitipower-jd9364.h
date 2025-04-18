// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_AMLOGIC_DISPLAY_PANEL_BOE_TV101WXM_FITIPOWER_JD9364_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_AMLOGIC_DISPLAY_PANEL_BOE_TV101WXM_FITIPOWER_JD9364_H_

#include <lib/mipi-dsi/mipi-dsi.h>

#include <cstdint>

#include "src/graphics/display/drivers/amlogic-display/panel-config.h"

namespace amlogic_display {

// clang-format off

constexpr uint8_t lcd_init_sequence_BOE_TV101WXM_FITIPOWER_JD9364[] = {
    kDsiOpSleep, 10,
    kDsiOpGpio, 3, 0, 1, 30,
    kDsiOpGpio, 3, 0, 0, 10,
    kDsiOpGpio, 3, 0, 1, 30,
    kDsiOpReadReg, 2, 4, 3,
    kDsiOpSleep, 10,

    kDsiOpSleep, 120,
    kMipiDsiDtGenShortWrite2, 2, 0xe0, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0xe1, 0x93,
    kMipiDsiDtGenShortWrite2, 2, 0xe2, 0x65,
    kMipiDsiDtGenShortWrite2, 2, 0xe3, 0xf8,
    kMipiDsiDtGenShortWrite2, 2, 0x80, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0xe0, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x00, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x01, 0x6f,
    kMipiDsiDtGenShortWrite2, 2, 0x17, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x18, 0xaf,
    kMipiDsiDtGenShortWrite2, 2, 0x19, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x1a, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x1b, 0xaf,
    kMipiDsiDtGenShortWrite2, 2, 0x1c, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x1f, 0x3e,
    kMipiDsiDtGenShortWrite2, 2, 0x20, 0x28,
    kMipiDsiDtGenShortWrite2, 2, 0x21, 0x28,
    kMipiDsiDtGenShortWrite2, 2, 0x22, 0x7e,
    kMipiDsiDtGenShortWrite2, 2, 0x35, 0x26,
    kMipiDsiDtGenShortWrite2, 2, 0x37, 0x09,
    kMipiDsiDtGenShortWrite2, 2, 0x38, 0x04,
    kMipiDsiDtGenShortWrite2, 2, 0x39, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x3a, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x3c, 0x78,
    kMipiDsiDtGenShortWrite2, 2, 0x3d, 0xff,
    kMipiDsiDtGenShortWrite2, 2, 0x3e, 0xff,
    kMipiDsiDtGenShortWrite2, 2, 0x3f, 0x7f,
    kMipiDsiDtGenShortWrite2, 2, 0x40, 0x06,
    kMipiDsiDtGenShortWrite2, 2, 0x41, 0xa0,
    kMipiDsiDtGenShortWrite2, 2, 0x42, 0x81,
    kMipiDsiDtGenShortWrite2, 2, 0x43, 0x08,
    kMipiDsiDtGenShortWrite2, 2, 0x44, 0x0b,
    kMipiDsiDtGenShortWrite2, 2, 0x45, 0x28,
    kMipiDsiDtGenShortWrite2, 2, 0x55, 0x0f,
    kMipiDsiDtGenShortWrite2, 2, 0x57, 0x69,
    kMipiDsiDtGenShortWrite2, 2, 0x59, 0x0a,
    kMipiDsiDtGenShortWrite2, 2, 0x5a, 0x28,
    kMipiDsiDtGenShortWrite2, 2, 0x5b, 0x14,
    kMipiDsiDtGenShortWrite2, 2, 0x5d, 0x7f,
    kMipiDsiDtGenShortWrite2, 2, 0x5e, 0x6a,
    kMipiDsiDtGenShortWrite2, 2, 0x5f, 0x5a,
    kMipiDsiDtGenShortWrite2, 2, 0x60, 0x4e,
    kMipiDsiDtGenShortWrite2, 2, 0x61, 0x4a,
    kMipiDsiDtGenShortWrite2, 2, 0x62, 0x3a,
    kMipiDsiDtGenShortWrite2, 2, 0x63, 0x3c,
    kMipiDsiDtGenShortWrite2, 2, 0x64, 0x23,
    kMipiDsiDtGenShortWrite2, 2, 0x65, 0x39,
    kMipiDsiDtGenShortWrite2, 2, 0x66, 0x35,
    kMipiDsiDtGenShortWrite2, 2, 0x67, 0x34,
    kMipiDsiDtGenShortWrite2, 2, 0x68, 0x51,
    kMipiDsiDtGenShortWrite2, 2, 0x69, 0x3e,
    kMipiDsiDtGenShortWrite2, 2, 0x6a, 0x44,
    kMipiDsiDtGenShortWrite2, 2, 0x6b, 0x34,
    kMipiDsiDtGenShortWrite2, 2, 0x6c, 0x2e,
    kMipiDsiDtGenShortWrite2, 2, 0x6d, 0x21,
    kMipiDsiDtGenShortWrite2, 2, 0x6e, 0x0e,
    kMipiDsiDtGenShortWrite2, 2, 0x6f, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x70, 0x7f,
    kMipiDsiDtGenShortWrite2, 2, 0x71, 0x6a,
    kMipiDsiDtGenShortWrite2, 2, 0x72, 0x5a,
    kMipiDsiDtGenShortWrite2, 2, 0x73, 0x4e,
    kMipiDsiDtGenShortWrite2, 2, 0x74, 0x4a,
    kMipiDsiDtGenShortWrite2, 2, 0x75, 0x3a,
    kMipiDsiDtGenShortWrite2, 2, 0x76, 0x3c,
    kMipiDsiDtGenShortWrite2, 2, 0x77, 0x23,
    kMipiDsiDtGenShortWrite2, 2, 0x78, 0x39,
    kMipiDsiDtGenShortWrite2, 2, 0x79, 0x35,
    kMipiDsiDtGenShortWrite2, 2, 0x7a, 0x34,
    kMipiDsiDtGenShortWrite2, 2, 0x7b, 0x51,
    kMipiDsiDtGenShortWrite2, 2, 0x7c, 0x3e,
    kMipiDsiDtGenShortWrite2, 2, 0x7d, 0x44,
    kMipiDsiDtGenShortWrite2, 2, 0x7e, 0x34,
    kMipiDsiDtGenShortWrite2, 2, 0x7f, 0x2e,
    kMipiDsiDtGenShortWrite2, 2, 0x80, 0x21,
    kMipiDsiDtGenShortWrite2, 2, 0x81, 0x0e,
    kMipiDsiDtGenShortWrite2, 2, 0x82, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0xe0, 0x02,
    kMipiDsiDtGenShortWrite2, 2, 0x00, 0x1e,
    kMipiDsiDtGenShortWrite2, 2, 0x01, 0x1e,
    kMipiDsiDtGenShortWrite2, 2, 0x02, 0x41,
    kMipiDsiDtGenShortWrite2, 2, 0x03, 0x41,
    kMipiDsiDtGenShortWrite2, 2, 0x04, 0x43,
    kMipiDsiDtGenShortWrite2, 2, 0x05, 0x43,
    kMipiDsiDtGenShortWrite2, 2, 0x06, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x07, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x08, 0x35,
    kMipiDsiDtGenShortWrite2, 2, 0x09, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x0a, 0x15,
    kMipiDsiDtGenShortWrite2, 2, 0x0b, 0x15,
    kMipiDsiDtGenShortWrite2, 2, 0x0c, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x0d, 0x47,
    kMipiDsiDtGenShortWrite2, 2, 0x0e, 0x47,
    kMipiDsiDtGenShortWrite2, 2, 0x0f, 0x45,
    kMipiDsiDtGenShortWrite2, 2, 0x10, 0x45,
    kMipiDsiDtGenShortWrite2, 2, 0x11, 0x4b,
    kMipiDsiDtGenShortWrite2, 2, 0x12, 0x4b,
    kMipiDsiDtGenShortWrite2, 2, 0x13, 0x49,
    kMipiDsiDtGenShortWrite2, 2, 0x14, 0x49,
    kMipiDsiDtGenShortWrite2, 2, 0x15, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x16, 0x1e,
    kMipiDsiDtGenShortWrite2, 2, 0x17, 0x1e,
    kMipiDsiDtGenShortWrite2, 2, 0x18, 0x40,
    kMipiDsiDtGenShortWrite2, 2, 0x19, 0x40,
    kMipiDsiDtGenShortWrite2, 2, 0x1a, 0x42,
    kMipiDsiDtGenShortWrite2, 2, 0x1b, 0x42,
    kMipiDsiDtGenShortWrite2, 2, 0x1c, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x1d, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x1e, 0x35,
    kMipiDsiDtGenShortWrite2, 2, 0x1f, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x20, 0x15,
    kMipiDsiDtGenShortWrite2, 2, 0x21, 0x15,
    kMipiDsiDtGenShortWrite2, 2, 0x22, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x23, 0x46,
    kMipiDsiDtGenShortWrite2, 2, 0x24, 0x46,
    kMipiDsiDtGenShortWrite2, 2, 0x25, 0x44,
    kMipiDsiDtGenShortWrite2, 2, 0x26, 0x44,
    kMipiDsiDtGenShortWrite2, 2, 0x27, 0x4a,
    kMipiDsiDtGenShortWrite2, 2, 0x28, 0x4a,
    kMipiDsiDtGenShortWrite2, 2, 0x29, 0x48,
    kMipiDsiDtGenShortWrite2, 2, 0x2a, 0x48,
    kMipiDsiDtGenShortWrite2, 2, 0x2b, 0x1f,
    kMipiDsiDtGenShortWrite2, 2, 0x58, 0x40,
    kMipiDsiDtGenShortWrite2, 2, 0x5b, 0x30,
    kMipiDsiDtGenShortWrite2, 2, 0x5c, 0x0f,
    kMipiDsiDtGenShortWrite2, 2, 0x5d, 0x30,
    kMipiDsiDtGenShortWrite2, 2, 0x5e, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x5f, 0x02,
    kMipiDsiDtGenShortWrite2, 2, 0x63, 0x14,
    kMipiDsiDtGenShortWrite2, 2, 0x64, 0x6a,
    kMipiDsiDtGenShortWrite2, 2, 0x67, 0x73,
    kMipiDsiDtGenShortWrite2, 2, 0x68, 0x11,
    kMipiDsiDtGenShortWrite2, 2, 0x69, 0x14,
    kMipiDsiDtGenShortWrite2, 2, 0x6a, 0x6a,
    kMipiDsiDtGenShortWrite2, 2, 0x6b, 0x08,
    kMipiDsiDtGenShortWrite2, 2, 0x6c, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x6d, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x6e, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x6f, 0x88,
    kMipiDsiDtGenShortWrite2, 2, 0x77, 0xdd,
    kMipiDsiDtGenShortWrite2, 2, 0x79, 0x0e,
    kMipiDsiDtGenShortWrite2, 2, 0x7a, 0x0f,
    kMipiDsiDtGenShortWrite2, 2, 0x7d, 0x14,
    kMipiDsiDtGenShortWrite2, 2, 0x7e, 0x82,
    kMipiDsiDtGenShortWrite2, 2, 0xe0, 0x04,
    kMipiDsiDtGenShortWrite2, 2, 0x09, 0x11,
    kMipiDsiDtGenShortWrite2, 2, 0x0e, 0x48,
    kMipiDsiDtGenShortWrite2, 2, 0x2b, 0x2b,
    kMipiDsiDtGenShortWrite2, 2, 0x2d, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x2e, 0x44,
    kMipiDsiDtGenShortWrite2, 2, 0xe0, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0xe6, 0x02,
    kMipiDsiDtGenShortWrite2, 2, 0xe7, 0x0c,
    kMipiDsiDtDcsShortWrite0, 1, 0x11,
    kDsiOpSleep, 100,
    kMipiDsiDtGenShortWrite2, 2, 0xe0, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x2b, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x2c, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x30, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x31, 0xfc,
    kMipiDsiDtGenShortWrite2, 2, 0x32, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x33, 0xf8,
    kMipiDsiDtGenShortWrite2, 2, 0x34, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x35, 0xf0,
    kMipiDsiDtGenShortWrite2, 2, 0x36, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x37, 0xe8,
    kMipiDsiDtGenShortWrite2, 2, 0x38, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x39, 0xe0,
    kMipiDsiDtGenShortWrite2, 2, 0x3a, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x3b, 0xd0,
    kMipiDsiDtGenShortWrite2, 2, 0x3c, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x3d, 0xc0,
    kMipiDsiDtGenShortWrite2, 2, 0x3e, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x3f, 0xa0,
    kMipiDsiDtGenShortWrite2, 2, 0x40, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x41, 0x80,
    kMipiDsiDtGenShortWrite2, 2, 0x42, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x43, 0x40,
    kMipiDsiDtGenShortWrite2, 2, 0x44, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x45, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x46, 0x02,
    kMipiDsiDtGenShortWrite2, 2, 0x47, 0x80,
    kMipiDsiDtGenShortWrite2, 2, 0x48, 0x02,
    kMipiDsiDtGenShortWrite2, 2, 0x49, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x4a, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x4b, 0xfc,
    kMipiDsiDtGenShortWrite2, 2, 0x4c, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x4d, 0x7c,
    kMipiDsiDtGenShortWrite2, 2, 0x4e, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x4f, 0xfc,
    kMipiDsiDtGenShortWrite2, 2, 0x50, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x51, 0xbc,
    kMipiDsiDtGenShortWrite2, 2, 0x52, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x53, 0x7c,
    kMipiDsiDtGenShortWrite2, 2, 0x54, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x55, 0x5c,
    kMipiDsiDtGenShortWrite2, 2, 0x56, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x57, 0x3c,
    kMipiDsiDtGenShortWrite2, 2, 0x58, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x59, 0x2c,
    kMipiDsiDtGenShortWrite2, 2, 0x5a, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x5b, 0x1c,
    kMipiDsiDtGenShortWrite2, 2, 0x5c, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x5d, 0x14,
    kMipiDsiDtGenShortWrite2, 2, 0x5e, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x5f, 0x0c,
    kMipiDsiDtGenShortWrite2, 2, 0x60, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x61, 0x04,
    kMipiDsiDtGenShortWrite2, 2, 0x62, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x63, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x64, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x65, 0xc9,
    kMipiDsiDtGenShortWrite2, 2, 0x66, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x67, 0xc6,
    kMipiDsiDtGenShortWrite2, 2, 0x68, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x69, 0xbe,
    kMipiDsiDtGenShortWrite2, 2, 0x6a, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x6b, 0xb7,
    kMipiDsiDtGenShortWrite2, 2, 0x6c, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x6d, 0xb1,
    kMipiDsiDtGenShortWrite2, 2, 0x6e, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x6f, 0xa3,
    kMipiDsiDtGenShortWrite2, 2, 0x70, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x71, 0x96,
    kMipiDsiDtGenShortWrite2, 2, 0x72, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x73, 0x79,
    kMipiDsiDtGenShortWrite2, 2, 0x74, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x75, 0x5d,
    kMipiDsiDtGenShortWrite2, 2, 0x76, 0x03,
    kMipiDsiDtGenShortWrite2, 2, 0x77, 0x26,
    kMipiDsiDtGenShortWrite2, 2, 0x78, 0x02,
    kMipiDsiDtGenShortWrite2, 2, 0x79, 0xe9,
    kMipiDsiDtGenShortWrite2, 2, 0x7a, 0x02,
    kMipiDsiDtGenShortWrite2, 2, 0x7b, 0x6e,
    kMipiDsiDtGenShortWrite2, 2, 0x7c, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x7d, 0xf3,
    kMipiDsiDtGenShortWrite2, 2, 0x7e, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x7f, 0xef,
    kMipiDsiDtGenShortWrite2, 2, 0x80, 0x01,
    kMipiDsiDtGenShortWrite2, 2, 0x81, 0x73,
    kMipiDsiDtGenShortWrite2, 2, 0x82, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x83, 0xf5,
    kMipiDsiDtGenShortWrite2, 2, 0x84, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x85, 0xb4,
    kMipiDsiDtGenShortWrite2, 2, 0x86, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x87, 0x79,
    kMipiDsiDtGenShortWrite2, 2, 0x88, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x89, 0x5d,
    kMipiDsiDtGenShortWrite2, 2, 0x8a, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x8b, 0x3c,
    kMipiDsiDtGenShortWrite2, 2, 0x8c, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x8d, 0x2b,
    kMipiDsiDtGenShortWrite2, 2, 0x8e, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x8f, 0x1c,
    kMipiDsiDtGenShortWrite2, 2, 0x90, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x91, 0x14,
    kMipiDsiDtGenShortWrite2, 2, 0x92, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x93, 0x0c,
    kMipiDsiDtGenShortWrite2, 2, 0x94, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x95, 0x04,
    kMipiDsiDtGenShortWrite2, 2, 0x96, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0x97, 0x00,
    kMipiDsiDtGenShortWrite2, 2, 0xe0, 0x00,
    kMipiDsiDtDcsShortWrite0, 1, 0x29,
    kDsiOpSleep, 0xff,
};

// clang-format on

}  // namespace amlogic_display

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_AMLOGIC_DISPLAY_PANEL_BOE_TV101WXM_FITIPOWER_JD9364_H_

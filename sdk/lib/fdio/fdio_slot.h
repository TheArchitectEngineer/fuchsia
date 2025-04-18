// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FDIO_FDIO_SLOT_H_
#define LIB_FDIO_FDIO_SLOT_H_

#include <optional>
#include <variant>

#include <fbl/ref_ptr.h>

#include "sdk/lib/fdio/internal.h"

// TODO(tamird): every operation on this type should require the global lock.
struct fdio_slot {
 public:
  fdio_slot() = default;
  fdio_slot(const fdio_slot&) = delete;

  fbl::RefPtr<fdio> get();
  fbl::RefPtr<fdio> release();

  bool try_set(fbl::RefPtr<fdio> io);

  fbl::RefPtr<fdio> replace(fbl::RefPtr<fdio> io);

  std::optional<void (fdio_slot::*)()> try_reserve();

  bool try_fill(fbl::RefPtr<fdio> io);

  bool allocated() const;

 private:
  struct available {};
  struct reserved {};

  void release_reservation();

  std::variant<available, reserved, fbl::RefPtr<fdio>> inner_;
};

#endif  // LIB_FDIO_FDIO_SLOT_H_

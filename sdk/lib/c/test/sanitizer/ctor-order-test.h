// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_C_TEST_SANITIZER_CTOR_ORDER_TEST_H_
#define LIB_C_TEST_SANITIZER_CTOR_ORDER_TEST_H_

#include <zircon/compiler.h>

enum InterposeStatus {
  DidNotInterpose,
  SuccessfullyInterposed,
  SuccessfullyInterposedByWeakSymbol,
};

struct Global {
  Global();
  InterposeStatus interposed;
};

#endif  // LIB_C_TEST_SANITIZER_CTOR_ORDER_TEST_H_

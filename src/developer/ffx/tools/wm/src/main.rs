// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[fuchsia_async::run_singlethreaded]
async fn main() {
    ffx_wm_suite::fho_suite_main().await
}

// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// DO NOT EDIT.
// Generated from FIDL library `zither.resources` by zither, a Fuchsia platform tool.

#![allow(unused_imports)]

use zerocopy::{FromBytes, IntoBytes};

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, IntoBytes, PartialEq)]
pub enum Subtype {
    A = 0,
    B = 1,
}

/// This is a handle.
pub type Handle = u32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, IntoBytes, PartialEq)]
pub struct StructWithHandleMembers {
    pub untyped_handle: Handle,
    pub handle_a: Handle,
    pub handle_b: Handle,
}

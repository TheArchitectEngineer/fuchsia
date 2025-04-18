// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Generated by garnet/public/rust/fuchsia-bootfs/scripts/bindgen.sh
// Manually modified to remove unused constants and structs.

/* automatically generated by rust-bindgen */
use zerocopy::byteorder::little_endian::U32;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

pub const ZBI_BOOTFS_PAGE_SIZE: u32 = 4096;
pub const ZBI_BOOTFS_MAGIC: u32 = 2775400441;
pub const ZBI_BOOTFS_MAX_NAME_LEN: u32 = 256;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, KnownLayout, FromBytes, Immutable, Unaligned)]
pub struct zbi_bootfs_header_t {
    pub magic: U32,
    pub dirsize: U32,
    pub reserved0: U32,
    pub reserved1: U32,
}
#[repr(C)]
#[derive(Debug, Default, KnownLayout, FromBytes, Immutable, Unaligned)]
pub struct zbi_bootfs_dirent_t {
    pub name_len: U32,
    pub data_len: U32,
    pub data_off: U32,
}

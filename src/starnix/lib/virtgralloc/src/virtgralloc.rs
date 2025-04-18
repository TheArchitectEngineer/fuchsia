/* automatically generated by rust-bindgen 0.69.1 */

// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zerocopy::{FromBytes, IntoBytes, KnownLayout};

pub const VIRTGRALLOC_IOCTL_BASE: u8 = 103u8;
pub const VIRTGRALLOC_DEVICE_NAME: &[u8; 18] = b"/dev/virtgralloc0\0";

pub type virtgralloc_VulkanMode = u64;
pub type virtgralloc_SetVulkanModeResult = u64;
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, IntoBytes, KnownLayout, FromBytes, zerocopy::Immutable)]
pub struct virtgralloc_set_vulkan_mode {
    pub vulkan_mode: virtgralloc_VulkanMode,
    pub result: virtgralloc_SetVulkanModeResult,
}
pub const VIRTGRALLOC_VULKAN_MODE_INVALID: virtgralloc_VulkanMode = 0;
pub const VIRTGRALLOC_VULKAN_MODE_SWIFTSHADER: virtgralloc_VulkanMode = 1;
pub const VIRTGRALLOC_VULKAN_MODE_MAGMA: virtgralloc_VulkanMode = 2;
pub const VIRTGRALLOC_SET_VULKAN_MODE_RESULT_INVALID: virtgralloc_SetVulkanModeResult = 0;
pub const VIRTGRALLOC_SET_VULKAN_MODE_RESULT_SUCCESS: virtgralloc_SetVulkanModeResult = 1;
pub const VIRTGRALLOC_IOCTL_SET_VULKAN_MODE: u32 = 3222300417;

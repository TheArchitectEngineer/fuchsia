// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// -- Partition labels --
pub const BLOBFS_PARTITION_LABEL: &str = "blobfs";
pub const DATA_PARTITION_LABEL: &str = "data";
pub const LEGACY_DATA_PARTITION_LABEL: &str = "minfs";

// -- Fxfs volume labels --
pub const BLOB_VOLUME_LABEL: &str = "blob";
pub const DATA_VOLUME_LABEL: &str = "data";
pub const UNENCRYPTED_VOLUME_LABEL: &str = "unencrypted";

// -- Partition type GUIDs --
pub const BLOBFS_TYPE_GUID: [u8; 16] = [
    0x0e, 0x38, 0x67, 0x29, 0x4c, 0x13, 0xbb, 0x4c, 0xb6, 0xda, 0x17, 0xe7, 0xce, 0x1c, 0xa4, 0x5d,
];
pub const DATA_TYPE_GUID: [u8; 16] = [
    0x0c, 0x5f, 0x18, 0x08, 0x2d, 0x89, 0x8a, 0x42, 0xa7, 0x89, 0xdb, 0xee, 0xc8, 0xf5, 0x5e, 0x6a,
];

// -- Driver paths (to be used to attach devices)
pub const FVM_DRIVER_PATH: &str = "fvm.cm";
pub const GPT_DRIVER_PATH: &str = "gpt.cm";
pub const MBR_DRIVER_PATH: &str = "mbr.cm";
pub const BOOTPART_DRIVER_PATH: &str = "bootpart.cm";
// pub const BLOCK_VERITY_DRIVER_PATH: &str = "block-verity.cm";
pub const NAND_BROKER_DRIVER_PATH: &str = "nand-broker.cm";
pub const ZXCRYPT_DRIVER_PATH: &str = "zxcrypt.cm";

pub const DEFAULT_F2FS_MIN_BYTES: u64 = 50 * 1024 * 1024;

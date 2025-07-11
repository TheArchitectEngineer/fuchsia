// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Functionality for extracting a ramdisk image from the zircon boot items. This ramdisk contains
//! an fvm with blobfs and data volumes, and is intended to be used in conjunction with the
//! ramdisk_image option, in run modes where we need to operate on the real disk and can't run
//! filesystems off it, such as recovery.

use crate::device::{BlockDevice, Device, LocalBlockDevice};
use anyhow::{ensure, Context, Error};
use fuchsia_component::client::connect_to_protocol;
use vmo_backed_block_server::{InitialContents, VmoBackedServerOptions};

use zerocopy::{FromBytes, Immutable, KnownLayout};

/// The following types and constants are defined in sdk/lib/zbi-format/include/lib/zbi-format/zbi.h.
const ZBI_TYPE_STORAGE_RAMDISK: u32 = 0x4b534452;
const ZBI_FLAGS_VERSION: u32 = 0x00010000;
const ZBI_ITEM_MAGIC: u32 = 0xb5781729;
const ZBI_FLAGS_STORAGE_COMPRESSED: u32 = 0x00000001;

#[repr(C)]
#[derive(KnownLayout, FromBytes, Immutable)]
struct ZbiHeader {
    type_: u32,
    length: u32,
    extra: u32,
    flags: u32,
    _reserved0: u32,
    _reserved1: u32,
    magic: u32,
    _crc32: u32,
}

async fn create_ramdisk(zbi_vmo: zx::Vmo, storage_host: bool) -> Result<Box<dyn Device>, Error> {
    let mut header_buf = [0u8; std::mem::size_of::<ZbiHeader>()];
    zbi_vmo.read(&mut header_buf, 0).context("reading zbi item header")?;
    // Expect is fine here - we made the buffer ourselves to the exact size of the header so
    // something is very wrong if we trip this.
    let header =
        ZbiHeader::read_from_bytes(header_buf.as_slice()).expect("buffer was the wrong size");

    ensure!(
        header.flags & ZBI_FLAGS_VERSION != 0,
        "invalid ZBI_TYPE_STORAGE_RAMDISK item header: flags"
    );
    ensure!(header.magic == ZBI_ITEM_MAGIC, "invalid ZBI_TYPE_STORAGE_RAMDISK item header: magic");
    ensure!(
        header.type_ == ZBI_TYPE_STORAGE_RAMDISK,
        "invalid ZBI_TYPE_STORAGE_RAMDISK item header: type"
    );

    // TODO(https://fxbug.dev/42109921): The old code ignored uncompressed items too, and silently.  Really
    // the protocol should be cleaned up so the VMO arrives without the header in it and then it
    // could just be used here directly if uncompressed (or maybe bootsvc deals with decompression
    // in the first place so the uncompressed VMO is always what we get).
    ensure!(
        header.flags & ZBI_FLAGS_STORAGE_COMPRESSED != 0,
        "ignoring uncompressed RAMDISK item in ZBI"
    );

    let ramdisk_vmo = zx::Vmo::create(header.extra as u64).context("making output vmo")?;
    let mut compressed_buf = vec![0u8; header.length as usize];
    zbi_vmo
        .read(&mut compressed_buf, std::mem::size_of::<ZbiHeader>() as u64)
        .context("reading compressed ramdisk")?;
    let decompressed_buf =
        zstd::decode_all(compressed_buf.as_slice()).context("zstd decompression failed")?;
    ramdisk_vmo.write(&decompressed_buf, 0).context("writing decompressed contents to vmo")?;

    if storage_host {
        let server = VmoBackedServerOptions {
            initial_contents: InitialContents::FromVmo(ramdisk_vmo),
            ..Default::default()
        }
        .build()
        .context("building ramdisk from vmo")?;

        Ok(Box::new(LocalBlockDevice::new(server).await?))
    } else {
        let ramdisk = ramdevice_client::RamdiskClientBuilder::new_with_vmo(ramdisk_vmo, None)
            .build()
            .await
            .context("building ramdisk from vmo")?;

        let topological_path = ramdisk
            .as_controller()
            .ok_or_else(|| anyhow::anyhow!("ramdisk instance missing controller"))?
            .get_topological_path()
            .await
            .context("get_topological_path (fidl failure)")?
            .map_err(zx::Status::from_raw)
            .context("get_topological_path returned an error")?;

        log::info!(topological_path:%; "launched ramdisk filesystem");

        // Ensure the boot image remains attached for the system lifetime.
        ramdisk.forget().context("detaching/forgetting ramdisk client")?;

        let mut device = BlockDevice::new(topological_path).await?;
        device.set_fshost_ramdisk(true);
        Ok(Box::new(device))
    }
}

/// Set up a ramdisk provided by the boot items service as a vmo. If there is no vmo provided, None
/// is returned. If there is, the ramdisk is decoded and set up, and the topological path is
/// returned.
pub async fn set_up_ramdisk(storage_host: bool) -> Result<Option<Box<dyn Device>>, Error> {
    let proxy = connect_to_protocol::<fidl_fuchsia_boot::ItemsMarker>()?;
    let (maybe_vmo, _length) = proxy
        .get(ZBI_TYPE_STORAGE_RAMDISK, 0)
        .await
        .context("boot items get failed (fidl failure)")?;

    if let Some(vmo) = maybe_vmo {
        Ok(Some(create_ramdisk(vmo, storage_host).await?))
    } else {
        Ok(None)
    }
}

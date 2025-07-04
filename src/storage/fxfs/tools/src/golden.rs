// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ops;
use anyhow::{bail, ensure, Context, Error};
use chrono::Local;
use fxfs::filesystem::{mkfs_with_volume, FxFilesystem, OpenFxFilesystem, SyncOptions};
use fxfs::object_store::{ObjectStore, NO_OWNER};
use fxfs::serialized_types::{Version, LATEST_VERSION};
use fxfs_crypto::Crypt;
use fxfs_insecure_crypto::InsecureCrypt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use storage_device::fake_device::FakeDevice;
use storage_device::DeviceHolder;

const IMAGE_BLOCKS: u64 = 8192;
// Version of first image with a verified file included in `create_image()`
const FSCRYPT_VERSION_START: u32 = 47;
const IMAGE_BLOCK_SIZE: u32 = 1024;
const EXPECTED_FILE_CONTENT: &[u8; 8] = b"content.";
const FXFS_GOLDEN_IMAGE_DIR: &str = "src/storage/fxfs/testdata";
const FXFS_GOLDEN_IMAGE_MANIFEST: &str = "golden_image_manifest.json";
const PROJECT_ID: u64 = 4;
const DEFAULT_VOLUME: &str = "default";
const UNENCRYPTED_VOLUME: &str = "unencrypted";
const CHECK_FILE_PATH: &str = "some/test_file.txt";
const CHECK_FILE_CONTENT: &[u8; 6] = &[0, 1, 2, 3, 4, 5];
const SECOND_VOLUME_VERSION: Version = Version { major: 38, minor: 0 };

/// Uses FUCHSIA_DIR environment variable to generate a path to the expected location of golden
/// images. Note that we do this largely for ergonomics because this binary is typically invoked
/// by running `fx fxfs create_golden` from an arbitrary directory.
fn golden_image_dir() -> Result<PathBuf, Error> {
    let fuchsia_dir = std::env::vars().find(|(k, _)| k == "FUCHSIA_DIR");
    if fuchsia_dir.is_none() {
        bail!("FUCHSIA_DIR environment variable is not set.");
    }
    let (_, fuchsia_dir) = fuchsia_dir.unwrap();
    Ok(PathBuf::from(fuchsia_dir).join(FXFS_GOLDEN_IMAGE_DIR))
}

/// Generates the filename where we expect to find a golden image for the current version of the
/// filesystem.
fn latest_image_filename() -> String {
    format!("fxfs_golden.{}.{}.img.zstd", LATEST_VERSION.major, LATEST_VERSION.minor)
}

/// Decompresses a zstd compressed local image into a RAM backed FakeDevice.
fn load_device(path: &Path) -> Result<FakeDevice, Error> {
    Ok(FakeDevice::from_image(zstd::Decoder::new(std::fs::File::open(path)?)?, IMAGE_BLOCK_SIZE)?)
}

/// Compresses contents of a device into a zstd compressed local image.
async fn save_device(device: DeviceHolder, path: &Path) -> Result<(), Error> {
    device.reopen(true);
    let mut writer = zstd::Encoder::new(std::fs::File::create(path)?, 6)?;
    let mut buf = device.allocate_buffer(device.block_size() as usize).await;
    let mut offset: u64 = 0;
    while offset < IMAGE_BLOCKS * IMAGE_BLOCK_SIZE as u64 {
        device.read(offset, buf.as_mut()).await?;
        writer.write_all(buf.as_ref().as_slice())?;
        offset += device.block_size() as u64;
    }
    writer.finish()?;
    Ok(())
}

async fn activity_in_volume(fs: &OpenFxFilesystem, vol: &Arc<ObjectStore>) -> Result<(), Error> {
    ops::mkdir(fs, vol, Path::new("some")).await?;
    // Apply limit to project id and apply that both to the "some" directory to have it get applied
    // everywhere else.
    ops::set_project_limit(vol, PROJECT_ID, 102400, 1024).await?;
    ops::set_project_for_node(vol, PROJECT_ID, &Path::new("some")).await?;

    ops::put(fs, vol, &Path::new("some/file.txt"), EXPECTED_FILE_CONTENT.to_vec()).await?;
    ops::put(fs, vol, &Path::new("some/deleted.txt"), EXPECTED_FILE_CONTENT.to_vec()).await?;
    // Compact here and below so that there are some persistent files added.
    fs.journal().compact().await?;
    ops::unlink(fs, vol, &Path::new("some/deleted.txt")).await?;
    ops::put(fs, vol, &Path::new("some/fsverity.txt"), EXPECTED_FILE_CONTENT.to_vec()).await?;
    ops::enable_verity(vol, &Path::new("some/fsverity.txt")).await?;

    fs.journal().compact().await?;

    ops::set_extended_attribute_for_node(
        vol,
        &Path::new("some"),
        b"security.selinux",
        b"test value",
    )
    .await?;
    ops::set_extended_attribute_for_node(
        vol,
        &Path::new("some/file.txt"),
        b"user.hash",
        b"different value",
    )
    .await?;

    // Exercise fscrypt and casefold with unicode filenames.
    if vol.crypt().is_some() {
        ops::mkdir(fs, vol, &Path::new("/fscrypt")).await?;
        ops::enable_fscrypt(fs, vol, &Path::new("/fscrypt"), 2).await?;
        ops::enable_casefold(vol, &Path::new("/fscrypt")).await?;
        ops::put(fs, vol, &Path::new("/fscrypt/Straße.txt"), EXPECTED_FILE_CONTENT.to_vec())
            .await?;
    }

    Ok(())
}

/// Create a new golden image (at the current version).
pub async fn create_image() -> Result<(), Error> {
    let path = golden_image_dir()?.join(latest_image_filename());

    let insecure_crypt = InsecureCrypt::new();
    insecure_crypt.add_wrapping_key(2, [1; 32].into());
    let crypt: Arc<dyn Crypt> = Arc::new(insecure_crypt);
    {
        let device = mkfs_with_volume(
            DeviceHolder::new(FakeDevice::new(IMAGE_BLOCKS, IMAGE_BLOCK_SIZE)),
            DEFAULT_VOLUME,
            Some(crypt.clone()),
        )
        .await?;
        save_device(device, path.as_path()).await?;
    }
    let device = DeviceHolder::new(load_device(&path)?);
    let fs = FxFilesystem::open(device).await?;
    let default_vol = ops::open_volume(&fs, DEFAULT_VOLUME, NO_OWNER, Some(crypt.clone())).await?;
    let unencrypted_vol = ops::create_volume(&fs, UNENCRYPTED_VOLUME, None).await?;
    for (vol, msg) in [(&default_vol, "default volume"), (&unencrypted_vol, "unencrypted volume")] {
        activity_in_volume(&fs, vol).await.context(msg)?;
    }

    // Write enough stuff to the journal (journal::BLOCK_SIZE per sync) to ensure we would fill
    // the disk without reclaim of both journal and file data.
    let num_iters = 2000;
    let before_generation = fs.super_block_header().generation;
    for _i in 0..num_iters {
        ops::put(&fs, &default_vol, &Path::new("some/repeat.txt"), EXPECTED_FILE_CONTENT.to_vec())
            .await?;
        fs.sync(SyncOptions { flush_device: true, precondition: None }).await?;
        ops::unlink(&fs, &default_vol, &Path::new("some/repeat.txt")).await?;
        fs.sync(SyncOptions { flush_device: true, precondition: None }).await?;
    }

    // Ensure that we have reclaimed the journal at least once.
    assert_ne!(before_generation, fs.super_block_header().generation);
    fs.close().await?;
    let device = fs.take_device().await;
    save_device(device, &path).await?;

    let mut file = std::fs::File::create(golden_image_dir()?.join("images.gni").as_path())?;
    file.write_all(
        format!(
            "# Copyright {} The Fuchsia Authors. All rights reserved.\n\
             # Use of this source code is governed by a BSD-style license that can be\n\
             # found in the LICENSE file.\n\
             # Auto-generated by `fx fxfs create_golden` on {}\n",
            Local::now().format("%Y"),
            Local::now()
        )
        .as_bytes(),
    )?;
    file.write_all(b"fxfs_golden_images = [\n")?;
    let mut paths = std::fs::read_dir(golden_image_dir()?)?.collect::<Result<Vec<_>, _>>()?;
    paths.sort_unstable_by_key(|path| path.path().to_str().unwrap().to_string());
    for file_name in
        paths.iter().map(|e| e.file_name()).filter(|x| x.to_str().unwrap().ends_with(".zstd"))
    {
        file.write_all(format!("  \"{}\",\n", file_name.to_str().unwrap()).as_bytes())?;
    }
    file.write_all(b"]\n")?;
    Ok(())
}

async fn check_volume(
    fs: &OpenFxFilesystem,
    vol: &Arc<ObjectStore>,
    version: &Version,
) -> Result<(), Error> {
    if ops::get(&vol, &Path::new("some/file.txt")).await? != EXPECTED_FILE_CONTENT.to_vec() {
        bail!("Expected file content incorrect.");
    }
    if ops::get(&vol, &Path::new("some/fsverity.txt")).await? != EXPECTED_FILE_CONTENT.to_vec() {
        bail!("Expected fsverity content incorrect.");
    }
    if ops::get(&vol, &Path::new("some/deleted.txt")).await.is_ok() {
        bail!("Found deleted file.");
    }

    if version.major >= FSCRYPT_VERSION_START && vol.crypt().is_some() {
        // Check fscrypt file read with a casefolded unicode filename.
        assert_eq!(
            ops::get(&vol, &Path::new("/fscrypt/Strasse.txt")).await?,
            EXPECTED_FILE_CONTENT
        );
    }

    // Make sure after writing a new file (using the latest format), the filesystem remains
    // valid.
    let mut content = vec![0u8; 6];
    content.copy_from_slice(CHECK_FILE_CONTENT);
    ops::put(&fs, &vol, &Path::new(CHECK_FILE_PATH), content).await?;
    Ok(())
}

/// Validates an image by looking for expected data and performing an fsck.
async fn check_image(path: &Path) -> Result<(), Error> {
    let insecure_crypt = InsecureCrypt::new();
    insecure_crypt.add_wrapping_key(2, [1; 32].into());
    let crypt: Arc<dyn Crypt> = Arc::new(insecure_crypt);
    let version = {
        let device = DeviceHolder::new(load_device(path)?);
        let fs = FxFilesystem::open(device).await?;
        ops::fsck(&fs, true).await.context("fsck failed")?;
        fs.journal().super_block_header().earliest_version
    };

    let device = DeviceHolder::new(load_device(path)?);
    let fs = FxFilesystem::open(device).await?;
    let mut volumes = vec![(DEFAULT_VOLUME, Some(crypt.clone()))];
    if version >= SECOND_VOLUME_VERSION {
        volumes.push((UNENCRYPTED_VOLUME, None));
    }
    for (vol_name, vol_crypt) in volumes.clone() {
        let vol = ops::open_volume(&fs, vol_name, NO_OWNER, vol_crypt)
            .await
            .context(format!("Opening {}", vol_name))?;
        check_volume(&fs, &vol, &version).await.context(format!("Checking {}", vol_name))?;
    }
    fs.close().await?;

    let device = fs.take_device().await;
    device.reopen(false);
    let fs = FxFilesystem::open(device).await?;
    ops::fsck(&fs, true).await.context("fsck failed")?;
    for (vol_name, vol_crypt) in volumes {
        let vol = ops::open_volume(&fs, vol_name, NO_OWNER, vol_crypt).await.context(vol_name)?;
        if &ops::get(&vol, &Path::new(CHECK_FILE_PATH)).await?.as_slice()
            != &CHECK_FILE_CONTENT.as_slice()
        {
            bail!("Unexpected file content in new file");
        }
    }

    fs.journal().compact().await?;
    assert_eq!(fs.journal().super_block_header().earliest_version, LATEST_VERSION);
    fs.close().await
}

pub async fn check_images(image_root: Option<String>) -> Result<(), Error> {
    let image_root = match image_root {
        Some(path) => std::env::current_exe()?.parent().unwrap().join(path),
        None => golden_image_dir()?,
    };

    let mut golden_files: Vec<String> = {
        let manifest_path = image_root.clone().join(FXFS_GOLDEN_IMAGE_MANIFEST);
        let manifest_contents =
            std::fs::read_to_string(manifest_path.clone()).with_context(|| {
                format!("Failed to read golden manifest: {}", manifest_path.display())
            })?;
        serde_json::from_str(&manifest_contents).with_context(|| {
            format!("Failed to parse manifest json: {}", manifest_path.display())
        })?
    };

    // First check that there exists an image for the latest version.
    ensure!(
        golden_files.contains(&latest_image_filename()),
        "Golden image is missing for version {} ({}). Please run 'fx fxfs create_golden'",
        LATEST_VERSION,
        latest_image_filename()
    );

    // Next ensure that we can parse all golden images and validate expected content.
    golden_files.sort();
    for golden_file in golden_files {
        let path_buf = image_root.clone().join(golden_file);
        println!("------------------------------------------------------------------------");
        println!("Validating golden image: {}", path_buf.file_name().unwrap().to_str().unwrap());
        println!("------------------------------------------------------------------------");
        if let Err(e) = check_image(path_buf.as_path()).await {
            bail!(
                "Failed to validate golden image {} with the latest code: {:?}",
                path_buf.display(),
                e
            )
        }
    }
    Ok(())
}

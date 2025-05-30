// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use async_trait::async_trait;
use errors::{ffx_bail, ffx_error};
use ffx_profile_heapdump_common::{
    build_process_selector, check_snapshot_error, connect_to_collector, export_to_pprof,
};
use ffx_profile_heapdump_snapshot_args::SnapshotCommand;
use ffx_writer::SimpleWriter;
use fho::{AvailabilityFlag, FfxMain, FfxTool};
use fidl::endpoints::create_request_stream;
use fidl_fuchsia_memory_heapdump_client as fheapdump_client;
use std::io::Write;
use target_holders::RemoteControlProxyHolder;

#[derive(FfxTool)]
#[check(AvailabilityFlag("ffx_profile_heapdump"))]
pub struct SnapshotTool {
    #[command]
    cmd: SnapshotCommand,
    remote_control: RemoteControlProxyHolder,
}

fho::embedded_plugin!(SnapshotTool);

#[async_trait(?Send)]
impl FfxMain for SnapshotTool {
    type Writer = SimpleWriter;

    async fn main(self, _writer: Self::Writer) -> fho::Result<()> {
        snapshot(self.remote_control, self.cmd).await?;
        Ok(())
    }
}

async fn snapshot(remote_control: RemoteControlProxyHolder, cmd: SnapshotCommand) -> Result<()> {
    let process_selector = build_process_selector(cmd.by_name, cmd.by_koid)?;
    let contents_dir = cmd.output_contents_dir.as_ref().map(std::path::Path::new);

    let (receiver_client, receiver_stream) = create_request_stream();
    let request = fheapdump_client::CollectorTakeLiveSnapshotRequest {
        process_selector: Some(process_selector),
        receiver: Some(receiver_client),
        with_contents: Some(contents_dir.is_some()),
        ..Default::default()
    };

    let collector = connect_to_collector(&remote_control, cmd.collector).await?;
    collector.take_live_snapshot(request)?;
    let snapshot =
        check_snapshot_error(heapdump_snapshot::Snapshot::receive_from(receiver_stream).await)?;

    // If the user has requested the blocks' contents, ensure that `contents_dir` is an empty
    // directory (creating it if necessary), then dump the contents of each allocated block to a
    // different file.
    if let Some(contents_dir) = contents_dir {
        if let Ok(mut iterator) = std::fs::read_dir(contents_dir) {
            // While not strictly necessary, requiring that the target directory is empty makes it
            // much harder to accidentally flood important directories.
            if iterator.next().is_some() {
                ffx_bail!("Output directory is not empty: {}", contents_dir.display());
            }
        } else {
            if let Err(err) = std::fs::create_dir(contents_dir) {
                ffx_bail!("Failed to create output directory: {}: {}", contents_dir.display(), err);
            }
        }

        for (address, info) in &snapshot.allocations {
            if let Some(ref data) = info.contents {
                let path = contents_dir.join(format!("0x{:x}", address));
                match std::fs::File::create(&path) {
                    Ok(mut file) => file.write_all(&data)?,
                    Err(err) => {
                        ffx_bail!("Failed to create output file: {}: {}", path.display(), err)
                    }
                };
            }
        }
    }

    // Always emit full metadata if the user requested the blocks' contents, as it serves as an
    // index for the generated files.
    let with_tags = cmd.with_tags || contents_dir.is_some();
    export_to_pprof(
        &snapshot,
        &mut std::fs::File::create(&cmd.output_file).map_err(|err| {
            ffx_error!("Failed to create output file: {}: {}", cmd.output_file, err)
        })?,
        with_tags,
        cmd.symbolize,
    )?;

    Ok(())
}

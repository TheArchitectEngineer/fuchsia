// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use blackout_target::{find_partition, set_up_partition};
use device_watcher::recursive_wait;
use fuchsia_async as fasync;
use ramdevice_client::RamdiskClient;
use storage_isolated_driver_manager::fvm;
use uuid::Uuid;

#[fasync::run_singlethreaded(test)]
async fn setup_then_find_no_path() {
    let ramdisk = RamdiskClient::create(8192, 128).await.unwrap();
    {
        let volume_manager_proxy = fvm::set_up_fvm(
            ramdisk.as_controller().expect("invalid controller"),
            ramdisk.as_dir().expect("invalid directory proxy"),
            8192,
        )
        .await
        .expect("failed to set up fvm");
        fvm::create_fvm_volume(
            &volume_manager_proxy,
            "blobfs",
            Uuid::new_v4().as_bytes(),
            Uuid::new_v4().as_bytes(),
            None,
            0,
        )
        .await
        .expect("failed to create fvm volume");
        recursive_wait(ramdisk.as_dir().expect("invalid directory proxy"), "/fvm/blobfs-p-1/block")
            .await
            .expect("failed to wait for device");
    }

    let setup_controller =
        set_up_partition("test-label", None, false).await.expect("failed to set up device");
    let setup_topo_path = setup_controller
        .get_topological_path()
        .await
        .expect("transport error on get_topological_path")
        .expect("failed to get topological path");

    let find_controller = find_partition("test-label", None).await.expect("failed to find device");
    let find_topo_path = find_controller
        .get_topological_path()
        .await
        .expect("transport error on get_topological_path")
        .expect("failed to get topological path");

    assert_eq!(setup_topo_path, find_topo_path);
}

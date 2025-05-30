#!/usr/bin/env fuchsia-vendored-python
# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import io
import json
import os
import sys
import tarfile
from typing import Any


class GatherPackageDeps:
    """Helper class to take a `package_manifest.json` and copy all files referenced
    into an archive that will then be available at runtime.

    Args:
      package_json (string): Path to the package's `package_manifest.json` file.
      meta_far (string): Path to the package's `meta.far` file.
      depfile (string): Path to the depfile to write to.

    Raises: ValueError if any parameter is empty.
    """

    def __init__(
        self,
        package_json: str | None,
        meta_far: str | None,
        output_tar: str | None,
        depfile: str | None,
    ) -> None:
        if package_json and os.path.exists(package_json):
            self.package_json = package_json
        else:
            raise ValueError("package_json must be to a valid file")

        if meta_far and os.path.exists(meta_far):
            self.meta_far = meta_far
        else:
            raise ValueError("meta_far must be to a valid file")

        if output_tar:
            self.output_tar = output_tar
        else:
            raise ValueError("output_tar cannot be empty")

        if depfile:
            self.depfile = depfile
        else:
            raise ValueError("depfile cannot be empty")

    def parse_package_json(
        self,
    ) -> tuple[list[tuple[str, str]], dict[str, Any]]:
        manifest_paths = []
        with open(self.package_json) as f:
            data = json.load(f)
            for file in data["blobs"]:
                if file["path"].startswith("meta/"):
                    file["source_path"] = "meta.far"
                    continue
                manifest_paths.append((file["path"], file["source_path"]))
                # Update the source path to be relative path inside the tar.
                file["source_path"] = file["path"]
        return manifest_paths, data

    def create_archive(
        self,
        manifest_paths: list[tuple[str, str]],
        package_json_data: dict[str, Any],
    ) -> None:
        # Explicitly use the GNU_FORMAT because the current dart library
        # (v.3.0.0) does not support parsing other tar formats that allow for
        # filenames longer than 100 characters.
        with tarfile.open(
            self.output_tar, "w", format=tarfile.GNU_FORMAT
        ) as tar:
            # Follow symlinks
            tar.dereference = True
            # Add all source files to archive.
            for archive_path, source_path in manifest_paths:
                tar.add(source_path, arcname=archive_path)

            # Add meta.far and package_manifest.json to archive.
            tar.add(self.meta_far, arcname="meta.far")
            with io.BytesIO(json.dumps(package_json_data).encode()) as manifest:
                tarinfo = tarfile.TarInfo("package_manifest.json")
                tarinfo.size = len(manifest.getvalue())
                tar.addfile(tarinfo, fileobj=manifest)

    def run(self) -> None:
        manifest_paths, package_json_data = self.parse_package_json()
        self.create_archive(manifest_paths, package_json_data)
        with open(self.depfile, "w") as f:
            f.write(
                "{}: {}\n".format(
                    self.output_tar,
                    " ".join(
                        os.path.relpath(source_path)
                        for (_, source_path) in manifest_paths
                    ),
                )
            )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--package_json",
        required=True,
        help="The path to the package_manifest.json generated by a `fuchsia_package`.",
    )
    parser.add_argument(
        "--meta_far", required=True, help="The path to the package's meta.far."
    )
    parser.add_argument(
        "--output_tar", required=True, help="The path to the output archive."
    )
    parser.add_argument(
        "--depfile",
        required=True,
        help="The path to write a depfile, see depfile from GN.",
    )
    args = parser.parse_args()

    GatherPackageDeps(
        args.package_json, args.meta_far, args.output_tar, args.depfile
    ).run()

    return 0


if __name__ == "__main__":
    sys.exit(main())

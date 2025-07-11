# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Python Types for Assembly Configuration Files

This module contains Python classes for working with files that have the same
schema as `//src/developer/ffx/plugins/assembly`.
"""

from dataclasses import dataclass, field
from typing import Optional, TypeVar

import serialization

__all__ = ["ImageAssemblyConfig", "KernelInfo"]

from .common import FileEntry, FilePath

ExtendsImageAssemblyConfig = TypeVar(
    "ExtendsImageAssemblyConfig", bound="ImageAssemblyConfig"
)


@dataclass
class ReleaseInfo:
    name: str
    repository: str
    version: str


@dataclass
class ProductReleaseInfo:
    info: ReleaseInfo
    pibs: list[ReleaseInfo]


@dataclass
class BoardReleaseInfo:
    info: ReleaseInfo
    bib_sets: list[ReleaseInfo]


@dataclass
class SystemReleaseInfo:
    platform: Optional[ReleaseInfo]
    product: Optional[ProductReleaseInfo]
    board: Optional[BoardReleaseInfo]


@dataclass
class KernelInfo:
    """Information about the kernel"""

    path: Optional[FilePath] = None
    args: set[str] = field(default_factory=set)


@dataclass
class BoardDriverArguments:
    vendor_id: int
    product_id: int
    name: str
    revision: int


@dataclass
class ImageAssemblyConfig:
    """The input configuration for the Image Assembly Operation

    This describes all the packages, bootfs files, kernel args, kernel, etc.
    that are to be combined into a complete set of assembled product images.
    """

    base: set[FilePath] = field(default_factory=set)
    cache: set[FilePath] = field(default_factory=set)
    on_demand: set[FilePath] = field(default_factory=set)
    system: set[FilePath] = field(default_factory=set)
    kernel: KernelInfo = field(default_factory=KernelInfo)
    qemu_kernel: Optional[FilePath] = None
    boot_args: set[str] = field(default_factory=set)
    bootfs_files: set[FileEntry] = field(default_factory=set)
    bootfs_packages: set[FilePath] = field(default_factory=set)
    board_driver_arguments: Optional[BoardDriverArguments] = None
    devicetree: Optional[FilePath] = None
    devicetree_overlay: Optional[FilePath] = None
    netboot_mode: bool = False
    board_name: None | str = None
    image_mode: None | str = None
    system_release_info: Optional[SystemReleaseInfo] = None
    partitions_config: Optional[FilePath] = None

    # TODO:  Flesh out the images_config with the actual types, if it's needed.
    images_config: dict[str, list[str]] = field(default_factory=dict)

    def __repr__(self) -> str:
        """Serialize to a JSON string"""
        return serialization.json_dumps(self, indent=2)

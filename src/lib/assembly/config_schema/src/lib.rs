// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod assembly_config;

/// Configuration that's provided to Assembly by the Board
pub mod board_config;
mod board_input_bundle_set;

pub mod common;
pub mod developer_overrides;
pub mod platform_config;
pub mod product_config;

pub use assembly_config::AssemblyConfig;
pub use board_config::{Architecture, BoardInformation, BoardInputBundle, BoardProvidedConfig};
pub use board_input_bundle_set::{BoardInputBundleEntry, BoardInputBundleSet};
pub use common::{
    DriverDetails, FeatureControl, PackageDetails, PackageSet, PackagedDriverDetails,
};
pub use platform_config::example_config::ExampleConfig;
pub use platform_config::icu_config::{ICUConfig, Revision};
pub use platform_config::intl_config::IntlConfig;
pub use platform_config::{BuildType, FeatureSetLevel};

use common::{option_path_schema, vec_path_schema};

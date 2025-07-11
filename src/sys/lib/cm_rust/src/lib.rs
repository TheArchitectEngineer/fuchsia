// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use cm_rust_derive::{
    ExposeDeclCommon, ExposeDeclCommonAlwaysRequired, FidlDecl, OfferDeclCommon,
    OfferDeclCommonNoAvailability, UseDeclCommon,
};
use cm_types::{AllowedOffers, BorrowedSeparatedPath, LongName, Name, Path, RelativePath, Url};
use from_enum::FromEnum;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::hash::Hash;
use strum_macros::EnumIter;
use thiserror::Error;
use {
    fidl_fuchsia_component_decl as fdecl, fidl_fuchsia_data as fdata, fidl_fuchsia_io as fio,
    fidl_fuchsia_process as fprocess, fidl_fuchsia_sys2 as fsys,
};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "serde")]
mod serde_ext;

/// Converts a fidl object into its corresponding native representation.
pub trait FidlIntoNative<T> {
    fn fidl_into_native(self) -> T;
}

impl<Native, Fidl> FidlIntoNative<Vec<Native>> for Vec<Fidl>
where
    Fidl: FidlIntoNative<Native>,
{
    fn fidl_into_native(self) -> Vec<Native> {
        self.into_iter().map(|s| s.fidl_into_native()).collect()
    }
}

pub trait NativeIntoFidl<T> {
    fn native_into_fidl(self) -> T;
}

impl<Native, Fidl> NativeIntoFidl<Vec<Fidl>> for Vec<Native>
where
    Native: NativeIntoFidl<Fidl>,
{
    fn native_into_fidl(self) -> Vec<Fidl> {
        self.into_iter().map(|s| s.native_into_fidl()).collect()
    }
}

impl FidlIntoNative<Name> for String {
    fn fidl_into_native(self) -> Name {
        // cm_fidl_validator should have already validated this
        self.parse().unwrap()
    }
}

impl NativeIntoFidl<String> for Name {
    fn native_into_fidl(self) -> String {
        self.to_string()
    }
}

impl FidlIntoNative<LongName> for String {
    fn fidl_into_native(self) -> LongName {
        // cm_fidl_validator should have already validated this
        self.parse().unwrap()
    }
}

impl NativeIntoFidl<String> for LongName {
    fn native_into_fidl(self) -> String {
        self.to_string()
    }
}

impl FidlIntoNative<Path> for String {
    fn fidl_into_native(self) -> Path {
        // cm_fidl_validator should have already validated this
        self.parse().unwrap()
    }
}

impl NativeIntoFidl<String> for Path {
    fn native_into_fidl(self) -> String {
        self.to_string()
    }
}

impl FidlIntoNative<RelativePath> for String {
    fn fidl_into_native(self) -> RelativePath {
        // cm_fidl_validator should have already validated this
        self.parse().unwrap()
    }
}

impl NativeIntoFidl<String> for RelativePath {
    fn native_into_fidl(self) -> String {
        self.to_string()
    }
}

impl NativeIntoFidl<Option<String>> for RelativePath {
    fn native_into_fidl(self) -> Option<String> {
        if self.is_dot() {
            None
        } else {
            Some(self.to_string())
        }
    }
}

impl FidlIntoNative<Url> for String {
    fn fidl_into_native(self) -> Url {
        // cm_fidl_validator should have already validated this
        self.parse().unwrap()
    }
}

impl NativeIntoFidl<String> for Url {
    fn native_into_fidl(self) -> String {
        self.to_string()
    }
}

/// Generates `FidlIntoNative` and `NativeIntoFidl` implementations that leaves the input unchanged.
macro_rules! fidl_translations_identical {
    ($into_type:ty) => {
        impl FidlIntoNative<$into_type> for $into_type {
            fn fidl_into_native(self) -> $into_type {
                self
            }
        }
        impl NativeIntoFidl<$into_type> for $into_type {
            fn native_into_fidl(self) -> Self {
                self
            }
        }
    };
}

/// Generates `FidlIntoNative` and `NativeIntoFidl` implementations that
/// delegate to existing `Into` implementations.
macro_rules! fidl_translations_from_into {
    ($native_type:ty, $fidl_type:ty) => {
        impl FidlIntoNative<$native_type> for $fidl_type {
            fn fidl_into_native(self) -> $native_type {
                self.into()
            }
        }
        impl NativeIntoFidl<$fidl_type> for $native_type {
            fn native_into_fidl(self) -> $fidl_type {
                self.into()
            }
        }
    };
}

/// Generates `FidlIntoNative` and `NativeIntoFidl` implementations for
/// an symmetrical enum types.
/// `fidl_type` should be the FIDL type while `native_type` should be
/// the Rust native type defined elsewhere in this file.
/// Each field of the enums must be provided in the `variant` fieldset.
macro_rules! fidl_translations_symmetrical_enums {
($fidl_type:ty , $native_type:ty, $($variant: ident),*) => {
        impl FidlIntoNative<$native_type> for $fidl_type {
            fn fidl_into_native(self) -> $native_type {
                match self {
                    $( <$fidl_type>::$variant => <$native_type>::$variant,  )*
                }
            }
        }
        impl NativeIntoFidl<$fidl_type> for $native_type {
            fn native_into_fidl(self) -> $fidl_type {
                match self {
                    $( <$native_type>::$variant => <$fidl_type>::$variant,  )*
                }
            }
        }
    };
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Default)]
#[fidl_decl(fidl_table = "fdecl::Component")]
pub struct ComponentDecl {
    pub program: Option<ProgramDecl>,
    pub uses: Vec<UseDecl>,
    pub exposes: Vec<ExposeDecl>,
    pub offers: Vec<OfferDecl>,
    pub capabilities: Vec<CapabilityDecl>,
    pub children: Vec<ChildDecl>,
    pub collections: Vec<CollectionDecl>,
    pub facets: Option<fdata::Dictionary>,
    pub environments: Vec<EnvironmentDecl>,
    pub config: Option<ConfigDecl>,
}

impl ComponentDecl {
    /// Returns the runner used by this component, or `None` if this is a non-executable component.
    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    pub fn get_runner(&self) -> Option<UseRunnerDecl> {
        self.program
            .as_ref()
            .and_then(|p| p.runner.as_ref())
            .map(|r| UseRunnerDecl {
                source: UseSource::Environment,
                source_name: r.clone(),
                source_dictionary: Default::default(),
            })
            .or_else(|| {
                self.uses.iter().find_map(|u| match u {
                    UseDecl::Runner(r) => Some(r.clone()),
                    _ => None,
                })
            })
    }

    /// Returns the `StorageDecl` corresponding to `storage_name`.
    pub fn find_storage_source<'a>(&'a self, storage_name: &Name) -> Option<&'a StorageDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Storage(s) if &s.name == storage_name => Some(s),
            _ => None,
        })
    }

    /// Returns the `ProtocolDecl` corresponding to `protocol_name`.
    pub fn find_protocol_source<'a>(&'a self, protocol_name: &Name) -> Option<&'a ProtocolDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Protocol(r) if &r.name == protocol_name => Some(r),
            _ => None,
        })
    }

    /// Returns the `DirectoryDecl` corresponding to `directory_name`.
    pub fn find_directory_source<'a>(&'a self, directory_name: &Name) -> Option<&'a DirectoryDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Directory(r) if &r.name == directory_name => Some(r),
            _ => None,
        })
    }

    /// Returns the `RunnerDecl` corresponding to `runner_name`.
    pub fn find_runner_source<'a>(&'a self, runner_name: &Name) -> Option<&'a RunnerDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Runner(r) if &r.name == runner_name => Some(r),
            _ => None,
        })
    }

    /// Returns the `ResolverDecl` corresponding to `resolver_name`.
    pub fn find_resolver_source<'a>(&'a self, resolver_name: &Name) -> Option<&'a ResolverDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Resolver(r) if &r.name == resolver_name => Some(r),
            _ => None,
        })
    }

    /// Returns the `CollectionDecl` corresponding to `collection_name`.
    pub fn find_collection<'a>(&'a self, collection_name: &str) -> Option<&'a CollectionDecl> {
        self.collections.iter().find(|c| c.name == collection_name)
    }

    /// Indicates whether the capability specified by `target_name` is exposed to the framework.
    pub fn is_protocol_exposed_to_framework(&self, in_target_name: &Name) -> bool {
        self.exposes.iter().any(|expose| match expose {
            ExposeDecl::Protocol(ExposeProtocolDecl { target, target_name, .. })
                if target == &ExposeTarget::Framework =>
            {
                target_name == in_target_name
            }
            _ => false,
        })
    }

    /// Indicates whether the capability specified by `source_name` is requested.
    pub fn uses_protocol(&self, source_name: &Name) -> bool {
        self.uses.iter().any(|use_decl| match use_decl {
            UseDecl::Protocol(ls) => &ls.source_name == source_name,
            _ => false,
        })
    }
}

pub use cm_types::Availability;

fidl_translations_symmetrical_enums!(
    fdecl::Availability,
    Availability,
    Required,
    Optional,
    SameAsTarget,
    Transitional
);

pub use cm_types::DeliveryType;

#[cfg(fuchsia_api_level_at_least = "HEAD")]
impl FidlIntoNative<DeliveryType> for fdecl::DeliveryType {
    fn fidl_into_native(self) -> DeliveryType {
        self.try_into().unwrap()
    }
}

#[cfg(fuchsia_api_level_at_least = "HEAD")]
impl NativeIntoFidl<fdecl::DeliveryType> for DeliveryType {
    fn native_into_fidl(self) -> fdecl::DeliveryType {
        self.into()
    }
}

pub trait SourcePath {
    fn source_path(&self) -> BorrowedSeparatedPath<'_>;
    fn is_from_dictionary(&self) -> bool {
        !self.source_path().dirname.is_dot()
    }
}

#[cfg_attr(
    feature = "serde",
    derive(Deserialize, Serialize),
    serde(tag = "type", rename_all = "snake_case")
)]
#[derive(FidlDecl, FromEnum, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_union = "fdecl::Use")]
pub enum UseDecl {
    Service(UseServiceDecl),
    Protocol(UseProtocolDecl),
    Directory(UseDirectoryDecl),
    Storage(UseStorageDecl),
    EventStream(UseEventStreamDecl),
    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    Runner(UseRunnerDecl),
    #[cfg(fuchsia_api_level_at_least = "20")]
    Config(UseConfigurationDecl),
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, UseDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::UseService", source_path = "dictionary")]
pub struct UseServiceDecl {
    pub source: UseSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target_path: Path,
    pub dependency_type: DependencyType,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, UseDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::UseProtocol", source_path = "dictionary")]
pub struct UseProtocolDecl {
    pub source: UseSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target_path: Path,
    pub dependency_type: DependencyType,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, UseDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::UseDirectory", source_path = "dictionary")]
pub struct UseDirectoryDecl {
    pub source: UseSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target_path: Path,

    #[cfg_attr(
        feature = "serde",
        serde(
            deserialize_with = "serde_ext::deserialize_fio_operations",
            serialize_with = "serde_ext::serialize_fio_operations"
        )
    )]
    pub rights: fio::Operations,

    #[fidl_decl(default_preserve_none)]
    pub subdir: RelativePath,
    pub dependency_type: DependencyType,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::UseStorage", source_path = "name_only")]
pub struct UseStorageDecl {
    pub source_name: Name,
    pub target_path: Path,
    #[fidl_decl(default)]
    pub availability: Availability,
}

impl SourceName for UseStorageDecl {
    fn source_name(&self) -> &Name {
        &self.source_name
    }
}

impl UseDeclCommon for UseStorageDecl {
    fn source(&self) -> &UseSource {
        &UseSource::Parent
    }

    fn availability(&self) -> &Availability {
        &self.availability
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, UseDeclCommon, Debug, Clone, PartialEq, Eq, Hash)]
#[fidl_decl(fidl_table = "fdecl::UseEventStream", source_path = "name_only")]
pub struct UseEventStreamDecl {
    pub source_name: Name,
    pub source: UseSource,
    pub scope: Option<Vec<EventScope>>,
    pub target_path: Path,
    pub filter: Option<BTreeMap<String, DictionaryValue>>,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg(fuchsia_api_level_at_least = "HEAD")]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::UseRunner", source_path = "dictionary")]
pub struct UseRunnerDecl {
    pub source: UseSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
}

#[cfg(fuchsia_api_level_at_least = "HEAD")]
impl SourceName for UseRunnerDecl {
    fn source_name(&self) -> &Name {
        &self.source_name
    }
}

#[cfg(fuchsia_api_level_at_least = "HEAD")]
impl UseDeclCommon for UseRunnerDecl {
    fn source(&self) -> &UseSource {
        &self.source
    }

    fn availability(&self) -> &Availability {
        &Availability::Required
    }
}

#[cfg(fuchsia_api_level_at_least = "20")]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, UseDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::UseConfiguration", source_path = "dictionary")]
pub struct UseConfigurationDecl {
    pub source: UseSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target_name: Name,
    #[fidl_decl(default)]
    pub availability: Availability,
    pub type_: ConfigValueType,
    pub default: Option<ConfigValue>,
}

#[cfg_attr(
    feature = "serde",
    derive(Deserialize, Serialize),
    serde(tag = "type", rename_all = "snake_case")
)]
#[derive(FidlDecl, FromEnum, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_union = "fdecl::Offer")]
pub enum OfferDecl {
    Service(OfferServiceDecl),
    Protocol(OfferProtocolDecl),
    Directory(OfferDirectoryDecl),
    Storage(OfferStorageDecl),
    Runner(OfferRunnerDecl),
    Resolver(OfferResolverDecl),
    EventStream(OfferEventStreamDecl),
    #[cfg(fuchsia_api_level_at_least = "25")]
    Dictionary(OfferDictionaryDecl),
    #[cfg(fuchsia_api_level_at_least = "20")]
    Config(OfferConfigurationDecl),
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::OfferEventStream", source_path = "name_only")]
pub struct OfferEventStreamDecl {
    pub source: OfferSource,
    pub scope: Option<Vec<EventScope>>,
    pub source_name: Name,
    pub target: OfferTarget,
    pub target_name: Name,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameMapping {
    pub source_name: Name,
    pub target_name: Name,
}

impl NativeIntoFidl<fdecl::NameMapping> for NameMapping {
    fn native_into_fidl(self) -> fdecl::NameMapping {
        fdecl::NameMapping {
            source_name: self.source_name.native_into_fidl(),
            target_name: self.target_name.native_into_fidl(),
        }
    }
}

impl FidlIntoNative<NameMapping> for fdecl::NameMapping {
    fn fidl_into_native(self) -> NameMapping {
        NameMapping {
            source_name: self.source_name.fidl_into_native(),
            target_name: self.target_name.fidl_into_native(),
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::OfferService", source_path = "dictionary")]
pub struct OfferServiceDecl {
    pub source: OfferSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: OfferTarget,
    pub target_name: Name,
    pub source_instance_filter: Option<Vec<Name>>,
    pub renamed_instances: Option<Vec<NameMapping>>,
    #[fidl_decl(default)]
    pub availability: Availability,
    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    #[fidl_decl(default)]
    pub dependency_type: DependencyType,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::OfferProtocol", source_path = "dictionary")]
pub struct OfferProtocolDecl {
    pub source: OfferSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: OfferTarget,
    pub target_name: Name,
    pub dependency_type: DependencyType,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::OfferDirectory", source_path = "dictionary")]
pub struct OfferDirectoryDecl {
    pub source: OfferSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: OfferTarget,
    pub target_name: Name,
    pub dependency_type: DependencyType,

    #[cfg_attr(
        feature = "serde",
        serde(
            deserialize_with = "serde_ext::deserialize_opt_fio_operations",
            serialize_with = "serde_ext::serialize_opt_fio_operations"
        )
    )]
    pub rights: Option<fio::Operations>,

    #[fidl_decl(default_preserve_none)]
    pub subdir: RelativePath,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::OfferStorage", source_path = "name_only")]
pub struct OfferStorageDecl {
    pub source: OfferSource,
    pub source_name: Name,
    pub target: OfferTarget,
    pub target_name: Name,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::OfferRunner", source_path = "dictionary")]
pub struct OfferRunnerDecl {
    pub source: OfferSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: OfferTarget,
    pub target_name: Name,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, OfferDeclCommonNoAvailability, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::OfferResolver", source_path = "dictionary")]
pub struct OfferResolverDecl {
    pub source: OfferSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: OfferTarget,
    pub target_name: Name,
}

#[cfg(fuchsia_api_level_at_least = "25")]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::OfferDictionary", source_path = "dictionary")]
pub struct OfferDictionaryDecl {
    pub source: OfferSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: OfferTarget,
    pub target_name: Name,
    pub dependency_type: DependencyType,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg(fuchsia_api_level_at_least = "20")]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::OfferConfiguration", source_path = "dictionary")]
pub struct OfferConfigurationDecl {
    pub source: OfferSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: OfferTarget,
    pub target_name: Name,
    #[fidl_decl(default)]
    pub availability: Availability,
}

impl SourceName for OfferDecl {
    fn source_name(&self) -> &Name {
        match &self {
            OfferDecl::Service(o) => o.source_name(),
            OfferDecl::Protocol(o) => o.source_name(),
            OfferDecl::Directory(o) => o.source_name(),
            OfferDecl::Storage(o) => o.source_name(),
            OfferDecl::Runner(o) => o.source_name(),
            OfferDecl::Resolver(o) => o.source_name(),
            OfferDecl::EventStream(o) => o.source_name(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            OfferDecl::Dictionary(o) => o.source_name(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            OfferDecl::Config(o) => o.source_name(),
        }
    }
}

impl SourcePath for OfferDecl {
    fn source_path(&self) -> BorrowedSeparatedPath<'_> {
        match &self {
            OfferDecl::Service(o) => o.source_path(),
            OfferDecl::Protocol(o) => o.source_path(),
            OfferDecl::Directory(o) => o.source_path(),
            OfferDecl::Storage(o) => o.source_path(),
            OfferDecl::Runner(o) => o.source_path(),
            OfferDecl::Resolver(o) => o.source_path(),
            OfferDecl::EventStream(o) => o.source_path(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            OfferDecl::Dictionary(o) => o.source_path(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            OfferDecl::Config(o) => o.source_path(),
        }
    }
}

impl UseDeclCommon for UseDecl {
    fn source(&self) -> &UseSource {
        match &self {
            UseDecl::Service(u) => u.source(),
            UseDecl::Protocol(u) => u.source(),
            UseDecl::Directory(u) => u.source(),
            UseDecl::Storage(u) => u.source(),
            UseDecl::EventStream(u) => u.source(),
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            UseDecl::Runner(u) => u.source(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            UseDecl::Config(u) => u.source(),
        }
    }

    fn availability(&self) -> &Availability {
        match &self {
            UseDecl::Service(u) => u.availability(),
            UseDecl::Protocol(u) => u.availability(),
            UseDecl::Directory(u) => u.availability(),
            UseDecl::Storage(u) => u.availability(),
            UseDecl::EventStream(u) => u.availability(),
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            UseDecl::Runner(u) => u.availability(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            UseDecl::Config(u) => u.availability(),
        }
    }
}

impl OfferDeclCommon for OfferDecl {
    fn target_name(&self) -> &Name {
        match &self {
            OfferDecl::Service(o) => o.target_name(),
            OfferDecl::Protocol(o) => o.target_name(),
            OfferDecl::Directory(o) => o.target_name(),
            OfferDecl::Storage(o) => o.target_name(),
            OfferDecl::Runner(o) => o.target_name(),
            OfferDecl::Resolver(o) => o.target_name(),
            OfferDecl::EventStream(o) => o.target_name(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            OfferDecl::Dictionary(o) => o.target_name(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            OfferDecl::Config(o) => o.target_name(),
        }
    }

    fn target(&self) -> &OfferTarget {
        match &self {
            OfferDecl::Service(o) => o.target(),
            OfferDecl::Protocol(o) => o.target(),
            OfferDecl::Directory(o) => o.target(),
            OfferDecl::Storage(o) => o.target(),
            OfferDecl::Runner(o) => o.target(),
            OfferDecl::Resolver(o) => o.target(),
            OfferDecl::EventStream(o) => o.target(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            OfferDecl::Dictionary(o) => o.target(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            OfferDecl::Config(o) => o.target(),
        }
    }

    fn source(&self) -> &OfferSource {
        match &self {
            OfferDecl::Service(o) => o.source(),
            OfferDecl::Protocol(o) => o.source(),
            OfferDecl::Directory(o) => o.source(),
            OfferDecl::Storage(o) => o.source(),
            OfferDecl::Runner(o) => o.source(),
            OfferDecl::Resolver(o) => o.source(),
            OfferDecl::EventStream(o) => o.source(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            OfferDecl::Dictionary(o) => o.source(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            OfferDecl::Config(o) => o.source(),
        }
    }

    fn availability(&self) -> &Availability {
        match &self {
            OfferDecl::Service(o) => o.availability(),
            OfferDecl::Protocol(o) => o.availability(),
            OfferDecl::Directory(o) => o.availability(),
            OfferDecl::Storage(o) => o.availability(),
            OfferDecl::Runner(o) => o.availability(),
            OfferDecl::Resolver(o) => o.availability(),
            OfferDecl::EventStream(o) => o.availability(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            OfferDecl::Dictionary(o) => o.availability(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            OfferDecl::Config(o) => o.availability(),
        }
    }
}

impl SourceName for OfferRunnerDecl {
    fn source_name(&self) -> &Name {
        &self.source_name
    }
}

impl OfferDeclCommon for OfferRunnerDecl {
    fn target_name(&self) -> &Name {
        &self.target_name
    }

    fn target(&self) -> &OfferTarget {
        &self.target
    }

    fn source(&self) -> &OfferSource {
        &self.source
    }

    fn availability(&self) -> &Availability {
        &Availability::Required
    }
}

#[cfg_attr(
    feature = "serde",
    derive(Deserialize, Serialize),
    serde(tag = "type", rename_all = "snake_case")
)]
#[derive(FidlDecl, FromEnum, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_union = "fdecl::Expose")]
pub enum ExposeDecl {
    Service(ExposeServiceDecl),
    Protocol(ExposeProtocolDecl),
    Directory(ExposeDirectoryDecl),
    Runner(ExposeRunnerDecl),
    Resolver(ExposeResolverDecl),
    #[cfg(fuchsia_api_level_at_least = "25")]
    Dictionary(ExposeDictionaryDecl),
    #[cfg(fuchsia_api_level_at_least = "20")]
    Config(ExposeConfigurationDecl),
}

impl SourceName for ExposeDecl {
    fn source_name(&self) -> &Name {
        match self {
            Self::Service(e) => e.source_name(),
            Self::Protocol(e) => e.source_name(),
            Self::Directory(e) => e.source_name(),
            Self::Runner(e) => e.source_name(),
            Self::Resolver(e) => e.source_name(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            Self::Dictionary(e) => e.source_name(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            Self::Config(e) => e.source_name(),
        }
    }
}

impl SourcePath for ExposeDecl {
    fn source_path(&self) -> BorrowedSeparatedPath<'_> {
        match self {
            Self::Service(e) => e.source_path(),
            Self::Protocol(e) => e.source_path(),
            Self::Directory(e) => e.source_path(),
            Self::Runner(e) => e.source_path(),
            Self::Resolver(e) => e.source_path(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            Self::Dictionary(e) => e.source_path(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            Self::Config(e) => e.source_path(),
        }
    }
}

impl ExposeDeclCommon for ExposeDecl {
    fn source(&self) -> &ExposeSource {
        match self {
            Self::Service(e) => e.source(),
            Self::Protocol(e) => e.source(),
            Self::Directory(e) => e.source(),
            Self::Runner(e) => e.source(),
            Self::Resolver(e) => e.source(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            Self::Dictionary(e) => e.source(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            Self::Config(e) => e.source(),
        }
    }

    fn target(&self) -> &ExposeTarget {
        match self {
            Self::Service(e) => e.target(),
            Self::Protocol(e) => e.target(),
            Self::Directory(e) => e.target(),
            Self::Runner(e) => e.target(),
            Self::Resolver(e) => e.target(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            Self::Dictionary(e) => e.target(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            Self::Config(e) => e.target(),
        }
    }

    fn target_name(&self) -> &Name {
        match self {
            Self::Service(e) => e.target_name(),
            Self::Protocol(e) => e.target_name(),
            Self::Directory(e) => e.target_name(),
            Self::Runner(e) => e.target_name(),
            Self::Resolver(e) => e.target_name(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            Self::Dictionary(e) => e.target_name(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            Self::Config(e) => e.target_name(),
        }
    }

    fn availability(&self) -> &Availability {
        match self {
            Self::Service(e) => e.availability(),
            Self::Protocol(e) => e.availability(),
            Self::Directory(e) => e.availability(),
            Self::Runner(e) => e.availability(),
            Self::Resolver(e) => e.availability(),
            #[cfg(fuchsia_api_level_at_least = "25")]
            Self::Dictionary(e) => e.availability(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            Self::Config(e) => e.availability(),
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, ExposeDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::ExposeService", source_path = "dictionary")]
pub struct ExposeServiceDecl {
    pub source: ExposeSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: ExposeTarget,
    pub target_name: Name,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, ExposeDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::ExposeProtocol", source_path = "dictionary")]
pub struct ExposeProtocolDecl {
    pub source: ExposeSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: ExposeTarget,
    pub target_name: Name,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, ExposeDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::ExposeDirectory", source_path = "dictionary")]
pub struct ExposeDirectoryDecl {
    pub source: ExposeSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: ExposeTarget,
    pub target_name: Name,

    #[cfg_attr(
        feature = "serde",
        serde(
            deserialize_with = "serde_ext::deserialize_opt_fio_operations",
            serialize_with = "serde_ext::serialize_opt_fio_operations"
        )
    )]
    pub rights: Option<fio::Operations>,

    #[fidl_decl(default_preserve_none)]
    pub subdir: RelativePath,

    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, ExposeDeclCommonAlwaysRequired, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::ExposeRunner", source_path = "dictionary")]
pub struct ExposeRunnerDecl {
    pub source: ExposeSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: ExposeTarget,
    pub target_name: Name,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, ExposeDeclCommonAlwaysRequired, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::ExposeResolver", source_path = "dictionary")]
pub struct ExposeResolverDecl {
    pub source: ExposeSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: ExposeTarget,
    pub target_name: Name,
}

#[cfg(fuchsia_api_level_at_least = "25")]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, ExposeDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::ExposeDictionary", source_path = "dictionary")]
pub struct ExposeDictionaryDecl {
    pub source: ExposeSource,
    pub source_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    pub target: ExposeTarget,
    pub target_name: Name,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg(fuchsia_api_level_at_least = "20")]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, ExposeDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::ExposeConfiguration", source_path = "name_only")]
pub struct ExposeConfigurationDecl {
    pub source: ExposeSource,
    pub source_name: Name,
    pub target: ExposeTarget,
    pub target_name: Name,
    #[cfg(fuchsia_api_level_at_least = "25")]
    #[fidl_decl(default_preserve_none)]
    pub source_dictionary: RelativePath,
    #[fidl_decl(default)]
    pub availability: Availability,
}

#[cfg_attr(
    feature = "serde",
    derive(Deserialize, Serialize),
    serde(tag = "type", rename_all = "snake_case")
)]
#[derive(FidlDecl, FromEnum, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_union = "fdecl::Capability")]
pub enum CapabilityDecl {
    Service(ServiceDecl),
    Protocol(ProtocolDecl),
    Directory(DirectoryDecl),
    Storage(StorageDecl),
    Runner(RunnerDecl),
    Resolver(ResolverDecl),
    EventStream(EventStreamDecl),
    #[cfg(fuchsia_api_level_at_least = "25")]
    Dictionary(DictionaryDecl),
    #[cfg(fuchsia_api_level_at_least = "20")]
    Config(ConfigurationDecl),
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Service")]
pub struct ServiceDecl {
    pub name: Name,
    pub source_path: Option<Path>,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Protocol")]
pub struct ProtocolDecl {
    pub name: Name,
    pub source_path: Option<Path>,
    #[fidl_decl(default)]
    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    pub delivery: DeliveryType,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Directory")]
pub struct DirectoryDecl {
    pub name: Name,
    pub source_path: Option<Path>,

    #[cfg_attr(
        feature = "serde",
        serde(
            deserialize_with = "serde_ext::deserialize_fio_operations",
            serialize_with = "serde_ext::serialize_fio_operations"
        )
    )]
    pub rights: fio::Operations,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Storage")]
pub struct StorageDecl {
    pub name: Name,
    pub source: StorageDirectorySource,
    pub backing_dir: Name,
    #[fidl_decl(default_preserve_none)]
    pub subdir: RelativePath,
    #[cfg_attr(feature = "serde", serde(with = "serde_ext::StorageId"))]
    pub storage_id: fdecl::StorageId,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Runner")]
pub struct RunnerDecl {
    pub name: Name,
    pub source_path: Option<Path>,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Resolver")]
pub struct ResolverDecl {
    pub name: Name,
    pub source_path: Option<Path>,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::EventStream")]
pub struct EventStreamDecl {
    pub name: Name,
}

#[cfg(fuchsia_api_level_at_least = "25")]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Dictionary")]
pub struct DictionaryDecl {
    pub name: Name,
    pub source_path: Option<Path>,
}

#[cfg(fuchsia_api_level_at_least = "20")]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Configuration")]
pub struct ConfigurationDecl {
    pub name: Name,
    pub value: ConfigValue,
}

impl CapabilityDecl {
    pub fn name(&self) -> &Name {
        match self {
            CapabilityDecl::Directory(decl) => &decl.name,
            CapabilityDecl::Protocol(decl) => &decl.name,
            CapabilityDecl::Resolver(decl) => &decl.name,
            CapabilityDecl::Runner(decl) => &decl.name,
            CapabilityDecl::Service(decl) => &decl.name,
            CapabilityDecl::Storage(decl) => &decl.name,
            CapabilityDecl::EventStream(decl) => &decl.name,
            #[cfg(fuchsia_api_level_at_least = "25")]
            CapabilityDecl::Dictionary(decl) => &decl.name,
            #[cfg(fuchsia_api_level_at_least = "20")]
            CapabilityDecl::Config(decl) => &decl.name,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            CapabilityDecl::Directory(decl) => decl.source_path.as_ref(),
            CapabilityDecl::Protocol(decl) => decl.source_path.as_ref(),
            CapabilityDecl::Resolver(decl) => decl.source_path.as_ref(),
            CapabilityDecl::Runner(decl) => decl.source_path.as_ref(),
            CapabilityDecl::Service(decl) => decl.source_path.as_ref(),
            CapabilityDecl::Storage(_) => None,
            CapabilityDecl::EventStream(_) => None,
            #[cfg(fuchsia_api_level_at_least = "25")]
            CapabilityDecl::Dictionary(_) => None,
            #[cfg(fuchsia_api_level_at_least = "20")]
            CapabilityDecl::Config(_) => None,
        }
    }
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Child")]
pub struct ChildDecl {
    pub name: LongName,
    pub url: Url,
    pub startup: fdecl::StartupMode,
    pub on_terminate: Option<fdecl::OnTerminate>,
    pub environment: Option<Name>,
    pub config_overrides: Option<Vec<ConfigOverride>>,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChildRef {
    pub name: LongName,
    pub collection: Option<Name>,
}

impl std::fmt::Display for ChildRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(collection) = &self.collection {
            write!(f, "{}:{}", collection, self.name)
        } else {
            write!(f, "{}", self.name)
        }
    }
}

impl FidlIntoNative<ChildRef> for fdecl::ChildRef {
    fn fidl_into_native(self) -> ChildRef {
        // cm_fidl_validator should have already validated this
        ChildRef {
            name: self.name.parse().unwrap(),
            collection: self.collection.map(|c| c.parse().unwrap()),
        }
    }
}

impl NativeIntoFidl<fdecl::ChildRef> for ChildRef {
    fn native_into_fidl(self) -> fdecl::ChildRef {
        fdecl::ChildRef {
            name: self.name.to_string(),
            collection: self.collection.map(|c| c.to_string()),
        }
    }
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Collection")]
pub struct CollectionDecl {
    pub name: Name,
    pub durability: fdecl::Durability,
    pub environment: Option<Name>,

    #[fidl_decl(default)]
    pub allowed_offers: AllowedOffers,
    #[fidl_decl(default)]
    pub allow_long_names: bool,

    pub persistent_storage: Option<bool>,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::Environment")]
pub struct EnvironmentDecl {
    pub name: Name,
    pub extends: fdecl::EnvironmentExtends,
    pub runners: Vec<RunnerRegistration>,
    pub resolvers: Vec<ResolverRegistration>,
    pub debug_capabilities: Vec<DebugRegistration>,
    pub stop_timeout_ms: Option<u32>,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_table = "fdecl::ConfigOverride")]
pub struct ConfigOverride {
    pub key: String,
    pub value: ConfigValue,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_table = "fdecl::ConfigSchema")]
pub struct ConfigDecl {
    pub fields: Vec<ConfigField>,
    pub checksum: ConfigChecksum,
    pub value_source: ConfigValueSource,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_union = "fdecl::ConfigChecksum")]
pub enum ConfigChecksum {
    Sha256([u8; 32]),
}

#[cfg(fuchsia_api_level_at_least = "HEAD")]
#[derive(FidlDecl, Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_table = "fdecl::ConfigSourceCapabilities")]
pub struct ConfigSourceCapabilities {}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_union = "fdecl::ConfigValueSource")]
pub enum ConfigValueSource {
    PackagePath(String),
    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    Capabilities(ConfigSourceCapabilities),
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_table = "fdecl::ConfigField")]
pub struct ConfigField {
    pub key: String,
    pub type_: ConfigValueType,

    // This field will not be present in compiled manifests which predate F12.
    #[fidl_decl(default)]
    pub mutability: ConfigMutability,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigNestedValueType {
    Bool,
    Uint8,
    Int8,
    Uint16,
    Int16,
    Uint32,
    Int32,
    Uint64,
    Int64,
    String { max_size: u32 },
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValueType {
    Bool,
    Uint8,
    Int8,
    Uint16,
    Int16,
    Uint32,
    Int32,
    Uint64,
    Int64,
    String { max_size: u32 },
    Vector { nested_type: ConfigNestedValueType, max_count: u32 },
}

impl ConfigValueType {
    pub fn get_max_size(&self) -> Option<u32> {
        match self {
            ConfigValueType::String { max_size } => Some(*max_size),
            ConfigValueType::Bool
            | ConfigValueType::Uint8
            | ConfigValueType::Int8
            | ConfigValueType::Uint16
            | ConfigValueType::Int16
            | ConfigValueType::Uint32
            | ConfigValueType::Int32
            | ConfigValueType::Uint64
            | ConfigValueType::Int64
            | ConfigValueType::Vector { .. } => None,
        }
    }

    pub fn get_nested_type(&self) -> Option<ConfigNestedValueType> {
        match self {
            ConfigValueType::Vector { nested_type, .. } => Some(nested_type.clone()),
            ConfigValueType::Bool
            | ConfigValueType::Uint8
            | ConfigValueType::Int8
            | ConfigValueType::Uint16
            | ConfigValueType::Int16
            | ConfigValueType::Uint32
            | ConfigValueType::Int32
            | ConfigValueType::Uint64
            | ConfigValueType::Int64
            | ConfigValueType::String { .. } => None,
        }
    }

    pub fn get_max_count(&self) -> Option<u32> {
        match self {
            ConfigValueType::Vector { max_count, .. } => Some(*max_count),
            ConfigValueType::Bool
            | ConfigValueType::Uint8
            | ConfigValueType::Int8
            | ConfigValueType::Uint16
            | ConfigValueType::Int16
            | ConfigValueType::Uint32
            | ConfigValueType::Int32
            | ConfigValueType::Uint64
            | ConfigValueType::Int64
            | ConfigValueType::String { .. } => None,
        }
    }
}

impl FidlIntoNative<ConfigNestedValueType> for fdecl::ConfigType {
    fn fidl_into_native(mut self) -> ConfigNestedValueType {
        match self.layout {
            fdecl::ConfigTypeLayout::Bool => ConfigNestedValueType::Bool,
            fdecl::ConfigTypeLayout::Uint8 => ConfigNestedValueType::Uint8,
            fdecl::ConfigTypeLayout::Uint16 => ConfigNestedValueType::Uint16,
            fdecl::ConfigTypeLayout::Uint32 => ConfigNestedValueType::Uint32,
            fdecl::ConfigTypeLayout::Uint64 => ConfigNestedValueType::Uint64,
            fdecl::ConfigTypeLayout::Int8 => ConfigNestedValueType::Int8,
            fdecl::ConfigTypeLayout::Int16 => ConfigNestedValueType::Int16,
            fdecl::ConfigTypeLayout::Int32 => ConfigNestedValueType::Int32,
            fdecl::ConfigTypeLayout::Int64 => ConfigNestedValueType::Int64,
            fdecl::ConfigTypeLayout::String => {
                let max_size =
                    if let fdecl::LayoutConstraint::MaxSize(s) = self.constraints.remove(0) {
                        s
                    } else {
                        panic!("Unexpected constraint on String layout type for config field");
                    };
                ConfigNestedValueType::String { max_size }
            }
            fdecl::ConfigTypeLayout::Vector => {
                panic!("Nested vectors are not supported in structured config")
            }
            fdecl::ConfigTypeLayoutUnknown!() => panic!("Unknown layout type for config field"),
        }
    }
}

impl NativeIntoFidl<fdecl::ConfigType> for ConfigNestedValueType {
    fn native_into_fidl(self) -> fdecl::ConfigType {
        let layout = match self {
            ConfigNestedValueType::Bool => fdecl::ConfigTypeLayout::Bool,
            ConfigNestedValueType::Uint8 => fdecl::ConfigTypeLayout::Uint8,
            ConfigNestedValueType::Uint16 => fdecl::ConfigTypeLayout::Uint16,
            ConfigNestedValueType::Uint32 => fdecl::ConfigTypeLayout::Uint32,
            ConfigNestedValueType::Uint64 => fdecl::ConfigTypeLayout::Uint64,
            ConfigNestedValueType::Int8 => fdecl::ConfigTypeLayout::Int8,
            ConfigNestedValueType::Int16 => fdecl::ConfigTypeLayout::Int16,
            ConfigNestedValueType::Int32 => fdecl::ConfigTypeLayout::Int32,
            ConfigNestedValueType::Int64 => fdecl::ConfigTypeLayout::Int64,
            ConfigNestedValueType::String { .. } => fdecl::ConfigTypeLayout::String,
        };
        let constraints = match self {
            ConfigNestedValueType::String { max_size } => {
                vec![fdecl::LayoutConstraint::MaxSize(max_size)]
            }
            _ => vec![],
        };
        fdecl::ConfigType { layout, constraints, parameters: Some(vec![]) }
    }
}

impl FidlIntoNative<ConfigValueType> for fdecl::ConfigType {
    fn fidl_into_native(mut self) -> ConfigValueType {
        match self.layout {
            fdecl::ConfigTypeLayout::Bool => ConfigValueType::Bool,
            fdecl::ConfigTypeLayout::Uint8 => ConfigValueType::Uint8,
            fdecl::ConfigTypeLayout::Uint16 => ConfigValueType::Uint16,
            fdecl::ConfigTypeLayout::Uint32 => ConfigValueType::Uint32,
            fdecl::ConfigTypeLayout::Uint64 => ConfigValueType::Uint64,
            fdecl::ConfigTypeLayout::Int8 => ConfigValueType::Int8,
            fdecl::ConfigTypeLayout::Int16 => ConfigValueType::Int16,
            fdecl::ConfigTypeLayout::Int32 => ConfigValueType::Int32,
            fdecl::ConfigTypeLayout::Int64 => ConfigValueType::Int64,
            fdecl::ConfigTypeLayout::String => {
                let max_size = if let fdecl::LayoutConstraint::MaxSize(s) =
                    self.constraints.remove(0)
                {
                    s
                } else {
                    panic!("Unexpected constraint on String layout type for config field. Expected MaxSize.");
                };
                ConfigValueType::String { max_size }
            }
            fdecl::ConfigTypeLayout::Vector => {
                let max_count = if let fdecl::LayoutConstraint::MaxSize(c) =
                    self.constraints.remove(0)
                {
                    c
                } else {
                    panic!("Unexpected constraint on Vector layout type for config field. Expected MaxSize.");
                };
                let mut parameters =
                    self.parameters.expect("Config field must have parameters set");
                let nested_type = if let fdecl::LayoutParameter::NestedType(nested_type) =
                    parameters.remove(0)
                {
                    nested_type.fidl_into_native()
                } else {
                    panic!("Unexpected parameter on Vector layout type for config field. Expected NestedType.");
                };
                ConfigValueType::Vector { max_count, nested_type }
            }
            fdecl::ConfigTypeLayoutUnknown!() => panic!("Unknown layout type for config field"),
        }
    }
}

impl NativeIntoFidl<fdecl::ConfigType> for ConfigValueType {
    fn native_into_fidl(self) -> fdecl::ConfigType {
        let layout = match self {
            ConfigValueType::Bool => fdecl::ConfigTypeLayout::Bool,
            ConfigValueType::Uint8 => fdecl::ConfigTypeLayout::Uint8,
            ConfigValueType::Uint16 => fdecl::ConfigTypeLayout::Uint16,
            ConfigValueType::Uint32 => fdecl::ConfigTypeLayout::Uint32,
            ConfigValueType::Uint64 => fdecl::ConfigTypeLayout::Uint64,
            ConfigValueType::Int8 => fdecl::ConfigTypeLayout::Int8,
            ConfigValueType::Int16 => fdecl::ConfigTypeLayout::Int16,
            ConfigValueType::Int32 => fdecl::ConfigTypeLayout::Int32,
            ConfigValueType::Int64 => fdecl::ConfigTypeLayout::Int64,
            ConfigValueType::String { .. } => fdecl::ConfigTypeLayout::String,
            ConfigValueType::Vector { .. } => fdecl::ConfigTypeLayout::Vector,
        };
        let (constraints, parameters) = match self {
            ConfigValueType::String { max_size } => {
                (vec![fdecl::LayoutConstraint::MaxSize(max_size)], vec![])
            }
            ConfigValueType::Vector { max_count, nested_type } => {
                let nested_type = nested_type.native_into_fidl();
                (
                    vec![fdecl::LayoutConstraint::MaxSize(max_count)],
                    vec![fdecl::LayoutParameter::NestedType(nested_type)],
                )
            }
            _ => (vec![], vec![]),
        };
        fdecl::ConfigType { layout, constraints, parameters: Some(parameters) }
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
    // TODO(https://fxbug.dev/42075220) uncomment once bitflags is updated
    // pub struct ConfigMutability: <fdecl::ConfigMutability as bitflags::BitFlags>::Bits {
    pub struct ConfigMutability: u32 {
        const PARENT = fdecl::ConfigMutability::PARENT.bits();
    }
}

#[cfg(feature = "serde")]
bitflags_serde_legacy::impl_traits!(ConfigMutability);

impl NativeIntoFidl<fdecl::ConfigMutability> for ConfigMutability {
    fn native_into_fidl(self) -> fdecl::ConfigMutability {
        fdecl::ConfigMutability::from_bits_allow_unknown(self.bits())
    }
}

impl FidlIntoNative<ConfigMutability> for fdecl::ConfigMutability {
    fn fidl_into_native(self) -> ConfigMutability {
        ConfigMutability::from_bits_retain(self.bits())
    }
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_table = "fdecl::ConfigValuesData")]
pub struct ConfigValuesData {
    pub values: Vec<ConfigValueSpec>,
    pub checksum: ConfigChecksum,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_table = "fdecl::ConfigValueSpec")]
pub struct ConfigValueSpec {
    pub value: ConfigValue,
}

#[derive(FromEnum, FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_union = "fdecl::ConfigValue")]
pub enum ConfigValue {
    Single(ConfigSingleValue),
    Vector(ConfigVectorValue),
}

impl ConfigValue {
    /// Return the type of this value.
    pub fn ty(&self) -> ConfigValueType {
        match self {
            Self::Single(sv) => sv.ty(),
            Self::Vector(vv) => vv.ty(),
        }
    }

    /// Check if this value matches the type of another value.
    pub fn matches_type(&self, other: &ConfigValue) -> bool {
        match (self, other) {
            (ConfigValue::Single(a), ConfigValue::Single(b)) => {
                std::mem::discriminant(a) == std::mem::discriminant(b)
            }
            (ConfigValue::Vector(a), ConfigValue::Vector(b)) => {
                std::mem::discriminant(a) == std::mem::discriminant(b)
            }
            _ => false,
        }
    }
}

impl From<&str> for ConfigValue {
    fn from(value: &str) -> Self {
        ConfigValue::Single(value.to_string().into())
    }
}

impl From<Vec<&str>> for ConfigValue {
    fn from(value: Vec<&str>) -> Self {
        let value: Vec<_> = value.into_iter().map(|s| s.to_string()).collect();
        ConfigValue::Vector(value.into())
    }
}

macro_rules! generate_configvalue_from {
    ($name:expr, $type:ty) => {
        impl From<$type> for ConfigValue {
            fn from(value: $type) -> Self {
                $name(value.into())
            }
        }
    };
}

generate_configvalue_from!(ConfigValue::Single, bool);
generate_configvalue_from!(ConfigValue::Single, u8);
generate_configvalue_from!(ConfigValue::Single, u16);
generate_configvalue_from!(ConfigValue::Single, u32);
generate_configvalue_from!(ConfigValue::Single, u64);
generate_configvalue_from!(ConfigValue::Single, i8);
generate_configvalue_from!(ConfigValue::Single, i16);
generate_configvalue_from!(ConfigValue::Single, i32);
generate_configvalue_from!(ConfigValue::Single, i64);
generate_configvalue_from!(ConfigValue::Single, String);
generate_configvalue_from!(ConfigValue::Vector, Vec<bool>);
generate_configvalue_from!(ConfigValue::Vector, Vec<u8>);
generate_configvalue_from!(ConfigValue::Vector, Vec<u16>);
generate_configvalue_from!(ConfigValue::Vector, Vec<u32>);
generate_configvalue_from!(ConfigValue::Vector, Vec<u64>);
generate_configvalue_from!(ConfigValue::Vector, Vec<i8>);
generate_configvalue_from!(ConfigValue::Vector, Vec<i16>);
generate_configvalue_from!(ConfigValue::Vector, Vec<i32>);
generate_configvalue_from!(ConfigValue::Vector, Vec<i64>);
generate_configvalue_from!(ConfigValue::Vector, Vec<String>);

impl fmt::Display for ConfigValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigValue::Single(sv) => sv.fmt(f),
            ConfigValue::Vector(lv) => lv.fmt(f),
        }
    }
}

#[derive(FromEnum, FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_union = "fdecl::ConfigSingleValue")]
pub enum ConfigSingleValue {
    Bool(bool),
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uint64(u64),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    String(String),
}

impl ConfigSingleValue {
    fn ty(&self) -> ConfigValueType {
        match self {
            ConfigSingleValue::Bool(_) => ConfigValueType::Bool,
            ConfigSingleValue::Uint8(_) => ConfigValueType::Uint8,
            ConfigSingleValue::Uint16(_) => ConfigValueType::Uint16,
            ConfigSingleValue::Uint32(_) => ConfigValueType::Uint32,
            ConfigSingleValue::Uint64(_) => ConfigValueType::Uint64,
            ConfigSingleValue::Int8(_) => ConfigValueType::Int8,
            ConfigSingleValue::Int16(_) => ConfigValueType::Int16,
            ConfigSingleValue::Int32(_) => ConfigValueType::Int32,
            ConfigSingleValue::Int64(_) => ConfigValueType::Int64,
            // We substitute the max size limit because the value itself doesn't carry the info.
            ConfigSingleValue::String(_) => ConfigValueType::String { max_size: std::u32::MAX },
        }
    }
}

impl fmt::Display for ConfigSingleValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ConfigSingleValue::*;
        match self {
            Bool(v) => write!(f, "{}", v),
            Uint8(v) => write!(f, "{}", v),
            Uint16(v) => write!(f, "{}", v),
            Uint32(v) => write!(f, "{}", v),
            Uint64(v) => write!(f, "{}", v),
            Int8(v) => write!(f, "{}", v),
            Int16(v) => write!(f, "{}", v),
            Int32(v) => write!(f, "{}", v),
            Int64(v) => write!(f, "{}", v),
            String(v) => write!(f, "\"{}\"", v),
        }
    }
}

#[derive(FromEnum, FidlDecl, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[fidl_decl(fidl_union = "fdecl::ConfigVectorValue")]
pub enum ConfigVectorValue {
    BoolVector(Vec<bool>),
    Uint8Vector(Vec<u8>),
    Uint16Vector(Vec<u16>),
    Uint32Vector(Vec<u32>),
    Uint64Vector(Vec<u64>),
    Int8Vector(Vec<i8>),
    Int16Vector(Vec<i16>),
    Int32Vector(Vec<i32>),
    Int64Vector(Vec<i64>),
    StringVector(Vec<String>),
}

impl ConfigVectorValue {
    fn ty(&self) -> ConfigValueType {
        // We substitute the max size limit because the value itself doesn't carry the info.
        match self {
            ConfigVectorValue::BoolVector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::Bool,
                max_count: std::u32::MAX,
            },
            ConfigVectorValue::Uint8Vector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::Uint8,
                max_count: std::u32::MAX,
            },
            ConfigVectorValue::Uint16Vector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::Uint16,
                max_count: std::u32::MAX,
            },
            ConfigVectorValue::Uint32Vector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::Uint32,
                max_count: std::u32::MAX,
            },
            ConfigVectorValue::Uint64Vector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::Uint64,
                max_count: std::u32::MAX,
            },
            ConfigVectorValue::Int8Vector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::Int8,
                max_count: std::u32::MAX,
            },
            ConfigVectorValue::Int16Vector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::Int16,
                max_count: std::u32::MAX,
            },
            ConfigVectorValue::Int32Vector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::Int32,
                max_count: std::u32::MAX,
            },
            ConfigVectorValue::Int64Vector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::Int64,
                max_count: std::u32::MAX,
            },
            ConfigVectorValue::StringVector(_) => ConfigValueType::Vector {
                nested_type: ConfigNestedValueType::String { max_size: std::u32::MAX },
                max_count: std::u32::MAX,
            },
        }
    }
}

impl fmt::Display for ConfigVectorValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ConfigVectorValue::*;
        macro_rules! print_list {
            ($f:ident, $list:ident) => {{
                $f.write_str("[")?;

                for (i, item) in $list.iter().enumerate() {
                    if i > 0 {
                        $f.write_str(", ")?;
                    }
                    write!($f, "{}", item)?;
                }

                $f.write_str("]")
            }};
        }
        match self {
            BoolVector(l) => print_list!(f, l),
            Uint8Vector(l) => print_list!(f, l),
            Uint16Vector(l) => print_list!(f, l),
            Uint32Vector(l) => print_list!(f, l),
            Uint64Vector(l) => print_list!(f, l),
            Int8Vector(l) => print_list!(f, l),
            Int16Vector(l) => print_list!(f, l),
            Int32Vector(l) => print_list!(f, l),
            Int64Vector(l) => print_list!(f, l),
            StringVector(l) => {
                f.write_str("[")?;
                for (i, item) in l.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "\"{}\"", item)?;
                }
                f.write_str("]")
            }
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::RunnerRegistration")]
pub struct RunnerRegistration {
    pub source_name: Name,
    pub target_name: Name,
    pub source: RegistrationSource,
}

impl SourceName for RunnerRegistration {
    fn source_name(&self) -> &Name {
        &self.source_name
    }
}

impl RegistrationDeclCommon for RunnerRegistration {
    const TYPE: &'static str = "runner";

    fn source(&self) -> &RegistrationSource {
        &self.source
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::ResolverRegistration")]
pub struct ResolverRegistration {
    pub resolver: Name,
    pub source: RegistrationSource,
    pub scheme: String,
}

impl SourceName for ResolverRegistration {
    fn source_name(&self) -> &Name {
        &self.resolver
    }
}

impl RegistrationDeclCommon for ResolverRegistration {
    const TYPE: &'static str = "resolver";

    fn source(&self) -> &RegistrationSource {
        &self.source
    }
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_union = "fdecl::DebugRegistration")]
pub enum DebugRegistration {
    Protocol(DebugProtocolRegistration),
}

impl RegistrationDeclCommon for DebugRegistration {
    const TYPE: &'static str = "debug_protocol";

    fn source(&self) -> &RegistrationSource {
        match self {
            DebugRegistration::Protocol(protocol_reg) => &protocol_reg.source,
        }
    }
}

impl SourceName for DebugRegistration {
    fn source_name(&self) -> &Name {
        match self {
            DebugRegistration::Protocol(protocol_reg) => &protocol_reg.source_name,
        }
    }
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fdecl::DebugProtocolRegistration")]
pub struct DebugProtocolRegistration {
    pub source_name: Name,
    pub source: RegistrationSource,
    pub target_name: Name,
}

#[derive(FidlDecl, Debug, Clone, PartialEq)]
#[fidl_decl(fidl_table = "fdecl::Program")]
pub struct ProgramDecl {
    pub runner: Option<Name>,
    pub info: fdata::Dictionary,
}

impl Default for ProgramDecl {
    fn default() -> Self {
        Self { runner: None, info: fdata::Dictionary::default() }
    }
}

fidl_translations_identical!([u8; 32]);
fidl_translations_identical!(u8);
fidl_translations_identical!(u16);
fidl_translations_identical!(u32);
fidl_translations_identical!(u64);
fidl_translations_identical!(i8);
fidl_translations_identical!(i16);
fidl_translations_identical!(i32);
fidl_translations_identical!(i64);
fidl_translations_identical!(bool);
fidl_translations_identical!(String);
fidl_translations_identical!(Vec<Name>);
fidl_translations_identical!(fdecl::StartupMode);
fidl_translations_identical!(fdecl::OnTerminate);
fidl_translations_identical!(fdecl::Durability);
fidl_translations_identical!(fdata::Dictionary);
fidl_translations_identical!(fio::Operations);
fidl_translations_identical!(fdecl::EnvironmentExtends);
fidl_translations_identical!(fdecl::StorageId);
fidl_translations_identical!(Vec<fprocess::HandleInfo>);
fidl_translations_identical!(fsys::ServiceInstance);
fidl_translations_from_into!(cm_types::AllowedOffers, fdecl::AllowedOffers);

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyType {
    Strong,
    Weak,
}

impl Default for DependencyType {
    fn default() -> Self {
        Self::Strong
    }
}

fidl_translations_symmetrical_enums!(fdecl::DependencyType, DependencyType, Strong, Weak);

impl UseDecl {
    pub fn path(&self) -> Option<&Path> {
        match self {
            UseDecl::Service(d) => Some(&d.target_path),
            UseDecl::Protocol(d) => Some(&d.target_path),
            UseDecl::Directory(d) => Some(&d.target_path),
            UseDecl::Storage(d) => Some(&d.target_path),
            UseDecl::EventStream(d) => Some(&d.target_path),
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            UseDecl::Runner(_) => None,
            #[cfg(fuchsia_api_level_at_least = "20")]
            UseDecl::Config(_) => None,
        }
    }

    pub fn name(&self) -> Option<&Name> {
        match self {
            UseDecl::Storage(storage_decl) => Some(&storage_decl.source_name),
            UseDecl::EventStream(_) => None,
            UseDecl::Service(_) | UseDecl::Protocol(_) | UseDecl::Directory(_) => None,
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            UseDecl::Runner(_) => None,
            #[cfg(fuchsia_api_level_at_least = "20")]
            UseDecl::Config(_) => None,
        }
    }
}

impl SourceName for UseDecl {
    fn source_name(&self) -> &Name {
        match self {
            UseDecl::Storage(storage_decl) => &storage_decl.source_name,
            UseDecl::Service(service_decl) => &service_decl.source_name,
            UseDecl::Protocol(protocol_decl) => &protocol_decl.source_name,
            UseDecl::Directory(directory_decl) => &directory_decl.source_name,
            UseDecl::EventStream(event_stream_decl) => &event_stream_decl.source_name,
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            UseDecl::Runner(runner_decl) => &runner_decl.source_name,
            #[cfg(fuchsia_api_level_at_least = "20")]
            UseDecl::Config(u) => &u.source_name,
        }
    }
}

impl SourcePath for UseDecl {
    fn source_path(&self) -> BorrowedSeparatedPath<'_> {
        match self {
            UseDecl::Service(u) => u.source_path(),
            UseDecl::Protocol(u) => u.source_path(),
            UseDecl::Directory(u) => u.source_path(),
            UseDecl::Storage(u) => u.source_path(),
            UseDecl::EventStream(u) => u.source_path(),
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            UseDecl::Runner(u) => u.source_path(),
            #[cfg(fuchsia_api_level_at_least = "20")]
            UseDecl::Config(u) => u.source_path(),
        }
    }
}

/// The trait for all declarations that have a source name.
pub trait SourceName {
    fn source_name(&self) -> &Name;
}

/// The common properties of a [Use](fdecl::Use) declaration.
pub trait UseDeclCommon: SourceName + SourcePath + Send + Sync {
    fn source(&self) -> &UseSource;
    fn availability(&self) -> &Availability;
}

/// The common properties of a Registration-with-environment declaration.
pub trait RegistrationDeclCommon: SourceName + Send + Sync {
    /// The name of the registration type, for error messages.
    const TYPE: &'static str;
    fn source(&self) -> &RegistrationSource;
}

/// The common properties of an [Offer](fdecl::Offer) declaration.
pub trait OfferDeclCommon: SourceName + SourcePath + fmt::Debug + Send + Sync {
    fn target_name(&self) -> &Name;
    fn target(&self) -> &OfferTarget;
    fn source(&self) -> &OfferSource;
    fn availability(&self) -> &Availability;
}

/// The common properties of an [Expose](fdecl::Expose) declaration.
pub trait ExposeDeclCommon: SourceName + SourcePath + fmt::Debug + Send + Sync {
    fn target_name(&self) -> &Name;
    fn target(&self) -> &ExposeTarget;
    fn source(&self) -> &ExposeSource;
    fn availability(&self) -> &Availability;
}

/// A named capability type.
///
/// `CapabilityTypeName` provides a user friendly type encoding for a capability.
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub enum CapabilityTypeName {
    Directory,
    EventStream,
    Protocol,
    Resolver,
    Runner,
    Service,
    Storage,
    Dictionary,
    Config,
}

impl std::str::FromStr for CapabilityTypeName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "directory" => Ok(CapabilityTypeName::Directory),
            "event_stream" => Ok(CapabilityTypeName::EventStream),
            "protocol" => Ok(CapabilityTypeName::Protocol),
            "resolver" => Ok(CapabilityTypeName::Resolver),
            "runner" => Ok(CapabilityTypeName::Runner),
            "service" => Ok(CapabilityTypeName::Service),
            "storage" => Ok(CapabilityTypeName::Storage),
            "dictionary" => Ok(CapabilityTypeName::Dictionary),
            "configuration" => Ok(CapabilityTypeName::Config),
            _ => Err(Error::ParseCapabilityTypeName { raw: s.to_string() }),
        }
    }
}

impl FidlIntoNative<CapabilityTypeName> for String {
    fn fidl_into_native(self) -> CapabilityTypeName {
        self.parse().unwrap()
    }
}

impl NativeIntoFidl<String> for CapabilityTypeName {
    fn native_into_fidl(self) -> String {
        self.to_string()
    }
}

impl fmt::Display for CapabilityTypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let display_name = match &self {
            CapabilityTypeName::Directory => "directory",
            CapabilityTypeName::EventStream => "event_stream",
            CapabilityTypeName::Protocol => "protocol",
            CapabilityTypeName::Resolver => "resolver",
            CapabilityTypeName::Runner => "runner",
            CapabilityTypeName::Service => "service",
            CapabilityTypeName::Storage => "storage",
            CapabilityTypeName::Dictionary => "dictionary",
            CapabilityTypeName::Config => "configuration",
        };
        write!(f, "{}", display_name)
    }
}

impl From<&UseDecl> for CapabilityTypeName {
    fn from(use_decl: &UseDecl) -> Self {
        match use_decl {
            UseDecl::Service(_) => Self::Service,
            UseDecl::Protocol(_) => Self::Protocol,
            UseDecl::Directory(_) => Self::Directory,
            UseDecl::Storage(_) => Self::Storage,
            UseDecl::EventStream(_) => Self::EventStream,
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            UseDecl::Runner(_) => Self::Runner,
            #[cfg(fuchsia_api_level_at_least = "20")]
            UseDecl::Config(_) => Self::Config,
        }
    }
}

impl From<&OfferDecl> for CapabilityTypeName {
    fn from(offer_decl: &OfferDecl) -> Self {
        match offer_decl {
            OfferDecl::Service(_) => Self::Service,
            OfferDecl::Protocol(_) => Self::Protocol,
            OfferDecl::Directory(_) => Self::Directory,
            OfferDecl::Storage(_) => Self::Storage,
            OfferDecl::Runner(_) => Self::Runner,
            OfferDecl::Resolver(_) => Self::Resolver,
            OfferDecl::EventStream(_) => Self::EventStream,
            #[cfg(fuchsia_api_level_at_least = "25")]
            OfferDecl::Dictionary(_) => Self::Dictionary,
            #[cfg(fuchsia_api_level_at_least = "20")]
            OfferDecl::Config(_) => Self::Config,
        }
    }
}

impl From<&ExposeDecl> for CapabilityTypeName {
    fn from(expose_decl: &ExposeDecl) -> Self {
        match expose_decl {
            ExposeDecl::Service(_) => Self::Service,
            ExposeDecl::Protocol(_) => Self::Protocol,
            ExposeDecl::Directory(_) => Self::Directory,
            ExposeDecl::Runner(_) => Self::Runner,
            ExposeDecl::Resolver(_) => Self::Resolver,
            #[cfg(fuchsia_api_level_at_least = "25")]
            ExposeDecl::Dictionary(_) => Self::Dictionary,
            #[cfg(fuchsia_api_level_at_least = "20")]
            ExposeDecl::Config(_) => Self::Config,
        }
    }
}

impl From<&CapabilityDecl> for CapabilityTypeName {
    fn from(capability: &CapabilityDecl) -> Self {
        match capability {
            CapabilityDecl::Service(_) => Self::Service,
            CapabilityDecl::Protocol(_) => Self::Protocol,
            CapabilityDecl::Directory(_) => Self::Directory,
            CapabilityDecl::Storage(_) => Self::Storage,
            CapabilityDecl::Runner(_) => Self::Runner,
            CapabilityDecl::Resolver(_) => Self::Resolver,
            CapabilityDecl::EventStream(_) => Self::EventStream,
            #[cfg(fuchsia_api_level_at_least = "25")]
            CapabilityDecl::Dictionary(_) => Self::Dictionary,
            #[cfg(fuchsia_api_level_at_least = "20")]
            CapabilityDecl::Config(_) => Self::Config,
        }
    }
}

impl From<CapabilityTypeName> for fio::DirentType {
    fn from(value: CapabilityTypeName) -> Self {
        match value {
            CapabilityTypeName::Directory => fio::DirentType::Directory,
            CapabilityTypeName::EventStream => fio::DirentType::Service,
            CapabilityTypeName::Protocol => fio::DirentType::Service,
            CapabilityTypeName::Service => fio::DirentType::Directory,
            CapabilityTypeName::Storage => fio::DirentType::Directory,
            CapabilityTypeName::Dictionary => fio::DirentType::Directory,
            CapabilityTypeName::Resolver => fio::DirentType::Service,
            CapabilityTypeName::Runner => fio::DirentType::Service,
            // Config capabilities don't appear in exposed or used dir
            CapabilityTypeName::Config => fio::DirentType::Unknown,
        }
    }
}

// TODO: Runners and third parties can use this to parse `facets`.
impl FidlIntoNative<HashMap<String, DictionaryValue>> for fdata::Dictionary {
    fn fidl_into_native(self) -> HashMap<String, DictionaryValue> {
        from_fidl_dict(self)
    }
}

impl NativeIntoFidl<fdata::Dictionary> for HashMap<String, DictionaryValue> {
    fn native_into_fidl(self) -> fdata::Dictionary {
        to_fidl_dict(self)
    }
}

impl FidlIntoNative<BTreeMap<String, DictionaryValue>> for fdata::Dictionary {
    fn fidl_into_native(self) -> BTreeMap<String, DictionaryValue> {
        from_fidl_dict_btree(self)
    }
}

impl NativeIntoFidl<fdata::Dictionary> for BTreeMap<String, DictionaryValue> {
    fn native_into_fidl(self) -> fdata::Dictionary {
        to_fidl_dict_btree(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DictionaryValue {
    Str(String),
    StrVec(Vec<String>),
    Null,
}

impl FidlIntoNative<DictionaryValue> for Option<Box<fdata::DictionaryValue>> {
    fn fidl_into_native(self) -> DictionaryValue {
        // Temporarily allow unreachable patterns while fuchsia.data.DictionaryValue
        // is migrated from `strict` to `flexible`.
        // TODO(https://fxbug.dev/42173900): Remove this.
        #[allow(unreachable_patterns)]
        match self {
            Some(v) => match *v {
                fdata::DictionaryValue::Str(s) => DictionaryValue::Str(s),
                fdata::DictionaryValue::StrVec(ss) => DictionaryValue::StrVec(ss),
                _ => DictionaryValue::Null,
            },
            None => DictionaryValue::Null,
        }
    }
}

impl NativeIntoFidl<Option<Box<fdata::DictionaryValue>>> for DictionaryValue {
    fn native_into_fidl(self) -> Option<Box<fdata::DictionaryValue>> {
        match self {
            DictionaryValue::Str(s) => Some(Box::new(fdata::DictionaryValue::Str(s))),
            DictionaryValue::StrVec(ss) => Some(Box::new(fdata::DictionaryValue::StrVec(ss))),
            DictionaryValue::Null => None,
        }
    }
}

fn from_fidl_dict(dict: fdata::Dictionary) -> HashMap<String, DictionaryValue> {
    match dict.entries {
        Some(entries) => entries.into_iter().map(|e| (e.key, e.value.fidl_into_native())).collect(),
        _ => HashMap::new(),
    }
}

fn to_fidl_dict(dict: HashMap<String, DictionaryValue>) -> fdata::Dictionary {
    fdata::Dictionary {
        entries: Some(
            dict.into_iter()
                .map(|(key, value)| fdata::DictionaryEntry { key, value: value.native_into_fidl() })
                .collect(),
        ),
        ..Default::default()
    }
}

fn from_fidl_dict_btree(dict: fdata::Dictionary) -> BTreeMap<String, DictionaryValue> {
    match dict.entries {
        Some(entries) => entries.into_iter().map(|e| (e.key, e.value.fidl_into_native())).collect(),
        _ => BTreeMap::new(),
    }
}

fn to_fidl_dict_btree(dict: BTreeMap<String, DictionaryValue>) -> fdata::Dictionary {
    fdata::Dictionary {
        entries: Some(
            dict.into_iter()
                .map(|(key, value)| fdata::DictionaryEntry { key, value: value.native_into_fidl() })
                .collect(),
        ),
        ..Default::default()
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UseSource {
    Parent,
    Framework,
    Debug,
    Self_,
    Capability(Name),
    Child(Name),
    Collection(Name),
    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    Environment,
}

impl std::fmt::Display for UseSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framework => write!(f, "framework"),
            Self::Parent => write!(f, "parent"),
            Self::Debug => write!(f, "debug environment"),
            Self::Self_ => write!(f, "self"),
            Self::Capability(c) => write!(f, "capability `{}`", c),
            Self::Child(c) => write!(f, "child `#{}`", c),
            Self::Collection(c) => write!(f, "collection `#{}`", c),
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            Self::Environment => write!(f, "environment"),
        }
    }
}

impl FidlIntoNative<UseSource> for fdecl::Ref {
    fn fidl_into_native(self) -> UseSource {
        match self {
            fdecl::Ref::Parent(_) => UseSource::Parent,
            fdecl::Ref::Framework(_) => UseSource::Framework,
            fdecl::Ref::Debug(_) => UseSource::Debug,
            fdecl::Ref::Self_(_) => UseSource::Self_,
            // cm_fidl_validator should have already validated this
            fdecl::Ref::Capability(c) => UseSource::Capability(c.name.parse().unwrap()),
            fdecl::Ref::Child(c) => UseSource::Child(c.name.parse().unwrap()),
            fdecl::Ref::Collection(c) => UseSource::Collection(c.name.parse().unwrap()),
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            fdecl::Ref::Environment(_) => UseSource::Environment,
            _ => panic!("invalid UseSource variant"),
        }
    }
}

impl NativeIntoFidl<fdecl::Ref> for UseSource {
    fn native_into_fidl(self) -> fdecl::Ref {
        match self {
            UseSource::Parent => fdecl::Ref::Parent(fdecl::ParentRef {}),
            UseSource::Framework => fdecl::Ref::Framework(fdecl::FrameworkRef {}),
            UseSource::Debug => fdecl::Ref::Debug(fdecl::DebugRef {}),
            UseSource::Self_ => fdecl::Ref::Self_(fdecl::SelfRef {}),
            UseSource::Capability(name) => {
                fdecl::Ref::Capability(fdecl::CapabilityRef { name: name.to_string() })
            }
            UseSource::Child(name) => {
                fdecl::Ref::Child(fdecl::ChildRef { name: name.to_string(), collection: None })
            }
            UseSource::Collection(name) => {
                fdecl::Ref::Collection(fdecl::CollectionRef { name: name.to_string() })
            }
            #[cfg(fuchsia_api_level_at_least = "HEAD")]
            UseSource::Environment => fdecl::Ref::Environment(fdecl::EnvironmentRef {}),
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventScope {
    Child(ChildRef),
    Collection(Name),
}

impl FidlIntoNative<EventScope> for fdecl::Ref {
    fn fidl_into_native(self) -> EventScope {
        match self {
            fdecl::Ref::Child(c) => {
                if let Some(_) = c.collection {
                    panic!("Dynamic children scopes are not supported for EventStreams");
                } else {
                    EventScope::Child(ChildRef { name: c.name.parse().unwrap(), collection: None })
                }
            }
            fdecl::Ref::Collection(collection) => {
                // cm_fidl_validator should have already validated this
                EventScope::Collection(collection.name.parse().unwrap())
            }
            _ => panic!("invalid EventScope variant"),
        }
    }
}

impl NativeIntoFidl<fdecl::Ref> for EventScope {
    fn native_into_fidl(self) -> fdecl::Ref {
        match self {
            EventScope::Child(child) => fdecl::Ref::Child(child.native_into_fidl()),
            EventScope::Collection(name) => {
                fdecl::Ref::Collection(fdecl::CollectionRef { name: name.native_into_fidl() })
            }
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferSource {
    Framework,
    Parent,
    Child(ChildRef),
    Collection(Name),
    Self_,
    Capability(Name),
    Void,
}

impl std::fmt::Display for OfferSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framework => write!(f, "framework"),
            Self::Parent => write!(f, "parent"),
            Self::Child(c) => write!(f, "child `#{}`", c),
            Self::Collection(c) => write!(f, "collection `#{}`", c),
            Self::Self_ => write!(f, "self"),
            Self::Capability(c) => write!(f, "capability `{}`", c),
            Self::Void => write!(f, "void"),
        }
    }
}

impl FidlIntoNative<OfferSource> for fdecl::Ref {
    fn fidl_into_native(self) -> OfferSource {
        match self {
            fdecl::Ref::Parent(_) => OfferSource::Parent,
            fdecl::Ref::Self_(_) => OfferSource::Self_,
            fdecl::Ref::Child(c) => OfferSource::Child(c.fidl_into_native()),
            // cm_fidl_validator should have already validated this
            fdecl::Ref::Collection(c) => OfferSource::Collection(c.name.parse().unwrap()),
            fdecl::Ref::Framework(_) => OfferSource::Framework,
            // cm_fidl_validator should have already validated this
            fdecl::Ref::Capability(c) => OfferSource::Capability(c.name.parse().unwrap()),
            fdecl::Ref::VoidType(_) => OfferSource::Void,
            _ => panic!("invalid OfferSource variant"),
        }
    }
}

impl NativeIntoFidl<fdecl::Ref> for OfferSource {
    fn native_into_fidl(self) -> fdecl::Ref {
        match self {
            OfferSource::Parent => fdecl::Ref::Parent(fdecl::ParentRef {}),
            OfferSource::Self_ => fdecl::Ref::Self_(fdecl::SelfRef {}),
            OfferSource::Child(c) => fdecl::Ref::Child(c.native_into_fidl()),
            OfferSource::Collection(name) => {
                fdecl::Ref::Collection(fdecl::CollectionRef { name: name.native_into_fidl() })
            }
            OfferSource::Framework => fdecl::Ref::Framework(fdecl::FrameworkRef {}),
            OfferSource::Capability(name) => {
                fdecl::Ref::Capability(fdecl::CapabilityRef { name: name.to_string() })
            }
            OfferSource::Void => fdecl::Ref::VoidType(fdecl::VoidRef {}),
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExposeSource {
    Self_,
    Child(Name),
    Collection(Name),
    Framework,
    Capability(Name),
    Void,
}

impl std::fmt::Display for ExposeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framework => write!(f, "framework"),
            Self::Child(c) => write!(f, "child `#{}`", c),
            Self::Collection(c) => write!(f, "collection `#{}`", c),
            Self::Self_ => write!(f, "self"),
            Self::Capability(c) => write!(f, "capability `{}`", c),
            Self::Void => write!(f, "void"),
        }
    }
}

impl FidlIntoNative<ExposeSource> for fdecl::Ref {
    fn fidl_into_native(self) -> ExposeSource {
        match self {
            fdecl::Ref::Self_(_) => ExposeSource::Self_,
            // cm_fidl_validator should have already validated this
            fdecl::Ref::Child(c) => ExposeSource::Child(c.name.parse().unwrap()),
            // cm_fidl_validator should have already validated this
            fdecl::Ref::Collection(c) => ExposeSource::Collection(c.name.parse().unwrap()),
            fdecl::Ref::Framework(_) => ExposeSource::Framework,
            // cm_fidl_validator should have already validated this
            fdecl::Ref::Capability(c) => ExposeSource::Capability(c.name.parse().unwrap()),
            fdecl::Ref::VoidType(_) => ExposeSource::Void,
            _ => panic!("invalid ExposeSource variant"),
        }
    }
}

impl NativeIntoFidl<fdecl::Ref> for ExposeSource {
    fn native_into_fidl(self) -> fdecl::Ref {
        match self {
            ExposeSource::Self_ => fdecl::Ref::Self_(fdecl::SelfRef {}),
            ExposeSource::Child(name) => fdecl::Ref::Child(fdecl::ChildRef {
                name: name.native_into_fidl(),
                collection: None,
            }),
            ExposeSource::Collection(name) => {
                fdecl::Ref::Collection(fdecl::CollectionRef { name: name.native_into_fidl() })
            }
            ExposeSource::Framework => fdecl::Ref::Framework(fdecl::FrameworkRef {}),
            ExposeSource::Capability(name) => {
                fdecl::Ref::Capability(fdecl::CapabilityRef { name: name.to_string() })
            }
            ExposeSource::Void => fdecl::Ref::VoidType(fdecl::VoidRef {}),
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExposeTarget {
    Parent,
    Framework,
}

impl std::fmt::Display for ExposeTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framework => write!(f, "framework"),
            Self::Parent => write!(f, "parent"),
        }
    }
}

impl FidlIntoNative<ExposeTarget> for fdecl::Ref {
    fn fidl_into_native(self) -> ExposeTarget {
        match self {
            fdecl::Ref::Parent(_) => ExposeTarget::Parent,
            fdecl::Ref::Framework(_) => ExposeTarget::Framework,
            _ => panic!("invalid ExposeTarget variant"),
        }
    }
}

impl NativeIntoFidl<fdecl::Ref> for ExposeTarget {
    fn native_into_fidl(self) -> fdecl::Ref {
        match self {
            ExposeTarget::Parent => fdecl::Ref::Parent(fdecl::ParentRef {}),
            ExposeTarget::Framework => fdecl::Ref::Framework(fdecl::FrameworkRef {}),
        }
    }
}

/// A source for a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSource<T> {
    /// The provider of the service, relative to a component.
    pub source: T,
    /// The name of the service.
    pub source_name: Name,
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageDirectorySource {
    Parent,
    Self_,
    Child(String),
}

impl FidlIntoNative<StorageDirectorySource> for fdecl::Ref {
    fn fidl_into_native(self) -> StorageDirectorySource {
        match self {
            fdecl::Ref::Parent(_) => StorageDirectorySource::Parent,
            fdecl::Ref::Self_(_) => StorageDirectorySource::Self_,
            fdecl::Ref::Child(c) => StorageDirectorySource::Child(c.name),
            _ => panic!("invalid OfferDirectorySource variant"),
        }
    }
}

impl NativeIntoFidl<fdecl::Ref> for StorageDirectorySource {
    fn native_into_fidl(self) -> fdecl::Ref {
        match self {
            StorageDirectorySource::Parent => fdecl::Ref::Parent(fdecl::ParentRef {}),
            StorageDirectorySource::Self_ => fdecl::Ref::Self_(fdecl::SelfRef {}),
            StorageDirectorySource::Child(child_name) => {
                fdecl::Ref::Child(fdecl::ChildRef { name: child_name, collection: None })
            }
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictionarySource {
    Parent,
    Self_,
    Child(ChildRef),
}

impl FidlIntoNative<DictionarySource> for fdecl::Ref {
    fn fidl_into_native(self) -> DictionarySource {
        match self {
            Self::Parent(_) => DictionarySource::Parent,
            Self::Self_(_) => DictionarySource::Self_,
            Self::Child(c) => DictionarySource::Child(c.fidl_into_native()),
            _ => panic!("invalid DictionarySource variant"),
        }
    }
}

impl NativeIntoFidl<fdecl::Ref> for DictionarySource {
    fn native_into_fidl(self) -> fdecl::Ref {
        match self {
            Self::Parent => fdecl::Ref::Parent(fdecl::ParentRef {}),
            Self::Self_ => fdecl::Ref::Self_(fdecl::SelfRef {}),
            Self::Child(c) => fdecl::Ref::Child(c.native_into_fidl()),
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationSource {
    Parent,
    Self_,
    Child(String),
}

impl FidlIntoNative<RegistrationSource> for fdecl::Ref {
    fn fidl_into_native(self) -> RegistrationSource {
        match self {
            fdecl::Ref::Parent(_) => RegistrationSource::Parent,
            fdecl::Ref::Self_(_) => RegistrationSource::Self_,
            fdecl::Ref::Child(c) => RegistrationSource::Child(c.name),
            _ => panic!("invalid RegistrationSource variant"),
        }
    }
}

impl NativeIntoFidl<fdecl::Ref> for RegistrationSource {
    fn native_into_fidl(self) -> fdecl::Ref {
        match self {
            RegistrationSource::Parent => fdecl::Ref::Parent(fdecl::ParentRef {}),
            RegistrationSource::Self_ => fdecl::Ref::Self_(fdecl::SelfRef {}),
            RegistrationSource::Child(child_name) => {
                fdecl::Ref::Child(fdecl::ChildRef { name: child_name, collection: None })
            }
        }
    }
}

#[cfg_attr(feature = "serde", derive(Deserialize, Serialize), serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OfferTarget {
    Child(ChildRef),
    Collection(Name),
    Capability(Name),
}

impl std::fmt::Display for OfferTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Child(c) => write!(f, "child `#{}`", c),
            Self::Collection(c) => write!(f, "collection `#{}`", c),
            Self::Capability(c) => write!(f, "capability `#{}`", c),
        }
    }
}

impl FidlIntoNative<OfferTarget> for fdecl::Ref {
    fn fidl_into_native(self) -> OfferTarget {
        match self {
            fdecl::Ref::Child(c) => OfferTarget::Child(c.fidl_into_native()),
            // cm_fidl_validator should have already validated this
            fdecl::Ref::Collection(c) => OfferTarget::Collection(c.name.parse().unwrap()),
            fdecl::Ref::Capability(c) => OfferTarget::Capability(c.name.parse().unwrap()),
            _ => panic!("invalid OfferTarget variant"),
        }
    }
}

impl NativeIntoFidl<fdecl::Ref> for OfferTarget {
    fn native_into_fidl(self) -> fdecl::Ref {
        match self {
            OfferTarget::Child(c) => fdecl::Ref::Child(c.native_into_fidl()),
            OfferTarget::Collection(collection_name) => {
                fdecl::Ref::Collection(fdecl::CollectionRef {
                    name: collection_name.native_into_fidl(),
                })
            }
            OfferTarget::Capability(capability_name) => {
                fdecl::Ref::Capability(fdecl::CapabilityRef {
                    name: capability_name.native_into_fidl(),
                })
            }
        }
    }
}

/// Converts the contents of a CM-FIDL declaration and produces the equivalent CM-Rust
/// struct.
/// This function applies cm_fidl_validator to check correctness.
impl TryFrom<fdecl::Component> for ComponentDecl {
    type Error = Error;

    fn try_from(decl: fdecl::Component) -> Result<Self, Self::Error> {
        cm_fidl_validator::validate(&decl).map_err(|err| Error::Validate { err })?;
        Ok(decl.fidl_into_native())
    }
}

// Converts the contents of a CM-Rust declaration into a CM_FIDL declaration
impl From<ComponentDecl> for fdecl::Component {
    fn from(decl: ComponentDecl) -> Self {
        decl.native_into_fidl()
    }
}

/// Errors produced by cm_rust.
#[derive(Debug, Error, Clone)]
pub enum Error {
    #[error("Fidl validation failed: {}", err)]
    Validate {
        #[source]
        err: cm_fidl_validator::error::ErrorList,
    },
    #[error("Invalid capability path: {}", raw)]
    InvalidCapabilityPath { raw: String },
    #[error("Invalid capability type name: {}", raw)]
    ParseCapabilityTypeName { raw: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use difference::Changeset;
    use fidl_fuchsia_component_decl as fdecl;

    fn offer_source_static_child(name: &str) -> OfferSource {
        OfferSource::Child(ChildRef { name: name.parse().unwrap(), collection: None })
    }

    fn offer_target_static_child(name: &str) -> OfferTarget {
        OfferTarget::Child(ChildRef { name: name.parse().unwrap(), collection: None })
    }

    macro_rules! test_try_from_decl {
        (
            $(
                $test_name:ident => {
                    input = $input:expr,
                    result = $result:expr,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    {
                        let res = ComponentDecl::try_from($input).expect("try_from failed");
                        if res != $result {
                            let a = format!("{:#?}", res);
                            let e = format!("{:#?}", $result);
                            panic!("Conversion from fidl to cm_rust did not yield expected result:\n{}", Changeset::new(&a, &e, "\n"));
                        }
                    }
                    {
                        let res = fdecl::Component::try_from($result).expect("try_from failed");
                        if res != $input {
                            let a = format!("{:#?}", res);
                            let e = format!("{:#?}", $input);
                            panic!("Conversion from cm_rust to fidl did not yield expected result:\n{}", Changeset::new(&a, &e, "\n"));
                        }
                    }
                }
            )+
        }
    }

    macro_rules! test_fidl_into_and_from {
        (
            $(
                $test_name:ident => {
                    input = $input:expr,
                    input_type = $input_type:ty,
                    result = $result:expr,
                    result_type = $result_type:ty,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    {
                        let res: Vec<$result_type> =
                            $input.into_iter().map(|e| e.fidl_into_native()).collect();
                        assert_eq!(res, $result);
                    }
                    {
                        let res: Vec<$input_type> =
                            $result.into_iter().map(|e| e.native_into_fidl()).collect();
                        assert_eq!(res, $input);
                    }
                }
            )+
        }
    }

    macro_rules! test_fidl_into {
        (
            $(
                $test_name:ident => {
                    input = $input:expr,
                    result = $result:expr,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    test_fidl_into_helper($input, $result);
                }
            )+
        }
    }

    fn test_fidl_into_helper<T, U>(input: T, expected_res: U)
    where
        T: FidlIntoNative<U>,
        U: std::cmp::PartialEq + std::fmt::Debug,
    {
        let res: U = input.fidl_into_native();
        assert_eq!(res, expected_res);
    }

    test_try_from_decl! {
        try_from_empty => {
            input = fdecl::Component {
                program: None,
                uses: None,
                exposes: None,
                offers: None,
                capabilities: None,
                children: None,
                collections: None,
                facets: None,
                environments: None,
                ..Default::default()
            },
            result = ComponentDecl {
                program: None,
                uses: vec![],
                exposes: vec![],
                offers: vec![],
                capabilities: vec![],
                children: vec![],
                collections: vec![],
                facets: None,
                environments: vec![],
                config: None,
            },
        },
        try_from_all => {
            input = fdecl::Component {
                program: Some(fdecl::Program {
                    runner: Some("elf".to_string()),
                    info: Some(fdata::Dictionary {
                        entries: Some(vec![
                            fdata::DictionaryEntry {
                                key: "args".to_string(),
                                value: Some(Box::new(fdata::DictionaryValue::StrVec(vec!["foo".to_string(), "bar".to_string()]))),
                            },
                            fdata::DictionaryEntry {
                                key: "binary".to_string(),
                                value: Some(Box::new(fdata::DictionaryValue::Str("bin/app".to_string()))),
                            },
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                uses: Some(vec![
                    fdecl::Use::Service(fdecl::UseService {
                        dependency_type: Some(fdecl::DependencyType::Strong),
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        source_name: Some("netstack".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target_path: Some("/svc/mynetstack".to_string()),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                    fdecl::Use::Protocol(fdecl::UseProtocol {
                        dependency_type: Some(fdecl::DependencyType::Strong),
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        source_name: Some("legacy_netstack".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target_path: Some("/svc/legacy_mynetstack".to_string()),
                        availability: Some(fdecl::Availability::Optional),
                        ..Default::default()
                    }),
                    fdecl::Use::Protocol(fdecl::UseProtocol {
                        dependency_type: Some(fdecl::DependencyType::Strong),
                        source: Some(fdecl::Ref::Child(fdecl::ChildRef { name: "echo".to_string(), collection: None})),
                        source_name: Some("echo_service".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target_path: Some("/svc/echo_service".to_string()),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                    fdecl::Use::Directory(fdecl::UseDirectory {
                        dependency_type: Some(fdecl::DependencyType::Strong),
                        source: Some(fdecl::Ref::Self_(fdecl::SelfRef {})),
                        source_name: Some("dir".to_string()),
                        source_dictionary: Some("dict1/me".to_string()),
                        target_path: Some("/data".to_string()),
                        rights: Some(fio::Operations::CONNECT),
                        subdir: Some("foo/bar".to_string()),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                    fdecl::Use::Storage(fdecl::UseStorage {
                        source_name: Some("cache".to_string()),
                        target_path: Some("/cache".to_string()),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                    fdecl::Use::Storage(fdecl::UseStorage {
                        source_name: Some("temp".to_string()),
                        target_path: Some("/temp".to_string()),
                        availability: Some(fdecl::Availability::Optional),
                        ..Default::default()
                    }),
                    fdecl::Use::EventStream(fdecl::UseEventStream {
                        source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                            collection: None,
                            name: "netstack".to_string(),
                        })),
                        source_name: Some("stopped".to_string()),
                        scope: Some(vec![
                            fdecl::Ref::Child(fdecl::ChildRef {
                                collection: None,
                                name:"a".to_string(),
                        }), fdecl::Ref::Collection(fdecl::CollectionRef {
                            name:"b".to_string(),
                        })]),
                        target_path: Some("/svc/test".to_string()),
                        availability: Some(fdecl::Availability::Optional),
                        ..Default::default()
                    }),
                    fdecl::Use::Runner(fdecl::UseRunner {
                        source: Some(fdecl::Ref::Environment(fdecl::EnvironmentRef {})),
                        source_name: Some("elf".to_string()),
                        source_dictionary: None,
                        ..Default::default()
                    }),
                    fdecl::Use::Config(fdecl::UseConfiguration {
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef)),
                        source_name: Some("fuchsia.config.MyConfig".to_string()),
                        target_name: Some("my_config".to_string()),
                        availability: Some(fdecl::Availability::Required),
                        type_: Some(fdecl::ConfigType{
                            layout: fdecl::ConfigTypeLayout::Bool,
                            parameters: Some(Vec::new()),
                            constraints: Vec::new(),
                        }),
                        ..Default::default()
                    }),
                ]),
                exposes: Some(vec![
                    fdecl::Expose::Protocol(fdecl::ExposeProtocol {
                        source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("legacy_netstack".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target_name: Some("legacy_mynetstack".to_string()),
                        target: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                    fdecl::Expose::Directory(fdecl::ExposeDirectory {
                        source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("dir".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target_name: Some("data".to_string()),
                        target: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        rights: Some(fio::Operations::CONNECT),
                        subdir: Some("foo/bar".to_string()),
                        availability: Some(fdecl::Availability::Optional),
                        ..Default::default()
                    }),
                    fdecl::Expose::Runner(fdecl::ExposeRunner {
                        source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("elf".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        target_name: Some("elf".to_string()),
                        ..Default::default()
                    }),
                    fdecl::Expose::Resolver(fdecl::ExposeResolver{
                        source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("pkg".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target: Some(fdecl::Ref::Parent(fdecl::ParentRef{})),
                        target_name: Some("pkg".to_string()),
                        ..Default::default()
                    }),
                    fdecl::Expose::Service(fdecl::ExposeService {
                        source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("netstack1".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target_name: Some("mynetstack".to_string()),
                        target: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                    fdecl::Expose::Service(fdecl::ExposeService {
                        source: Some(fdecl::Ref::Collection(fdecl::CollectionRef {
                            name: "modular".to_string(),
                        })),
                        source_name: Some("netstack2".to_string()),
                        source_dictionary: None,
                        target_name: Some("mynetstack".to_string()),
                        target: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                    fdecl::Expose::Dictionary(fdecl::ExposeDictionary {
                        source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("bundle".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target_name: Some("mybundle".to_string()),
                        target: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                ]),
                offers: Some(vec![
                    fdecl::Offer::Protocol(fdecl::OfferProtocol {
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        source_name: Some("legacy_netstack".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target: Some(fdecl::Ref::Child(
                           fdecl::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("legacy_mynetstack".to_string()),
                        dependency_type: Some(fdecl::DependencyType::Weak),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                    fdecl::Offer::Directory(fdecl::OfferDirectory {
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        source_name: Some("dir".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target: Some(fdecl::Ref::Collection(
                            fdecl::CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("data".to_string()),
                        rights: Some(fio::Operations::CONNECT),
                        subdir: None,
                        dependency_type: Some(fdecl::DependencyType::Strong),
                        availability: Some(fdecl::Availability::Optional),
                        ..Default::default()
                    }),
                    fdecl::Offer::Storage(fdecl::OfferStorage {
                        source_name: Some("cache".to_string()),
                        source: Some(fdecl::Ref::Self_(fdecl::SelfRef {})),
                        target: Some(fdecl::Ref::Collection(
                            fdecl::CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("cache".to_string()),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                    fdecl::Offer::Runner(fdecl::OfferRunner {
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        source_name: Some("elf".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target: Some(fdecl::Ref::Child(
                           fdecl::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("elf2".to_string()),
                        ..Default::default()
                    }),
                    fdecl::Offer::Resolver(fdecl::OfferResolver{
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef{})),
                        source_name: Some("pkg".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target: Some(fdecl::Ref::Child(
                           fdecl::ChildRef {
                              name: "echo".to_string(),
                              collection: None,
                           }
                        )),
                        target_name: Some("pkg".to_string()),
                        ..Default::default()
                    }),
                    fdecl::Offer::Service(fdecl::OfferService {
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        source_name: Some("netstack1".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target: Some(fdecl::Ref::Child(
                           fdecl::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("mynetstack1".to_string()),
                        availability: Some(fdecl::Availability::Required),
                        dependency_type: Some(fdecl::DependencyType::Strong),
                        ..Default::default()
                    }),
                    fdecl::Offer::Service(fdecl::OfferService {
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        source_name: Some("netstack2".to_string()),
                        source_dictionary: None,
                        target: Some(fdecl::Ref::Child(
                           fdecl::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("mynetstack2".to_string()),
                        availability: Some(fdecl::Availability::Optional),
                        dependency_type: Some(fdecl::DependencyType::Strong),
                        ..Default::default()
                    }),
                    fdecl::Offer::Service(fdecl::OfferService {
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        source_name: Some("netstack3".to_string()),
                        source_dictionary: None,
                        target: Some(fdecl::Ref::Child(
                           fdecl::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("mynetstack3".to_string()),
                        source_instance_filter: Some(vec!["allowedinstance".to_string()]),
                        renamed_instances: Some(vec![fdecl::NameMapping{source_name: "default".to_string(), target_name: "allowedinstance".to_string()}]),
                        availability: Some(fdecl::Availability::Required),
                        dependency_type: Some(fdecl::DependencyType::Strong),
                        ..Default::default()
                    }),
                    fdecl::Offer::Dictionary(fdecl::OfferDictionary {
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        source_name: Some("bundle".to_string()),
                        source_dictionary: Some("in/dict".to_string()),
                        target: Some(fdecl::Ref::Child(
                           fdecl::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("mybundle".to_string()),
                        dependency_type: Some(fdecl::DependencyType::Weak),
                        availability: Some(fdecl::Availability::Required),
                        ..Default::default()
                    }),
                ]),
                capabilities: Some(vec![
                    fdecl::Capability::Service(fdecl::Service {
                        name: Some("netstack".to_string()),
                        source_path: Some("/netstack".to_string()),
                        ..Default::default()
                    }),
                    fdecl::Capability::Protocol(fdecl::Protocol {
                        name: Some("netstack2".to_string()),
                        source_path: Some("/netstack2".to_string()),
                        delivery: Some(fdecl::DeliveryType::Immediate),
                        ..Default::default()
                    }),
                    fdecl::Capability::Directory(fdecl::Directory {
                        name: Some("data".to_string()),
                        source_path: Some("/data".to_string()),
                        rights: Some(fio::Operations::CONNECT),
                        ..Default::default()
                    }),
                    fdecl::Capability::Storage(fdecl::Storage {
                        name: Some("cache".to_string()),
                        backing_dir: Some("data".to_string()),
                        source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
                        subdir: Some("cache".to_string()),
                        storage_id: Some(fdecl::StorageId::StaticInstanceId),
                        ..Default::default()
                    }),
                    fdecl::Capability::Runner(fdecl::Runner {
                        name: Some("elf".to_string()),
                        source_path: Some("/elf".to_string()),
                        ..Default::default()
                    }),
                    fdecl::Capability::Resolver(fdecl::Resolver {
                        name: Some("pkg".to_string()),
                        source_path: Some("/pkg_resolver".to_string()),
                        ..Default::default()
                    }),
                    fdecl::Capability::Dictionary(fdecl::Dictionary {
                        name: Some("dict1".to_string()),
                        ..Default::default()
                    }),
                    fdecl::Capability::Dictionary(fdecl::Dictionary {
                        name: Some("dict2".to_string()),
                        source_path: Some("/in/other".to_string()),
                        ..Default::default()
                    }),
                ]),
                children: Some(vec![
                     fdecl::Child {
                         name: Some("netstack".to_string()),
                         url: Some("fuchsia-pkg://fuchsia.com/netstack#meta/netstack.cm"
                                   .to_string()),
                         startup: Some(fdecl::StartupMode::Lazy),
                         on_terminate: None,
                         environment: None,
                         ..Default::default()
                     },
                     fdecl::Child {
                         name: Some("gtest".to_string()),
                         url: Some("fuchsia-pkg://fuchsia.com/gtest#meta/gtest.cm".to_string()),
                         startup: Some(fdecl::StartupMode::Lazy),
                         on_terminate: Some(fdecl::OnTerminate::None),
                         environment: None,
                         ..Default::default()
                     },
                     fdecl::Child {
                         name: Some("echo".to_string()),
                         url: Some("fuchsia-pkg://fuchsia.com/echo#meta/echo.cm"
                                   .to_string()),
                         startup: Some(fdecl::StartupMode::Eager),
                         on_terminate: Some(fdecl::OnTerminate::Reboot),
                         environment: Some("test_env".to_string()),
                         ..Default::default()
                     },
                ]),
                collections: Some(vec![
                     fdecl::Collection {
                         name: Some("modular".to_string()),
                         durability: Some(fdecl::Durability::Transient),
                         environment: None,
                         allowed_offers: Some(fdecl::AllowedOffers::StaticOnly),
                         allow_long_names: Some(true),
                         persistent_storage: None,
                         ..Default::default()
                     },
                     fdecl::Collection {
                         name: Some("tests".to_string()),
                         durability: Some(fdecl::Durability::Transient),
                         environment: Some("test_env".to_string()),
                         allowed_offers: Some(fdecl::AllowedOffers::StaticAndDynamic),
                         allow_long_names: Some(true),
                         persistent_storage: Some(true),
                         ..Default::default()
                     },
                ]),
                facets: Some(fdata::Dictionary {
                    entries: Some(vec![
                        fdata::DictionaryEntry {
                            key: "author".to_string(),
                            value: Some(Box::new(fdata::DictionaryValue::Str("Fuchsia".to_string()))),
                        },
                    ]),
                    ..Default::default()
                }),
                environments: Some(vec![
                    fdecl::Environment {
                        name: Some("test_env".to_string()),
                        extends: Some(fdecl::EnvironmentExtends::Realm),
                        runners: Some(vec![
                            fdecl::RunnerRegistration {
                                source_name: Some("runner".to_string()),
                                source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                                    name: "gtest".to_string(),
                                    collection: None,
                                })),
                                target_name: Some("gtest-runner".to_string()),
                                ..Default::default()
                            }
                        ]),
                        resolvers: Some(vec![
                            fdecl::ResolverRegistration {
                                resolver: Some("pkg_resolver".to_string()),
                                source: Some(fdecl::Ref::Parent(fdecl::ParentRef{})),
                                scheme: Some("fuchsia-pkg".to_string()),
                                ..Default::default()
                            }
                        ]),
                        debug_capabilities: Some(vec![
                         fdecl::DebugRegistration::Protocol(fdecl::DebugProtocolRegistration {
                             source_name: Some("some_protocol".to_string()),
                             source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                                 name: "gtest".to_string(),
                                 collection: None,
                             })),
                             target_name: Some("some_protocol".to_string()),
                             ..Default::default()
                            })
                        ]),
                        stop_timeout_ms: Some(4567),
                        ..Default::default()
                    }
                ]),
                config: Some(fdecl::ConfigSchema{
                    fields: Some(vec![
                        fdecl::ConfigField {
                            key: Some("enable_logging".to_string()),
                            type_: Some(fdecl::ConfigType {
                                layout: fdecl::ConfigTypeLayout::Bool,
                                parameters: Some(vec![]),
                                constraints: vec![],
                            }),
                            mutability: Some(Default::default()),
                            ..Default::default()
                        }
                    ]),
                    checksum: Some(fdecl::ConfigChecksum::Sha256([
                        0x64, 0x49, 0x9E, 0x75, 0xF3, 0x37, 0x69, 0x88, 0x74, 0x3B, 0x38, 0x16,
                        0xCD, 0x14, 0x70, 0x9F, 0x3D, 0x4A, 0xD3, 0xE2, 0x24, 0x9A, 0x1A, 0x34,
                        0x80, 0xB4, 0x9E, 0xB9, 0x63, 0x57, 0xD6, 0xED,
                    ])),
                    value_source: Some(
                        fdecl::ConfigValueSource::PackagePath("fake.cvf".to_string())
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            },
            result = {
                ComponentDecl {
                    program: Some(ProgramDecl {
                        runner: Some("elf".parse().unwrap()),
                        info: fdata::Dictionary {
                            entries: Some(vec![
                                fdata::DictionaryEntry {
                                    key: "args".to_string(),
                                    value: Some(Box::new(fdata::DictionaryValue::StrVec(vec!["foo".to_string(), "bar".to_string()]))),
                                },
                                fdata::DictionaryEntry{
                                    key: "binary".to_string(),
                                    value: Some(Box::new(fdata::DictionaryValue::Str("bin/app".to_string()))),
                                },
                            ]),
                            ..Default::default()
                        },
                    }),
                    uses: vec![
                        UseDecl::Service(UseServiceDecl {
                            dependency_type: DependencyType::Strong,
                            source: UseSource::Parent,
                            source_name: "netstack".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target_path: "/svc/mynetstack".parse().unwrap(),
                            availability: Availability::Required,
                        }),
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: DependencyType::Strong,
                            source: UseSource::Parent,
                            source_name: "legacy_netstack".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target_path: "/svc/legacy_mynetstack".parse().unwrap(),
                            availability: Availability::Optional,
                        }),
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: DependencyType::Strong,
                            source: UseSource::Child("echo".parse().unwrap()),
                            source_name: "echo_service".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target_path: "/svc/echo_service".parse().unwrap(),
                            availability: Availability::Required,
                        }),
                        UseDecl::Directory(UseDirectoryDecl {
                            dependency_type: DependencyType::Strong,
                            source: UseSource::Self_,
                            source_name: "dir".parse().unwrap(),
                            source_dictionary: "dict1/me".parse().unwrap(),
                            target_path: "/data".parse().unwrap(),
                            rights: fio::Operations::CONNECT,
                            subdir: "foo/bar".parse().unwrap(),
                            availability: Availability::Required,
                        }),
                        UseDecl::Storage(UseStorageDecl {
                            source_name: "cache".parse().unwrap(),
                            target_path: "/cache".parse().unwrap(),
                            availability: Availability::Required,
                        }),
                        UseDecl::Storage(UseStorageDecl {
                            source_name: "temp".parse().unwrap(),
                            target_path: "/temp".parse().unwrap(),
                            availability: Availability::Optional,
                        }),
                        UseDecl::EventStream(UseEventStreamDecl {
                            source: UseSource::Child("netstack".parse().unwrap()),
                            scope: Some(vec![EventScope::Child(ChildRef{ name: "a".parse().unwrap(), collection: None}), EventScope::Collection("b".parse().unwrap())]),
                            source_name: "stopped".parse().unwrap(),
                            target_path: "/svc/test".parse().unwrap(),
                            filter: None,
                            availability: Availability::Optional,
                        }),
                        UseDecl::Runner(UseRunnerDecl {
                            source: UseSource::Environment,
                            source_name: "elf".parse().unwrap(),
                            source_dictionary: ".".parse().unwrap(),
                        }),
                        UseDecl::Config(UseConfigurationDecl {
                            source: UseSource::Parent,
                            source_name: "fuchsia.config.MyConfig".parse().unwrap(),
                            target_name: "my_config".parse().unwrap(),
                            availability: Availability::Required,
                            type_: ConfigValueType::Bool,
                            default: None,
                            source_dictionary: ".".parse().unwrap(),
                        }),
                    ],
                    exposes: vec![
                        ExposeDecl::Protocol(ExposeProtocolDecl {
                            source: ExposeSource::Child("netstack".parse().unwrap()),
                            source_name: "legacy_netstack".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target_name: "legacy_mynetstack".parse().unwrap(),
                            target: ExposeTarget::Parent,
                            availability: Availability::Required,
                        }),
                        ExposeDecl::Directory(ExposeDirectoryDecl {
                            source: ExposeSource::Child("netstack".parse().unwrap()),
                            source_name: "dir".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target_name: "data".parse().unwrap(),
                            target: ExposeTarget::Parent,
                            rights: Some(fio::Operations::CONNECT),
                            subdir: "foo/bar".parse().unwrap(),
                            availability: Availability::Optional,
                        }),
                        ExposeDecl::Runner(ExposeRunnerDecl {
                            source: ExposeSource::Child("netstack".parse().unwrap()),
                            source_name: "elf".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target: ExposeTarget::Parent,
                            target_name: "elf".parse().unwrap(),
                        }),
                        ExposeDecl::Resolver(ExposeResolverDecl {
                            source: ExposeSource::Child("netstack".parse().unwrap()),
                            source_name: "pkg".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target: ExposeTarget::Parent,
                            target_name: "pkg".parse().unwrap(),
                        }),
                        ExposeDecl::Service(ExposeServiceDecl {
                            source: ExposeSource::Child("netstack".parse().unwrap()),
                            source_name: "netstack1".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target_name: "mynetstack".parse().unwrap(),
                            target: ExposeTarget::Parent,
                            availability: Availability::Required,
                        }),
                        ExposeDecl::Service(ExposeServiceDecl {
                            source: ExposeSource::Collection("modular".parse().unwrap()),
                            source_name: "netstack2".parse().unwrap(),
                            source_dictionary: ".".parse().unwrap(),
                            target_name: "mynetstack".parse().unwrap(),
                            target: ExposeTarget::Parent,
                            availability: Availability::Required,
                        }),
                        ExposeDecl::Dictionary(ExposeDictionaryDecl {
                            source: ExposeSource::Child("netstack".parse().unwrap()),
                            source_name: "bundle".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target_name: "mybundle".parse().unwrap(),
                            target: ExposeTarget::Parent,
                            availability: Availability::Required,
                        }),
                    ],
                    offers: vec![
                        OfferDecl::Protocol(OfferProtocolDecl {
                            source: OfferSource::Parent,
                            source_name: "legacy_netstack".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target: offer_target_static_child("echo"),
                            target_name: "legacy_mynetstack".parse().unwrap(),
                            dependency_type: DependencyType::Weak,
                            availability: Availability::Required,
                        }),
                        OfferDecl::Directory(OfferDirectoryDecl {
                            source: OfferSource::Parent,
                            source_name: "dir".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target: OfferTarget::Collection("modular".parse().unwrap()),
                            target_name: "data".parse().unwrap(),
                            rights: Some(fio::Operations::CONNECT),
                            subdir: ".".parse().unwrap(),
                            dependency_type: DependencyType::Strong,
                            availability: Availability::Optional,
                        }),
                        OfferDecl::Storage(OfferStorageDecl {
                            source_name: "cache".parse().unwrap(),
                            source: OfferSource::Self_,
                            target: OfferTarget::Collection("modular".parse().unwrap()),
                            target_name: "cache".parse().unwrap(),
                            availability: Availability::Required,
                        }),
                        OfferDecl::Runner(OfferRunnerDecl {
                            source: OfferSource::Parent,
                            source_name: "elf".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target: offer_target_static_child("echo"),
                            target_name: "elf2".parse().unwrap(),
                        }),
                        OfferDecl::Resolver(OfferResolverDecl {
                            source: OfferSource::Parent,
                            source_name: "pkg".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target: offer_target_static_child("echo"),
                            target_name: "pkg".parse().unwrap(),
                        }),
                        OfferDecl::Service(OfferServiceDecl {
                            source: OfferSource::Parent,
                            source_name: "netstack1".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            source_instance_filter: None,
                            renamed_instances: None,
                            target: offer_target_static_child("echo"),
                            target_name: "mynetstack1".parse().unwrap(),
                            availability: Availability::Required,
                            dependency_type: Default::default(),
                        }),
                        OfferDecl::Service(OfferServiceDecl {
                            source: OfferSource::Parent,
                            source_name: "netstack2".parse().unwrap(),
                            source_dictionary: ".".parse().unwrap(),
                            source_instance_filter: None,
                            renamed_instances: None,
                            target: offer_target_static_child("echo"),
                            target_name: "mynetstack2".parse().unwrap(),
                            availability: Availability::Optional,
                            dependency_type: Default::default(),
                        }),
                        OfferDecl::Service(OfferServiceDecl {
                            source: OfferSource::Parent,
                            source_name: "netstack3".parse().unwrap(),
                            source_dictionary: ".".parse().unwrap(),
                            source_instance_filter: Some(vec!["allowedinstance".parse().unwrap()]),
                            renamed_instances: Some(vec![NameMapping{source_name: "default".parse().unwrap(), target_name: "allowedinstance".parse().unwrap()}]),
                            target: offer_target_static_child("echo"),
                            target_name: "mynetstack3".parse().unwrap(),
                            availability: Availability::Required,
                            dependency_type: Default::default(),
                        }),
                        OfferDecl::Dictionary(OfferDictionaryDecl {
                            source: OfferSource::Parent,
                            source_name: "bundle".parse().unwrap(),
                            source_dictionary: "in/dict".parse().unwrap(),
                            target: offer_target_static_child("echo"),
                            target_name: "mybundle".parse().unwrap(),
                            dependency_type: DependencyType::Weak,
                            availability: Availability::Required,
                        }),
                    ],
                    capabilities: vec![
                        CapabilityDecl::Service(ServiceDecl {
                            name: "netstack".parse().unwrap(),
                            source_path: Some("/netstack".parse().unwrap()),
                        }),
                        CapabilityDecl::Protocol(ProtocolDecl {
                            name: "netstack2".parse().unwrap(),
                            source_path: Some("/netstack2".parse().unwrap()),
                            delivery: DeliveryType::Immediate,
                        }),
                        CapabilityDecl::Directory(DirectoryDecl {
                            name: "data".parse().unwrap(),
                            source_path: Some("/data".parse().unwrap()),
                            rights: fio::Operations::CONNECT,
                        }),
                        CapabilityDecl::Storage(StorageDecl {
                            name: "cache".parse().unwrap(),
                            backing_dir: "data".parse().unwrap(),
                            source: StorageDirectorySource::Parent,
                            subdir: "cache".parse().unwrap(),
                            storage_id: fdecl::StorageId::StaticInstanceId,
                        }),
                        CapabilityDecl::Runner(RunnerDecl {
                            name: "elf".parse().unwrap(),
                            source_path: Some("/elf".parse().unwrap()),
                        }),
                        CapabilityDecl::Resolver(ResolverDecl {
                            name: "pkg".parse().unwrap(),
                            source_path: Some("/pkg_resolver".parse().unwrap()),
                        }),
                        CapabilityDecl::Dictionary(DictionaryDecl {
                            name: "dict1".parse().unwrap(),
                            source_path: None,
                        }),
                        CapabilityDecl::Dictionary(DictionaryDecl {
                            name: "dict2".parse().unwrap(),
                            source_path: Some("/in/other".parse().unwrap()),
                        }),
                    ],
                    children: vec![
                        ChildDecl {
                            name: "netstack".parse().unwrap(),
                            url: "fuchsia-pkg://fuchsia.com/netstack#meta/netstack.cm".parse().unwrap(),
                            startup: fdecl::StartupMode::Lazy,
                            on_terminate: None,
                            environment: None,
                            config_overrides: None,
                        },
                        ChildDecl {
                            name: "gtest".parse().unwrap(),
                            url: "fuchsia-pkg://fuchsia.com/gtest#meta/gtest.cm".parse().unwrap(),
                            startup: fdecl::StartupMode::Lazy,
                            on_terminate: Some(fdecl::OnTerminate::None),
                            environment: None,
                            config_overrides: None,
                        },
                        ChildDecl {
                            name: "echo".parse().unwrap(),
                            url: "fuchsia-pkg://fuchsia.com/echo#meta/echo.cm".parse().unwrap(),
                            startup: fdecl::StartupMode::Eager,
                            on_terminate: Some(fdecl::OnTerminate::Reboot),
                            environment: Some("test_env".parse().unwrap()),
                            config_overrides: None,
                        },
                    ],
                    collections: vec![
                        CollectionDecl {
                            name: "modular".parse().unwrap(),
                            durability: fdecl::Durability::Transient,
                            environment: None,
                            allowed_offers: cm_types::AllowedOffers::StaticOnly,
                            allow_long_names: true,
                            persistent_storage: None,
                        },
                        CollectionDecl {
                            name: "tests".parse().unwrap(),
                            durability: fdecl::Durability::Transient,
                            environment: Some("test_env".parse().unwrap()),
                            allowed_offers: cm_types::AllowedOffers::StaticAndDynamic,
                            allow_long_names: true,
                            persistent_storage: Some(true),
                        },
                    ],
                    facets: Some(fdata::Dictionary {
                        entries: Some(vec![
                            fdata::DictionaryEntry {
                                key: "author".to_string(),
                                value: Some(Box::new(fdata::DictionaryValue::Str("Fuchsia".to_string()))),
                            },
                        ]),
                        ..Default::default()
                    }),
                    environments: vec![
                        EnvironmentDecl {
                            name: "test_env".parse().unwrap(),
                            extends: fdecl::EnvironmentExtends::Realm,
                            runners: vec![
                                RunnerRegistration {
                                    source_name: "runner".parse().unwrap(),
                                    source: RegistrationSource::Child("gtest".to_string()),
                                    target_name: "gtest-runner".parse().unwrap(),
                                }
                            ],
                            resolvers: vec![
                                ResolverRegistration {
                                    resolver: "pkg_resolver".parse().unwrap(),
                                    source: RegistrationSource::Parent,
                                    scheme: "fuchsia-pkg".to_string(),
                                }
                            ],
                            debug_capabilities: vec![
                                DebugRegistration::Protocol(DebugProtocolRegistration {
                                    source_name: "some_protocol".parse().unwrap(),
                                    source: RegistrationSource::Child("gtest".to_string()),
                                    target_name: "some_protocol".parse().unwrap(),
                                })
                            ],
                            stop_timeout_ms: Some(4567),
                        }
                    ],
                    config: Some(ConfigDecl {
                        fields: vec![
                            ConfigField {
                                key: "enable_logging".to_string(),
                                type_: ConfigValueType::Bool,
                                mutability: ConfigMutability::default(),
                            }
                        ],
                        checksum: ConfigChecksum::Sha256([
                            0x64, 0x49, 0x9E, 0x75, 0xF3, 0x37, 0x69, 0x88, 0x74, 0x3B, 0x38, 0x16,
                            0xCD, 0x14, 0x70, 0x9F, 0x3D, 0x4A, 0xD3, 0xE2, 0x24, 0x9A, 0x1A, 0x34,
                            0x80, 0xB4, 0x9E, 0xB9, 0x63, 0x57, 0xD6, 0xED,
                        ]),
                        value_source: ConfigValueSource::PackagePath("fake.cvf".to_string())
                    }),
                }
            },
        },
    }

    test_fidl_into_and_from! {
        fidl_into_and_from_use_source => {
            input = vec![
                fdecl::Ref::Parent(fdecl::ParentRef{}),
                fdecl::Ref::Framework(fdecl::FrameworkRef{}),
                fdecl::Ref::Debug(fdecl::DebugRef{}),
                fdecl::Ref::Capability(fdecl::CapabilityRef {name: "capability".to_string()}),
                fdecl::Ref::Child(fdecl::ChildRef {
                    name: "foo".into(),
                    collection: None,
                }),
                fdecl::Ref::Environment(fdecl::EnvironmentRef{}),
            ],
            input_type = fdecl::Ref,
            result = vec![
                UseSource::Parent,
                UseSource::Framework,
                UseSource::Debug,
                UseSource::Capability("capability".parse().unwrap()),
                UseSource::Child("foo".parse().unwrap()),
                UseSource::Environment,
            ],
            result_type = UseSource,
        },
        fidl_into_and_from_expose_source => {
            input = vec![
                fdecl::Ref::Self_(fdecl::SelfRef {}),
                fdecl::Ref::Child(fdecl::ChildRef {
                    name: "foo".into(),
                    collection: None,
                }),
                fdecl::Ref::Framework(fdecl::FrameworkRef {}),
                fdecl::Ref::Collection(fdecl::CollectionRef { name: "foo".to_string() }),
            ],
            input_type = fdecl::Ref,
            result = vec![
                ExposeSource::Self_,
                ExposeSource::Child("foo".parse().unwrap()),
                ExposeSource::Framework,
                ExposeSource::Collection("foo".parse().unwrap()),
            ],
            result_type = ExposeSource,
        },
        fidl_into_and_from_offer_source => {
            input = vec![
                fdecl::Ref::Self_(fdecl::SelfRef {}),
                fdecl::Ref::Child(fdecl::ChildRef {
                    name: "foo".into(),
                    collection: None,
                }),
                fdecl::Ref::Framework(fdecl::FrameworkRef {}),
                fdecl::Ref::Capability(fdecl::CapabilityRef { name: "foo".to_string() }),
                fdecl::Ref::Parent(fdecl::ParentRef {}),
                fdecl::Ref::Collection(fdecl::CollectionRef { name: "foo".to_string() }),
                fdecl::Ref::VoidType(fdecl::VoidRef {}),
            ],
            input_type = fdecl::Ref,
            result = vec![
                OfferSource::Self_,
                offer_source_static_child("foo"),
                OfferSource::Framework,
                OfferSource::Capability("foo".parse().unwrap()),
                OfferSource::Parent,
                OfferSource::Collection("foo".parse().unwrap()),
                OfferSource::Void,
            ],
            result_type = OfferSource,
        },
        fidl_into_and_from_dictionary_source => {
            input = vec![
                fdecl::Ref::Self_(fdecl::SelfRef {}),
                fdecl::Ref::Child(fdecl::ChildRef {
                    name: "foo".into(),
                    collection: None,
                }),
                fdecl::Ref::Parent(fdecl::ParentRef {}),
            ],
            input_type = fdecl::Ref,
            result = vec![
                DictionarySource::Self_,
                DictionarySource::Child(ChildRef {
                    name: "foo".parse().unwrap(),
                    collection: None,
                }),
                DictionarySource::Parent,
            ],
            result_type = DictionarySource,
        },

        fidl_into_and_from_capability_without_path => {
            input = vec![
                fdecl::Protocol {
                    name: Some("foo_protocol".to_string()),
                    source_path: None,
                    delivery: Some(fdecl::DeliveryType::Immediate),
                    ..Default::default()
                },
            ],
            input_type = fdecl::Protocol,
            result = vec![
                ProtocolDecl {
                    name: "foo_protocol".parse().unwrap(),
                    source_path: None,
                    delivery: DeliveryType::Immediate,
                }
            ],
            result_type = ProtocolDecl,
        },
        fidl_into_and_from_storage_capability => {
            input = vec![
                fdecl::Storage {
                    name: Some("minfs".to_string()),
                    backing_dir: Some("minfs".into()),
                    source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                        name: "foo".into(),
                        collection: None,
                    })),
                    subdir: None,
                    storage_id: Some(fdecl::StorageId::StaticInstanceIdOrMoniker),
                    ..Default::default()
                },
            ],
            input_type = fdecl::Storage,
            result = vec![
                StorageDecl {
                    name: "minfs".parse().unwrap(),
                    backing_dir: "minfs".parse().unwrap(),
                    source: StorageDirectorySource::Child("foo".to_string()),
                    subdir: ".".parse().unwrap(),
                    storage_id: fdecl::StorageId::StaticInstanceIdOrMoniker,
                },
            ],
            result_type = StorageDecl,
        },
        fidl_into_and_from_storage_capability_restricted => {
            input = vec![
                fdecl::Storage {
                    name: Some("minfs".to_string()),
                    backing_dir: Some("minfs".into()),
                    source: Some(fdecl::Ref::Child(fdecl::ChildRef {
                        name: "foo".into(),
                        collection: None,
                    })),
                    subdir: None,
                    storage_id: Some(fdecl::StorageId::StaticInstanceId),
                    ..Default::default()
                },
            ],
            input_type = fdecl::Storage,
            result = vec![
                StorageDecl {
                    name: "minfs".parse().unwrap(),
                    backing_dir: "minfs".parse().unwrap(),
                    source: StorageDirectorySource::Child("foo".to_string()),
                    subdir: ".".parse().unwrap(),
                    storage_id: fdecl::StorageId::StaticInstanceId,
                },
            ],
            result_type = StorageDecl,
        },
    }

    test_fidl_into! {
        all_with_omitted_defaults => {
            input = fdecl::Component {
                program: Some(fdecl::Program {
                    runner: Some("elf".to_string()),
                    info: Some(fdata::Dictionary {
                        entries: Some(vec![]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                uses: Some(vec![]),
                exposes: Some(vec![]),
                offers: Some(vec![]),
                capabilities: Some(vec![]),
                children: Some(vec![]),
                collections: Some(vec![
                     fdecl::Collection {
                         name: Some("modular".to_string()),
                         durability: Some(fdecl::Durability::Transient),
                         environment: None,
                         allowed_offers: None,
                         allow_long_names: None,
                         persistent_storage: None,
                         ..Default::default()
                     },
                     fdecl::Collection {
                         name: Some("tests".to_string()),
                         durability: Some(fdecl::Durability::Transient),
                         environment: Some("test_env".to_string()),
                         allowed_offers: Some(fdecl::AllowedOffers::StaticOnly),
                         allow_long_names: None,
                         persistent_storage: Some(false),
                         ..Default::default()
                     },
                     fdecl::Collection {
                         name: Some("dyn_offers".to_string()),
                         durability: Some(fdecl::Durability::Transient),
                         allowed_offers: Some(fdecl::AllowedOffers::StaticAndDynamic),
                         allow_long_names: None,
                         persistent_storage: Some(true),
                         ..Default::default()
                     },
                     fdecl::Collection {
                         name: Some("long_child_names".to_string()),
                         durability: Some(fdecl::Durability::Transient),
                         allowed_offers: None,
                         allow_long_names: Some(true),
                         persistent_storage: None,
                         ..Default::default()
                     },
                ]),
                facets: Some(fdata::Dictionary{
                    entries: Some(vec![]),
                    ..Default::default()
                }),
                environments: Some(vec![]),
                ..Default::default()
            },
            result = {
                ComponentDecl {
                    program: Some(ProgramDecl {
                        runner: Some("elf".parse().unwrap()),
                        info: fdata::Dictionary {
                            entries: Some(vec![]),
                            ..Default::default()
                        },
                    }),
                    uses: vec![],
                    exposes: vec![],
                    offers: vec![],
                    capabilities: vec![],
                    children: vec![],
                    collections: vec![
                        CollectionDecl {
                            name: "modular".parse().unwrap(),
                            durability: fdecl::Durability::Transient,
                            environment: None,
                            allowed_offers: cm_types::AllowedOffers::StaticOnly,
                            allow_long_names: false,
                            persistent_storage: None,
                        },
                        CollectionDecl {
                            name: "tests".parse().unwrap(),
                            durability: fdecl::Durability::Transient,
                            environment: Some("test_env".parse().unwrap()),
                            allowed_offers: cm_types::AllowedOffers::StaticOnly,
                            allow_long_names: false,
                            persistent_storage: Some(false),
                        },
                        CollectionDecl {
                            name: "dyn_offers".parse().unwrap(),
                            durability: fdecl::Durability::Transient,
                            environment: None,
                            allowed_offers: cm_types::AllowedOffers::StaticAndDynamic,
                            allow_long_names: false,
                            persistent_storage: Some(true),
                        },
                        CollectionDecl {
                            name: "long_child_names".parse().unwrap(),
                            durability: fdecl::Durability::Transient,
                            environment: None,
                            allowed_offers: cm_types::AllowedOffers::StaticOnly,
                            allow_long_names: true,
                            persistent_storage: None,
                        },
                    ],
                    facets: Some(fdata::Dictionary{
                        entries: Some(vec![]),
                        ..Default::default()
                    }),
                    environments: vec![],
                    config: None,
                }
            },
        },
    }

    #[test]
    fn default_expose_availability() {
        let source = fdecl::Ref::Self_(fdecl::SelfRef {});
        let source_name = "source";
        let target = fdecl::Ref::Parent(fdecl::ParentRef {});
        let target_name = "target";
        assert_eq!(
            *fdecl::ExposeService {
                source: Some(source.clone()),
                source_name: Some(source_name.into()),
                target: Some(target.clone()),
                target_name: Some(target_name.into()),
                availability: None,
                ..Default::default()
            }
            .fidl_into_native()
            .availability(),
            Availability::Required
        );
        assert_eq!(
            *fdecl::ExposeProtocol {
                source: Some(source.clone()),
                source_name: Some(source_name.into()),
                target: Some(target.clone()),
                target_name: Some(target_name.into()),
                ..Default::default()
            }
            .fidl_into_native()
            .availability(),
            Availability::Required
        );
        assert_eq!(
            *fdecl::ExposeDirectory {
                source: Some(source.clone()),
                source_name: Some(source_name.into()),
                target: Some(target.clone()),
                target_name: Some(target_name.into()),
                ..Default::default()
            }
            .fidl_into_native()
            .availability(),
            Availability::Required
        );
        assert_eq!(
            *fdecl::ExposeRunner {
                source: Some(source.clone()),
                source_name: Some(source_name.into()),
                target: Some(target.clone()),
                target_name: Some(target_name.into()),
                ..Default::default()
            }
            .fidl_into_native()
            .availability(),
            Availability::Required
        );
        assert_eq!(
            *fdecl::ExposeResolver {
                source: Some(source.clone()),
                source_name: Some(source_name.into()),
                target: Some(target.clone()),
                target_name: Some(target_name.into()),
                ..Default::default()
            }
            .fidl_into_native()
            .availability(),
            Availability::Required
        );
        assert_eq!(
            *fdecl::ExposeDictionary {
                source: Some(source.clone()),
                source_name: Some(source_name.into()),
                target: Some(target.clone()),
                target_name: Some(target_name.into()),
                ..Default::default()
            }
            .fidl_into_native()
            .availability(),
            Availability::Required
        );
    }

    #[test]
    fn default_delivery_type() {
        assert_eq!(
            fdecl::Protocol {
                name: Some("foo".to_string()),
                source_path: Some("/foo".to_string()),
                delivery: None,
                ..Default::default()
            }
            .fidl_into_native()
            .delivery,
            DeliveryType::Immediate
        )
    }

    #[test]
    fn on_readable_delivery_type() {
        assert_eq!(
            fdecl::Protocol {
                name: Some("foo".to_string()),
                source_path: Some("/foo".to_string()),
                delivery: Some(fdecl::DeliveryType::OnReadable),
                ..Default::default()
            }
            .fidl_into_native()
            .delivery,
            DeliveryType::OnReadable
        )
    }

    #[test]
    fn config_value_matches_type() {
        let bool_true = ConfigValue::Single(ConfigSingleValue::Bool(true));
        let bool_false = ConfigValue::Single(ConfigSingleValue::Bool(false));
        let uint8_zero = ConfigValue::Single(ConfigSingleValue::Uint8(0));
        let vec_bool_true = ConfigValue::Vector(ConfigVectorValue::BoolVector(vec![true]));
        let vec_bool_false = ConfigValue::Vector(ConfigVectorValue::BoolVector(vec![false]));

        assert!(bool_true.matches_type(&bool_false));
        assert!(vec_bool_true.matches_type(&vec_bool_false));

        assert!(!bool_true.matches_type(&uint8_zero));
        assert!(!bool_true.matches_type(&vec_bool_true));
    }
}

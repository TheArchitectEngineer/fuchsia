// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::component_instance::{ComponentInstanceForAnalyzer, TopInstanceForAnalyzer};
use crate::route::{TargetDecl, VerifyRouteResult};
use crate::{match_absolute_component_urls, PkgUrlMatch};
use anyhow::{anyhow, Context, Result};
use cm_config::RuntimeConfig;
use cm_rust::{
    CapabilityDecl, CapabilityTypeName, ComponentDecl, ExposeDecl, ExposeDeclCommon, OfferDecl,
    OfferDeclCommon, OfferStorageDecl, OfferTarget, ProgramDecl, ResolverRegistration, SourceName,
    UseDecl, UseDeclCommon, UseEventStreamDecl, UseRunnerDecl, UseSource, UseStorageDecl,
};
use cm_types::{Name, Url};
use config_encoder::ConfigFields;
use fidl::prelude::*;
use fuchsia_url::AbsoluteComponentUrl;
use futures::FutureExt;
use moniker::{ChildName, ExtendedMoniker, Moniker};
use router_error::Explain;
use routing::capability_source::{
    BuiltinSource, CapabilitySource, CapabilityToCapabilitySource, ComponentCapability,
    ComponentSource, InternalCapability,
};
use routing::component_instance::{
    ComponentInstanceInterface, ExtendedInstanceInterface, TopInstanceInterface,
};
use routing::environment::{find_first_absolute_ancestor_url, RunnerRegistry};
use routing::error::{ComponentInstanceError, RoutingError};
use routing::legacy_router::RouteBundle;
use routing::mapper::{RouteMapper, RouteSegment};
use routing::policy::GlobalPolicyChecker;
use routing::{route_capability, route_event_stream, RouteRequest, RouteSource};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use thiserror::Error;
use {fidl_fuchsia_sys2 as fsys, zx_status};

/// Errors that may occur when building a `ComponentModelForAnalyzer` from
/// a set of component manifests.
#[derive(Clone, Debug, Deserialize, Error, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildAnalyzerModelError {
    #[error("no component declaration found for url `{0}` requested by node `{1}`")]
    ComponentDeclNotFound(String, String),

    #[error("invalid child declaration containing url `{0}` at node `{1}`")]
    InvalidChildDecl(String, String),

    #[error("no node found with path `{0}`")]
    ComponentNodeNotFound(String),

    #[error("environment `{0}` requested by child `{1}` not found at node `{2}`")]
    EnvironmentNotFound(String, String, String),

    #[error("multiple resolvers found for scheme `{0}`")]
    DuplicateResolverScheme(String),

    #[error("malformed url {0} for component instance {1}")]
    MalformedUrl(String, String),

    #[error("dynamic component with url {0} an invalid moniker")]
    DynamicComponentInvalidMoniker(String),

    #[error("dynamic component at {0} with url {1} is not part of a collection")]
    DynamicComponentWithoutCollection(String, String),
}

/// Errors that a `ComponentModelForAnalyzer` may detect in the component graph.
#[derive(Clone, Debug, Error, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AnalyzerModelError {
    #[error("the source instance `{0}` is not executable")]
    SourceInstanceNotExecutable(Moniker),

    #[error(
        "at component {0} the capability `{1}` is not a valid source for the capability `{2}`"
    )]
    InvalidSourceCapability(ExtendedMoniker, String, String),

    #[error("no resolver found in environment of component `{0}` for scheme `{1}`")]
    MissingResolverForScheme(Moniker, String),

    #[error(transparent)]
    ComponentInstanceError(#[from] ComponentInstanceError),

    #[error(transparent)]
    RoutingError(#[from] RoutingError),
}

impl AnalyzerModelError {
    pub fn as_zx_status(&self) -> zx_status::Status {
        match self {
            Self::SourceInstanceNotExecutable(_) => zx_status::Status::NOT_FOUND,
            Self::InvalidSourceCapability(_, _, _) => zx_status::Status::NOT_FOUND,
            Self::MissingResolverForScheme(_, _) => zx_status::Status::NOT_FOUND,
            Self::ComponentInstanceError(err) => err.as_zx_status(),
            Self::RoutingError(err) => err.as_zx_status(),
        }
    }
}

impl From<AnalyzerModelError> for ExtendedMoniker {
    fn from(err: AnalyzerModelError) -> ExtendedMoniker {
        match err {
            AnalyzerModelError::InvalidSourceCapability(moniker, _, _) => moniker,
            AnalyzerModelError::MissingResolverForScheme(moniker, _)
            | AnalyzerModelError::SourceInstanceNotExecutable(moniker) => moniker.into(),
            AnalyzerModelError::ComponentInstanceError(err) => err.into(),
            AnalyzerModelError::RoutingError(err) => err.into(),
        }
    }
}

/// Builds a `ComponentModelForAnalyzer` from a set of component manifests.
pub struct ModelBuilderForAnalyzer {
    default_root_url: Url,
}

/// The type returned by `ModelBuilderForAnalyzer::build()`. May contain some
/// errors even if `model` is `Some`.
pub struct BuildModelResult {
    pub model: Option<Arc<ComponentModelForAnalyzer>>,
    pub errors: Vec<anyhow::Error>,
}

impl BuildModelResult {
    fn new() -> Self {
        Self { model: None, errors: Vec::new() }
    }
}

#[derive(Default)]
pub struct DynamicConfig {
    pub components: HashMap<Moniker, (AbsoluteComponentUrl, Option<Name>)>,
    pub dictionaries: DynamicDictionaryConfig,
}

pub type DynamicDictionaryConfig = HashMap<Moniker, HashMap<Name, Vec<(CapabilityTypeName, Name)>>>;

impl ModelBuilderForAnalyzer {
    pub fn new(default_root_url: Url) -> Self {
        Self { default_root_url }
    }

    fn load_dynamic_components(
        input: HashMap<Moniker, (AbsoluteComponentUrl, Option<Name>)>,
    ) -> (HashMap<Moniker, Vec<Child>>, Vec<anyhow::Error>) {
        let mut errors: Vec<anyhow::Error> = vec![];
        let mut dynamic_components: HashMap<Moniker, Vec<Child>> = HashMap::new();
        for (moniker, (url, environment)) in input.into_iter() {
            let Some((parent_moniker, child_moniker)) = moniker.split_leaf() else {
                errors.push(
                    BuildAnalyzerModelError::DynamicComponentInvalidMoniker(url.to_string()).into(),
                );
                continue;
            };
            if child_moniker.collection().is_none() {
                errors.push(
                    BuildAnalyzerModelError::DynamicComponentWithoutCollection(
                        moniker.to_string(),
                        url.to_string(),
                    )
                    .into(),
                );
                continue;
            }

            let children = dynamic_components.entry(parent_moniker).or_insert_with(|| vec![]);
            match Url::new(&url.to_string()) {
                Ok(url) => {
                    children.push(Child { child_moniker: child_moniker.into(), url, environment });
                }
                Err(_) => {
                    errors.push(
                        BuildAnalyzerModelError::MalformedUrl(url.to_string(), moniker.to_string())
                            .into(),
                    );
                }
            }
        }
        (dynamic_components, errors)
    }

    pub fn build(
        self,
        decls_by_url: HashMap<Url, (ComponentDecl, Option<ConfigFields>)>,
        runtime_config: Arc<RuntimeConfig>,
        component_id_index: Arc<component_id_index::Index>,
        runner_registry: RunnerRegistry,
    ) -> BuildModelResult {
        self.build_with_dynamic_config(
            DynamicConfig::default(),
            decls_by_url,
            runtime_config,
            component_id_index,
            runner_registry,
        )
    }

    pub fn build_with_dynamic_config(
        self,
        dynamic_config: DynamicConfig,
        decls_by_url: HashMap<Url, (ComponentDecl, Option<ConfigFields>)>,
        runtime_config: Arc<RuntimeConfig>,
        component_id_index: Arc<component_id_index::Index>,
        runner_registry: RunnerRegistry,
    ) -> BuildModelResult {
        let mut result = BuildModelResult::new();

        let (dynamic_components, mut dynamic_component_errors) =
            Self::load_dynamic_components(dynamic_config.components);
        result.errors.append(&mut dynamic_component_errors);

        let dynamic_dictionaries = Arc::new(dynamic_config.dictionaries);

        // Initialize the model with an empty `instances` map.
        let mut model = ComponentModelForAnalyzer {
            top_instance: TopInstanceForAnalyzer::new(
                runtime_config.namespace_capabilities.clone(),
                runtime_config.builtin_capabilities.clone(),
            ),
            instances: HashMap::new(),
            policy_checker: GlobalPolicyChecker::new(runtime_config.security_policy.clone()),
            component_id_index,
        };

        let root_url = runtime_config.root_component_url.as_ref().unwrap_or(&self.default_root_url);

        // If `root_url` matches a `ComponentDecl` in `decls_by_url`, construct the root
        // instance and then recursively add child instances to the model.
        match Self::get_decl_by_url(&decls_by_url, root_url) {
            Err(err) => {
                result.errors.push(err.context("Failed to parse root URL as fuchsia package URL"));
            }
            Ok(None) => {
                result.errors.push(anyhow!("Failed to locate root component with URL: {root_url}"));
            }
            Ok(Some((root_decl, root_config))) => {
                let root_instance = ComponentInstanceForAnalyzer::new_root(
                    root_decl.clone(),
                    root_config.clone(),
                    root_url.clone(),
                    Arc::clone(&model.top_instance),
                    Arc::clone(&runtime_config),
                    model.policy_checker.clone(),
                    Arc::clone(&model.component_id_index),
                    runner_registry,
                    Arc::clone(&dynamic_dictionaries),
                );

                Self::add_descendants(
                    &root_instance,
                    &decls_by_url,
                    &dynamic_components,
                    &dynamic_dictionaries,
                    &mut model,
                    &mut result,
                );

                model.instances.insert(root_instance.moniker().clone(), root_instance);

                result.model = Some(Arc::new(model));
            }
        }

        result
    }

    // Adds all descendants of `instance` to `model`, also inserting each new instance
    // in the `children` map of its parent, including children denoted in
    // `dynamic_components`.
    fn add_descendants(
        instance: &Arc<ComponentInstanceForAnalyzer>,
        decls_by_url: &HashMap<Url, (ComponentDecl, Option<ConfigFields>)>,
        dynamic_components: &HashMap<Moniker, Vec<Child>>,
        dynamic_dictionaries: &Arc<DynamicDictionaryConfig>,
        model: &mut ComponentModelForAnalyzer,
        result: &mut BuildModelResult,
    ) {
        let mut children = vec![];
        for child_decl in instance.decl.children.iter() {
            let child_moniker = ChildName::new(child_decl.name.clone(), None);
            match Self::get_absolute_child_url(&child_decl.url, instance) {
                Ok(url) => {
                    children.push(Child {
                        child_moniker,
                        url,
                        environment: child_decl.environment.clone(),
                    });
                }
                Err(err) => {
                    result.errors.push(anyhow!(err));
                }
            }
        }
        if let Some(dynamic_children) = dynamic_components.get(instance.moniker()) {
            children.append(
                &mut dynamic_children
                    .into_iter()
                    .map(|dynamic_child| dynamic_child.clone())
                    .collect(),
            );
        }

        for child in children.iter() {
            if child.child_moniker.name().is_empty() {
                result.errors.push(anyhow!(BuildAnalyzerModelError::InvalidChildDecl(
                    child.url.to_string(),
                    instance.moniker().to_string(),
                )));
                continue;
            }

            match Self::get_decl_by_url(decls_by_url, &child.url)
                .context("Failed to parse absolute child URL")
            {
                Err(err) => {
                    result.errors.push(err);
                }
                Ok(Some((child_component_decl, child_config))) => {
                    match ComponentInstanceForAnalyzer::new_for_child(
                        child,
                        child_component_decl.clone(),
                        child_config.clone(),
                        Arc::clone(instance),
                        model.policy_checker.clone(),
                        Arc::clone(&model.component_id_index),
                        Arc::clone(&dynamic_dictionaries),
                    ) {
                        Ok(child_instance) => {
                            Self::add_descendants(
                                &child_instance,
                                decls_by_url,
                                dynamic_components,
                                dynamic_dictionaries,
                                model,
                                result,
                            );

                            instance.add_child(
                                child.child_moniker.clone(),
                                Arc::clone(&child_instance),
                            );

                            model
                                .instances
                                .insert(child_instance.moniker().clone(), child_instance);
                        }
                        Err(err) => {
                            result.errors.push(anyhow!(err));
                        }
                    }
                }
                Ok(None) => {
                    result.errors.push(anyhow!(BuildAnalyzerModelError::ComponentDeclNotFound(
                        child.url.to_string(),
                        instance.moniker().to_string(),
                    )));
                }
            }
        }
    }

    // Given a component instance and the url `child_url` of a child of that instance,
    // returns an absolute url for the child.
    fn get_absolute_child_url(
        child_url: &Url,
        instance: &Arc<ComponentInstanceForAnalyzer>,
    ) -> Result<Url, BuildAnalyzerModelError> {
        let child_url = child_url.as_str();
        let err = BuildAnalyzerModelError::MalformedUrl(
            instance.url().to_string(),
            instance.moniker().to_string(),
        );

        let url = match url::Url::parse(child_url) {
            Ok(u) => u,
            Err(url::ParseError::RelativeUrlWithoutBase) => {
                let absolute_prefix = match instance.url().is_relative() {
                    true => find_first_absolute_ancestor_url(instance).map_err(|_| err.clone())?,
                    false => instance.url().clone(),
                };
                let absolute_prefix =
                    url::Url::parse(absolute_prefix.as_str()).map_err(|_| err.clone())?;
                absolute_prefix
                    .join(child_url)
                    .expect("failed to join child URL to absolute prefix")
            }
            _ => return Err(err),
        };
        Url::new(url.as_str()).map_err(|_| err.clone())
    }

    fn get_decl_by_url<'a>(
        decls_by_url: &'a HashMap<Url, (ComponentDecl, Option<ConfigFields>)>,
        url: &Url,
    ) -> Result<Option<&'a (ComponentDecl, Option<ConfigFields>)>> {
        // Non-`fuchsia-pkg` URLs are not matched with nuance: they must precisely match an entry
        // in `decls_by_url`.
        if url.scheme().expect("all urls are absolute") != "fuchsia-pkg" {
            return Ok(decls_by_url.get(url));
        }

        let fuchsia_component_url = AbsoluteComponentUrl::parse(url.as_str())
            .context("Failed to parse component fuchsia-pkg URL as absolute package URL")?;

        // Gather both strong and weak URL matches against `fuchsia_component_url`.
        let decl_url_matches = decls_by_url
            .keys()
            .filter_map(|decl_url| {
                if decl_url.scheme().expect("all urls are absolute") != "fuchsia-pkg" {
                    None
                } else if let Ok(decl_fuchsia_pkg_url) =
                    AbsoluteComponentUrl::parse(decl_url.as_str())
                {
                    match match_absolute_component_urls(
                        &decl_fuchsia_pkg_url,
                        &fuchsia_component_url,
                    ) {
                        PkgUrlMatch::NoMatch => None,
                        pkg_url_match => Some((decl_url, pkg_url_match)),
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<(&Url, PkgUrlMatch)>>();

        // Return best match. Emit warning or error when multiple matches are found.
        if decl_url_matches.len() == 0 {
            return Ok(None);
        } else if decl_url_matches.len() == 1 {
            if decl_url_matches[0].1 == PkgUrlMatch::WeakMatch {
                log::warn!("Weak component URL match: {} matches {}", url, decl_url_matches[0].0);
            }
            return Ok(decls_by_url.get(decl_url_matches[0].0));
        } else {
            let strong_decl_url_matches = decl_url_matches
                .iter()
                .filter_map(|(url, url_match)| match url_match {
                    PkgUrlMatch::StrongMatch => Some(*url),
                    _ => None,
                })
                .collect::<Vec<&Url>>();

            if strong_decl_url_matches.len() == 0 {
                log::warn!(
                    "Multiple weak component URL matches for {}; matching to first: {}",
                    url,
                    decl_url_matches[0].0
                );
                return Ok(decls_by_url.get(decl_url_matches[0].0));
            } else {
                if strong_decl_url_matches.len() > 1 {
                    log::error!(
                        "Multiple strong package URL matches for {}; matching to first: {}",
                        url,
                        strong_decl_url_matches[0]
                    );
                }
                return Ok(decls_by_url.get(strong_decl_url_matches[0]));
            }
        }
    }
}

/// `ComponentModelForAnalyzer` owns a representation of the v2 component graph and
/// supports lookup of component instances by `Moniker`.
#[derive(Debug, Default)]
pub struct ComponentModelForAnalyzer {
    top_instance: Arc<TopInstanceForAnalyzer>,
    instances: HashMap<Moniker, Arc<ComponentInstanceForAnalyzer>>,
    policy_checker: GlobalPolicyChecker,
    component_id_index: Arc<component_id_index::Index>,
}

impl ComponentModelForAnalyzer {
    /// Returns the number of component instances in the model, not counting the top instance.
    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn get_root_instance(
        self: &Arc<Self>,
    ) -> Result<Arc<ComponentInstanceForAnalyzer>, ComponentInstanceError> {
        self.get_instance(&Moniker::root())
    }

    /// Returns the component instance corresponding to `id` if it is present in the model, or an
    /// `InstanceNotFound` error if not.
    pub fn get_instance(
        self: &Arc<Self>,
        moniker: &Moniker,
    ) -> Result<Arc<ComponentInstanceForAnalyzer>, ComponentInstanceError> {
        match self.instances.get(moniker) {
            Some(instance) => Ok(Arc::clone(instance)),
            None => Err(ComponentInstanceError::instance_not_found(moniker.clone())),
        }
    }

    fn does_child_reference_offer(self: &Arc<Self>, offer: &OfferDecl, child: Moniker) -> bool {
        let instance = if let Ok(i) = self.get_instance(&child.into()) {
            i
        } else {
            // We couldn't find the instance that references this offer.
            return false;
        };

        // Look for a use from parent
        for use_ in &instance.decl.uses {
            if use_.source_name() == offer.target_name() {
                match use_.source() {
                    cm_rust::UseSource::Parent => return true,
                    _ => {}
                }
            }
        }

        // Look for a next offer from parent
        for next_offer in &instance.decl.offers {
            if next_offer.source_name() == offer.target_name() {
                match next_offer.source() {
                    cm_rust::OfferSource::Parent => return true,
                    _ => {}
                }
            }
        }

        return false;
    }

    /// For this offer decl, if the offer target does not reference the capability in its manifest,
    /// attempt to route it and report any errors.
    ///
    /// In other words, this will only verify offer decls that terminate the route chain.
    async fn try_check_offer_capability(
        self: &Arc<Self>,
        offer_decl: &OfferDecl,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> Vec<VerifyRouteResult> {
        let target_moniker = target.moniker();

        let offer_target = offer_decl.target();
        let should_check_offer = match offer_target {
            OfferTarget::Child(c) => {
                let child = ChildName::parse(&c.name).unwrap();
                let offer_target_moniker = target_moniker.child(child);

                // This offer should be checked if there is no reference to it in the child.
                !self.does_child_reference_offer(offer_decl, offer_target_moniker)
            }
            OfferTarget::Collection(_) => {
                // Offering to a collection should always cause an offer check.
                true
            }
            OfferTarget::Capability(_) => {
                // Offering to a dictionary (aggregation) should always cause an offer check.
                true
            }
        };

        if should_check_offer {
            self.check_offer_capability(offer_decl, target).await
        } else {
            // This offer decl doesn't need to be checked.
            vec![]
        }
    }

    pub async fn check_offer_capability(
        self: &Arc<Self>,
        offer_decl: &OfferDecl,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> Vec<VerifyRouteResult> {
        let mut results = Vec::new();
        let (capability, route_request) = match offer_decl.clone() {
            OfferDecl::Protocol(offer_decl) => {
                let capability = offer_decl.source_name.clone();
                let route_request = RouteRequest::OfferProtocol(offer_decl);
                (capability, route_request)
            }
            OfferDecl::Directory(offer_decl) => {
                let capability = offer_decl.source_name.clone();
                let route_request = RouteRequest::OfferDirectory(offer_decl);
                (capability, route_request)
            }
            OfferDecl::Service(offer_decl) => {
                let capability = offer_decl.source_name.clone();
                let route_request = RouteRequest::OfferService(RouteBundle::from_offer(offer_decl));
                (capability, route_request)
            }
            OfferDecl::EventStream(offer_decl) => {
                let capability = offer_decl.source_name.clone();
                let route_request = RouteRequest::OfferEventStream(offer_decl);
                (capability, route_request)
            }
            OfferDecl::Runner(offer_decl) => {
                let capability = offer_decl.source_name.clone();
                let route_request = RouteRequest::OfferRunner(offer_decl);
                (capability, route_request)
            }
            OfferDecl::Resolver(offer_decl) => {
                let capability = offer_decl.source_name.clone();
                let route_request = RouteRequest::OfferResolver(offer_decl);
                (capability, route_request)
            }
            OfferDecl::Config(offer_decl) => {
                let capability = offer_decl.source_name.clone();
                let route_request = RouteRequest::OfferConfig(offer_decl);
                (capability, route_request)
            }
            OfferDecl::Dictionary(offer_decl) => {
                let capability = offer_decl.source_name.clone();
                let route_request = RouteRequest::OfferDictionary(offer_decl);
                (capability, route_request)
            }
            // Storage capabilities are a special case because they result in 2 routes.
            OfferDecl::Storage(offer_decl) => {
                let capability = offer_decl.source_name.clone();
                let (result, storage_route, dir_route) =
                    Self::route_storage_and_backing_directory_from_offer_sync(
                        offer_decl.clone(),
                        target,
                    )
                    .await;

                // Ignore any valid routes to void.
                if let Ok(ref source) = result {
                    if matches!(source.source, CapabilitySource::Void(_)) {
                        return vec![];
                    }
                }

                match (
                    result.map_err(|e| AnalyzerModelError::from(e)),
                    vec![storage_route, dir_route],
                    capability,
                ) {
                    (Ok(source), routes, capability) => {
                        match self.check_use_source(&source, &target).await {
                            Ok(()) => {
                                for route in routes.into_iter() {
                                    results.push(VerifyRouteResult {
                                        using_node: target.moniker().clone(),
                                        target_decl: TargetDecl::Offer(OfferDecl::Storage(
                                            offer_decl.clone(),
                                        )),
                                        capability: Some(capability.clone()),
                                        error: None,
                                        route,
                                        source: Some(source.source.clone()),
                                    });
                                }
                            }
                            Err(err) => {
                                for route in routes.into_iter() {
                                    results.push(VerifyRouteResult {
                                        using_node: target.moniker().clone(),
                                        target_decl: TargetDecl::Offer(OfferDecl::Storage(
                                            offer_decl.clone(),
                                        )),
                                        capability: Some(capability.clone()),
                                        error: Some(err.clone()),
                                        route,
                                        source: Some(source.source.clone()),
                                    });
                                }
                            }
                        }
                    }
                    (Err(err), routes, capability) => {
                        for route in routes.into_iter() {
                            results.push(VerifyRouteResult {
                                using_node: target.moniker().clone(),
                                target_decl: TargetDecl::Offer(OfferDecl::Storage(
                                    offer_decl.clone(),
                                )),
                                capability: Some(capability.clone()),
                                error: Some(err.clone()),
                                route,
                                source: None,
                            });
                        }
                    }
                }
                return results;
            }
        };

        let (route_result, route) = Self::route_capability_sync(route_request, target);
        let source = match route_result {
            Ok(source) => source,
            Err(err) => {
                results.push(VerifyRouteResult {
                    using_node: target.moniker().clone(),
                    target_decl: TargetDecl::Offer(offer_decl.clone()),
                    capability: Some(capability.clone()),
                    error: Some(err.into()),
                    route,
                    source: None,
                });
                return results;
            }
        };

        // Ignore any valid routes to void.
        if let CapabilitySource::Void(_) = source.source {
            return vec![];
        }

        match self.check_use_source(&source, &target).await {
            Ok(()) => {
                results.push(VerifyRouteResult {
                    using_node: target.moniker().clone(),
                    target_decl: TargetDecl::Offer(offer_decl.clone()),
                    capability: Some(capability.clone()),
                    error: None,
                    route,
                    source: Some(source.source),
                });
            }
            Err(err) => {
                results.push(VerifyRouteResult {
                    using_node: target.moniker().clone(),
                    target_decl: TargetDecl::Offer(offer_decl.clone()),
                    capability: Some(capability.clone()),
                    error: Some(err),
                    route,
                    source: Some(source.source),
                });
            }
        };

        results
    }

    /// Checks the routing for all capabilities of the specified types that are `used` by `target`.
    pub async fn check_routes_for_instance(
        self: &Arc<Self>,
        target: &Arc<ComponentInstanceForAnalyzer>,
        capability_types: &HashSet<CapabilityTypeName>,
    ) -> HashMap<CapabilityTypeName, Vec<VerifyRouteResult>> {
        let mut results = HashMap::new();
        for capability_type in capability_types.iter() {
            results.insert(capability_type.clone(), vec![]);
        }

        for use_decl in target.decl.uses.iter().filter(|&u| capability_types.contains(&u.into())) {
            let type_results = results
                .get_mut(&CapabilityTypeName::from(use_decl))
                .expect("expected results for capability type");
            for result in self.check_use_capability(use_decl, &target).await {
                type_results.push(result);
            }
        }

        for expose_decl in
            target.decl.exposes.iter().filter(|&e| capability_types.contains(&e.into()))
        {
            let type_results = results
                .get_mut(&CapabilityTypeName::from(expose_decl))
                .expect("expected results for capability type");
            if let Some(result) = self.check_use_exposed_capability(expose_decl, &target).await {
                type_results.push(result);
            }
        }

        for offer_decl in
            target.decl.offers.iter().filter(|&o| capability_types.contains(&o.into()))
        {
            let type_results = results
                .get_mut(&CapabilityTypeName::from(offer_decl))
                .expect("expected results for capability type");
            for result in self.try_check_offer_capability(offer_decl, &target).await {
                type_results.push(result);
            }
        }

        if capability_types.contains(&CapabilityTypeName::Runner) {
            if let Some(ref program) = target.decl.program {
                let type_results = results
                    .get_mut(&CapabilityTypeName::Runner)
                    .expect("expected results for capability type");
                if let Some(result) = self.check_program_runner(program, &target) {
                    type_results.push(result);
                }
            }
        }

        if capability_types.contains(&CapabilityTypeName::Resolver) {
            let type_results = results
                .get_mut(&CapabilityTypeName::Resolver)
                .expect("expected results for capability type");
            type_results.push(self.check_resolver(&target));
        }

        results
    }

    /// Given a `UseDecl` for a capability at an instance `target`, first routes the capability
    /// to its source and then validates the source.
    ///
    /// This returns a vector of route results because some capabilities (storage) cause
    /// multiple route verifications (route storage + backing directory) and both results
    /// are relevant.
    pub async fn check_use_capability(
        self: &Arc<Self>,
        use_decl: &UseDecl,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> Vec<VerifyRouteResult> {
        let mut results = Vec::new();
        let route_result = match use_decl.clone() {
            UseDecl::Directory(use_directory_decl) => {
                let capability = use_directory_decl.source_name.clone();
                let (result, route) = Self::route_capability_sync(
                    RouteRequest::UseDirectory(use_directory_decl),
                    target,
                );

                // Ignore any valid routes to void.
                if let Ok(ref source) = result {
                    if matches!(source.source, CapabilitySource::Void(_)) {
                        return vec![];
                    }
                }

                (result.map_err(|e| AnalyzerModelError::from(e)), vec![route], capability)
            }
            UseDecl::Protocol(use_protocol_decl) => {
                let capability = use_protocol_decl.source_name.clone();
                let (result, route) = Self::route_capability_sync(
                    RouteRequest::UseProtocol(use_protocol_decl),
                    target,
                );

                // Ignore any valid routes to void.
                if let Ok(ref source) = result {
                    if matches!(source.source, CapabilitySource::Void(_)) {
                        return vec![];
                    }
                }

                (result.map_err(|e| e.into()), vec![route], capability)
            }
            UseDecl::Service(use_service_decl) => {
                let capability = use_service_decl.source_name.clone();
                let (result, route) =
                    Self::route_capability_sync(RouteRequest::UseService(use_service_decl), target);

                // Ignore any valid routes to void.
                if let Ok(ref source) = result {
                    if matches!(source.source, CapabilitySource::Void(_)) {
                        return vec![];
                    }
                }

                (result.map_err(|e| e.into()), vec![route], capability)
            }
            UseDecl::Storage(use_storage_decl) => {
                let capability = use_storage_decl.source_name.clone();
                let (result, storage_route, dir_route) =
                    Self::route_storage_and_backing_directory_sync(use_storage_decl, target).await;

                // Ignore any valid routes to void.
                if let Ok(ref source) = result {
                    if matches!(source.source, CapabilitySource::Void(_)) {
                        return vec![];
                    }
                }

                let result = result.map_err(|e| e.into());
                (result, vec![storage_route, dir_route], capability)
            }
            UseDecl::EventStream(use_event_stream_decl) => {
                let capability = use_event_stream_decl.source_name.clone();
                match Self::route_capability_sync(
                    RouteRequest::UseEventStream(use_event_stream_decl),
                    target,
                ) {
                    (Ok(source), route) => (Ok(source), vec![route], capability),
                    (Err(err), route) => (Err(err.into()), vec![route], capability),
                }
            }
            UseDecl::Runner(use_runner_decl) => {
                let capability = use_runner_decl.source_name.clone();
                match Self::route_capability_sync(RouteRequest::UseRunner(use_runner_decl), target)
                {
                    (Ok(source), route) => (Ok(source), vec![route], capability),
                    (Err(err), route) => (Err(err.into()), vec![route], capability),
                }
            }
            UseDecl::Config(use_config_decl) => {
                let capability = use_config_decl.source_name.clone();
                match Self::route_capability_sync(RouteRequest::UseConfig(use_config_decl), target)
                {
                    (Ok(source), route) => (Ok(source), vec![route], capability),
                    (Err(err), route) => (Err(err.into()), vec![route], capability),
                }
            }
        };
        match route_result {
            (Ok(source), routes, capability) => match self.check_use_source(&source, &target).await
            {
                Ok(()) => {
                    for route in routes.into_iter() {
                        results.push(VerifyRouteResult {
                            using_node: target.moniker().clone(),
                            target_decl: TargetDecl::Use(use_decl.clone()),
                            capability: Some(capability.clone()),
                            error: None,
                            route,
                            source: Some(source.source.clone()),
                        });
                    }
                }
                Err(err) => {
                    for route in routes.into_iter() {
                        results.push(VerifyRouteResult {
                            using_node: target.moniker().clone(),
                            target_decl: TargetDecl::Use(use_decl.clone()),
                            capability: Some(capability.clone()),
                            error: Some(err.clone()),
                            route,
                            source: Some(source.source.clone()),
                        });
                    }
                }
            },
            (Err(err), routes, capability) => {
                for route in routes.into_iter() {
                    results.push(VerifyRouteResult {
                        using_node: target.moniker().clone(),
                        target_decl: TargetDecl::Use(use_decl.clone()),
                        capability: Some(capability.clone()),
                        error: Some(err.clone()),
                        route,
                        source: None,
                    });
                }
            }
        }
        results
    }

    /// Given a `ExposeDecl` for a capability at an instance `target`, checks whether the capability
    /// can be used from an expose declaration. If so, routes the capability to its source and then
    /// validates the source.
    pub async fn check_use_exposed_capability(
        self: &Arc<Self>,
        expose_decl: &ExposeDecl,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> Option<VerifyRouteResult> {
        match self.request_from_expose(expose_decl) {
            Some(request) => {
                let (result, route) = Self::route_capability_sync(request, target);
                let (error, source) = match result {
                    Err(e) => (Some(e.into()), None),
                    Ok(source) => {
                        (self.check_use_source(&source, &target).await.err(), Some(source.source))
                    }
                };

                Some(VerifyRouteResult {
                    using_node: target.moniker().clone(),
                    target_decl: TargetDecl::Expose(expose_decl.clone()),
                    capability: Some(expose_decl.target_name().clone()),
                    error,
                    route,
                    source,
                })
            }
            None => None,
        }
    }

    /// Given a `ProgramDecl` for a component instance, checks whether the specified runner has
    /// a valid capability route.
    pub fn check_program_runner(
        self: &Arc<Self>,
        program_decl: &ProgramDecl,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> Option<VerifyRouteResult> {
        match program_decl.runner {
            Some(ref runner) => {
                let use_runner = UseRunnerDecl {
                    source: UseSource::Environment,
                    source_name: runner.clone(),
                    source_dictionary: Default::default(),
                };
                let (result, route) = Self::route_capability_sync(
                    RouteRequest::UseRunner(use_runner.clone()),
                    target,
                );
                match result {
                    Ok(source) => Some(VerifyRouteResult {
                        using_node: target.moniker().clone(),
                        target_decl: TargetDecl::Use(UseDecl::Runner(use_runner)),
                        capability: Some(runner.clone()),
                        error: None,
                        route,
                        source: Some(source.source),
                    }),
                    Err(err) => Some(VerifyRouteResult {
                        using_node: target.moniker().clone(),
                        target_decl: TargetDecl::Use(UseDecl::Runner(use_runner)),
                        capability: Some(runner.clone()),
                        error: Some(err.into()),
                        route,
                        source: None,
                    }),
                }
            }
            None => None,
        }
    }

    /// Given a component instance, extracts the URL scheme for that instance and looks for a
    /// resolver for that scheme in the instance's environment, recording an error if none
    /// is found. If a resolver is found, checks that it has a valid capability route.
    pub fn check_resolver(
        self: &Arc<Self>,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> VerifyRouteResult {
        let scheme = target.url().scheme().expect("all urls are absolute");

        match target.environment.get_registered_resolver(&scheme) {
            Ok(Some((ExtendedInstanceInterface::Component(instance), resolver))) => {
                let (route_result, route) = Self::route_capability_sync(
                    RouteRequest::Resolver(resolver.clone()),
                    &instance,
                );
                match route_result {
                    Ok(source) => VerifyRouteResult {
                        using_node: target.moniker().clone(),
                        target_decl: TargetDecl::ResolverFromEnvironment(scheme.clone()),
                        capability: Some(resolver.resolver),
                        error: None,
                        route,
                        source: Some(source.source),
                    },
                    Err(err) => VerifyRouteResult {
                        using_node: target.moniker().clone(),
                        target_decl: TargetDecl::ResolverFromEnvironment(scheme.clone()),
                        capability: Some(resolver.resolver),
                        error: Some(err.into()),
                        route,
                        source: None,
                    },
                }
            }
            Ok(Some((ExtendedInstanceInterface::AboveRoot(_), resolver))) => {
                match self.get_builtin_resolver_decl(&resolver) {
                    Ok(decl) => {
                        let route = vec![RouteSegment::ProvideAsBuiltin { capability: decl }];
                        VerifyRouteResult {
                            using_node: target.moniker().clone(),
                            target_decl: TargetDecl::ResolverFromEnvironment(scheme.clone()),
                            capability: Some(resolver.resolver),
                            error: None,
                            route,
                            source: Some(CapabilitySource::Builtin(BuiltinSource {
                                capability: InternalCapability::Resolver(
                                    Name::new(scheme.clone()).unwrap(),
                                ),
                            })),
                        }
                    }
                    Err(err) => VerifyRouteResult {
                        using_node: target.moniker().clone(),
                        target_decl: TargetDecl::ResolverFromEnvironment(scheme.clone()),
                        capability: Some(resolver.resolver),
                        error: Some(err),
                        route: vec![],
                        source: None,
                    },
                }
            }
            Ok(None) => VerifyRouteResult {
                using_node: target.moniker().clone(),
                target_decl: TargetDecl::ResolverFromEnvironment(scheme.clone()),
                capability: None,
                error: Some(AnalyzerModelError::MissingResolverForScheme(
                    target.moniker().clone(),
                    scheme.to_string(),
                )),
                route: vec![],
                source: None,
            },
            Err(err) => VerifyRouteResult {
                using_node: target.moniker().clone(),
                target_decl: TargetDecl::ResolverFromEnvironment(scheme.clone()),
                capability: None,
                error: Some(AnalyzerModelError::from(err)),
                route: vec![],
                source: None,
            },
        }
    }

    // Retrieves the `CapabilityDecl` for a built-in resolver from its registration, or an
    // error if the resolver is not provided as a built-in capability.
    fn get_builtin_resolver_decl(
        &self,
        resolver: &ResolverRegistration,
    ) -> Result<CapabilityDecl, AnalyzerModelError> {
        match self.top_instance.builtin_capabilities().iter().find(|&decl| {
            if let CapabilityDecl::Resolver(resolver_decl) = decl {
                resolver_decl.name == resolver.resolver
            } else {
                false
            }
        }) {
            Some(decl) => Ok(decl.clone()),
            None => Err(AnalyzerModelError::RoutingError(
                RoutingError::use_from_component_manager_not_found(resolver.resolver.to_string()),
            )),
        }
    }

    // Checks properties of a capability source that are necessary to use the capability
    // and that are possible to verify statically.
    async fn check_use_source(
        &self,
        route_source: &RouteSource,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> Result<(), AnalyzerModelError> {
        match &route_source.source {
            CapabilitySource::Component(ComponentSource { moniker, .. }) => {
                let source_component = target.find_absolute(&moniker).await?;
                self.check_executable(&source_component)
            }
            CapabilitySource::Namespace(_) => Ok(()),
            CapabilitySource::Capability(CapabilityToCapabilitySource {
                source_capability,
                moniker: _,
            }) => self
                .check_capability_source(&source_capability, route_source.source.source_moniker()),
            CapabilitySource::Builtin(_) => Ok(()),
            CapabilitySource::Framework(_) => Ok(()),
            CapabilitySource::Void(_) => Ok(()),
            _ => unimplemented![],
        }
    }

    // A helper function validating a source of type `Capability`.
    // The only capability which may have a source of another capability is the `StorageAdmin`
    // protocol. We confirm that the source is a storage capability.
    fn check_capability_source(
        &self,
        source_capability: &ComponentCapability,
        source_moniker: ExtendedMoniker,
    ) -> Result<(), AnalyzerModelError> {
        match source_capability {
            ComponentCapability::Storage(_) => Ok(()),
            _ => Err(AnalyzerModelError::InvalidSourceCapability(
                source_moniker,
                format!("{:?}", source_capability.source_name()),
                fsys::StorageAdminMarker::PROTOCOL_NAME.to_string(),
            )),
        }
    }
    // A helper function which prepares a route request for capabilities which can be used
    // from an expose declaration, and returns None if the capability type cannot be used
    // from an expose.
    fn request_from_expose(self: &Arc<Self>, expose_decl: &ExposeDecl) -> Option<RouteRequest> {
        match expose_decl {
            ExposeDecl::Directory(expose_directory_decl) => {
                Some(RouteRequest::ExposeDirectory(expose_directory_decl.clone()))
            }
            ExposeDecl::Protocol(expose_protocol_decl) => {
                Some(RouteRequest::ExposeProtocol(expose_protocol_decl.clone()))
            }
            ExposeDecl::Service(expose_service_decl) => Some(RouteRequest::ExposeService(
                RouteBundle::from_expose(expose_service_decl.clone()),
            )),
            _ => None,
        }
    }

    // A helper function checking whether a component instance is executable.
    fn check_executable(
        &self,
        component: &Arc<ComponentInstanceForAnalyzer>,
    ) -> Result<(), AnalyzerModelError> {
        match component.decl.program {
            Some(_) => Ok(()),
            None => {
                Err(AnalyzerModelError::SourceInstanceNotExecutable(component.moniker().clone()))
            }
        }
    }

    // Routes a capability from a `ComponentInstanceForAnalyzer` and panics if the future returned by
    // `route_capability` is not ready immediately.
    //
    // TODO(https://fxbug.dev/42168300): Remove this function and use `route_capability` directly when Scrutiny's
    // `DataController`s allow async function calls.
    pub fn route_capability_sync(
        request: RouteRequest,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> (Result<RouteSource, RoutingError>, Vec<RouteSegment>) {
        let mut mapper = RouteMapper::new();
        let result = route_capability(request, target, &mut mapper)
            .now_or_never()
            .expect("future was not ready immediately");
        (result, mapper.get_route())
    }

    pub fn route_event_stream_sync(
        request: UseEventStreamDecl,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> (Result<RouteSource, RoutingError>, Vec<RouteSegment>) {
        let mut mapper = RouteMapper::new();
        let result = route_event_stream(request, target, &mut mapper)
            .now_or_never()
            .expect("future was not ready immediately");
        (result, mapper.get_route())
    }

    // Routes a storage capability and its backing directory from a `ComponentInstanceForAnalyzer` and
    // panics if the returned future is not ready immediately. If routing was successful, then `result`
    // contains the source of the backing directory capability.
    //
    // TODO(https://fxbug.dev/42168300): Remove this function and use `route_capability` directly when Scrutiny's
    // `DataController`s allow async function calls.
    async fn route_storage_and_backing_directory_sync(
        use_decl: UseStorageDecl,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> (Result<RouteSource, RoutingError>, Vec<RouteSegment>, Vec<RouteSegment>) {
        let mut storage_mapper = RouteMapper::new();
        let mut backing_dir_mapper = RouteMapper::new();
        let result = async {
            let result =
                route_capability(RouteRequest::UseStorage(use_decl), target, &mut storage_mapper)
                    .await?;
            let (storage_decl, storage_component) = match result.source {
                CapabilitySource::Component(ComponentSource {
                    capability: ComponentCapability::Storage(storage_decl),
                    moniker,
                    ..
                }) => {
                    let source_component = target.find_absolute(&moniker).await?;
                    (storage_decl, source_component)
                }
                CapabilitySource::Void(_) => return Ok(result),
                _ => unreachable!("unexpected storage source"),
            };
            route_capability(
                RouteRequest::StorageBackingDirectory(storage_decl),
                &storage_component,
                &mut backing_dir_mapper,
            )
            .await
        }
        .now_or_never()
        .expect("future was not ready immediately");
        (result, storage_mapper.get_route(), backing_dir_mapper.get_route())
    }

    // Routes a storage capability and its backing directory from an offer to a `ComponentInstanceForAnalyzer`
    // and panics if the returned future is not ready immediately. If routing was successful, then `result`
    // contains the source of the backing directory capability.
    //
    // TODO(https://fxbug.dev/42168300): Remove this function and use `route_capability` directly when Scrutiny's
    // `DataController`s allow async function calls.
    async fn route_storage_and_backing_directory_from_offer_sync(
        offer_decl: OfferStorageDecl,
        target: &Arc<ComponentInstanceForAnalyzer>,
    ) -> (Result<RouteSource, RoutingError>, Vec<RouteSegment>, Vec<RouteSegment>) {
        let mut storage_mapper = RouteMapper::new();
        let mut backing_dir_mapper = RouteMapper::new();
        let result = async {
            let result = route_capability(
                RouteRequest::OfferStorage(offer_decl),
                target,
                &mut storage_mapper,
            )
            .await?;
            let (storage_decl, storage_component) = match result.source {
                CapabilitySource::Component(ComponentSource {
                    capability: ComponentCapability::Storage(storage_decl),
                    moniker,
                    ..
                }) => {
                    let source_component = target.find_absolute(&moniker).await?;
                    (storage_decl, source_component)
                }
                CapabilitySource::Void(_) => return Ok(result),
                _ => unreachable!("unexpected storage source"),
            };
            route_capability(
                RouteRequest::StorageBackingDirectory(storage_decl),
                &storage_component,
                &mut backing_dir_mapper,
            )
            .await
        }
        .now_or_never()
        .expect("future was not ready immediately");
        (result, storage_mapper.get_route(), backing_dir_mapper.get_route())
    }

    pub fn collect_config_by_url(&self) -> anyhow::Result<BTreeMap<String, ConfigFields>> {
        let mut configs = BTreeMap::new();
        for instance in self.instances.values() {
            let mut fields = match instance.config_fields() {
                Some(f) => f.clone(),
                None => {
                    let Some(ref config_decl) = instance.decl.config else {
                        continue;
                    };
                    ConfigFields { fields: Vec::new(), checksum: config_decl.checksum.clone() }
                }
            };

            for use_ in instance.decl.uses.iter() {
                let cm_rust::UseDecl::Config(config) = use_ else {
                    continue;
                };
                let value = routing::config::route_config_value(config, instance)
                    .now_or_never()
                    .expect("future was not ready immediately")?;
                let Some(value) = value else {
                    continue;
                };

                let new_field = config_encoder::ConfigField {
                    key: config.target_name.clone().into(),
                    value,
                    mutability: Default::default(),
                };

                let mut needs_key = true;
                for field in &mut fields.fields {
                    if field.key != new_field.key {
                        continue;
                    }
                    field.value = new_field.value.clone();
                    needs_key = false;
                }
                if needs_key {
                    fields.fields.push(new_field);
                }
            }

            configs.insert(instance.url().to_string(), fields.clone());
        }
        Ok(configs)
    }
}

#[derive(Clone, Debug)]
pub struct Child {
    pub child_moniker: ChildName,
    pub url: Url,
    pub environment: Option<Name>,
}

#[cfg(test)]
mod tests {
    use super::ModelBuilderForAnalyzer;
    use crate::environment::BOOT_SCHEME;
    use crate::ComponentModelForAnalyzer;
    use anyhow::Result;
    use assert_matches::assert_matches;
    use cm_config::RuntimeConfig;
    use cm_rust::{
        Availability, ComponentDecl, DependencyType, RegistrationSource, ResolverRegistration,
        RunnerRegistration, UseProtocolDecl, UseSource, UseStorageDecl,
    };
    use cm_rust_testing::{
        CapabilityBuilder, ChildBuilder, ComponentDeclBuilder, EnvironmentBuilder, UseBuilder,
    };
    use cm_types::{Name, Url};
    use config_encoder::ConfigFields;
    use fidl_fuchsia_component_internal as component_internal;
    use maplit::hashmap;
    use moniker::{ChildName, Moniker};
    use routing::component_instance::{
        ComponentInstanceInterface, ExtendedInstanceInterface, WeakExtendedInstanceInterface,
    };
    use routing::environment::RunnerRegistry;
    use routing::error::ComponentInstanceError;
    use routing::RouteRequest;
    use std::collections::HashMap;
    use std::sync::Arc;

    const TEST_URL_PREFIX: &str = "test:///";

    fn make_test_url(component_name: &str) -> Url {
        Url::new(&format!("{}{}", TEST_URL_PREFIX, component_name)).unwrap()
    }

    fn make_decl_map(
        components: Vec<(&'static str, ComponentDecl)>,
    ) -> HashMap<Url, (ComponentDecl, Option<ConfigFields>)> {
        HashMap::from_iter(
            components.into_iter().map(|(name, decl)| (make_test_url(name), (decl, None))),
        )
    }

    // Builds a model with structure `root -- child`, retrieves each of the 2 resulting component
    // instances, and tests their public methods.
    #[fuchsia::test]
    fn build_model() -> Result<()> {
        let components = vec![
            ("root", ComponentDeclBuilder::new().child_default("child").build()),
            ("child", ComponentDeclBuilder::new().build()),
        ];

        let config = Arc::new(RuntimeConfig::default());
        let url = make_test_url("root");
        let build_model_result = ModelBuilderForAnalyzer::new(url).build(
            make_decl_map(components),
            config,
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );
        assert_eq!(build_model_result.errors.len(), 0);
        assert!(build_model_result.model.is_some());
        let model = build_model_result.model.unwrap();
        assert_eq!(model.len(), 2);

        let root_instance = model.get_instance(&Moniker::root()).expect("root instance");
        let child_instance =
            model.get_instance(&Moniker::parse_str("child").unwrap()).expect("child instance");

        let other_moniker = Moniker::parse_str("other").unwrap();
        let get_other_result = model.get_instance(&other_moniker);
        assert_eq!(
            get_other_result.err().unwrap().to_string(),
            ComponentInstanceError::instance_not_found(
                Moniker::parse_str(&other_moniker.to_string()).unwrap()
            )
            .to_string()
        );

        assert_eq!(root_instance.moniker(), &Moniker::root());
        assert_eq!(child_instance.moniker(), &Moniker::parse_str("child").unwrap());

        match root_instance.try_get_parent()? {
            ExtendedInstanceInterface::AboveRoot(_) => {}
            _ => panic!("root instance's parent should be `AboveRoot`"),
        }
        match child_instance.try_get_parent()? {
            ExtendedInstanceInterface::Component(component) => {
                assert_eq!(component.moniker(), root_instance.moniker());
            }
            _ => panic!("child instance's parent should be root component"),
        }

        let get_child = root_instance
            .resolve()
            .map(|locked| locked.get_child(&ChildName::try_new("child", None).unwrap()))?;
        assert!(get_child.is_some());
        assert_eq!(get_child.as_ref().unwrap().moniker(), child_instance.moniker());

        let root_environment = root_instance.environment();
        let child_environment = child_instance.environment();

        assert_eq!(root_environment.env().name(), None);
        match root_environment.env().parent() {
            WeakExtendedInstanceInterface::AboveRoot(_) => {}
            _ => panic!("root environment's parent should be `AboveRoot`"),
        }

        assert_eq!(child_environment.env().name(), None);
        match child_environment.env().parent() {
            WeakExtendedInstanceInterface::Component(component) => {
                assert_eq!(component.upgrade()?.moniker(), root_instance.moniker());
            }
            _ => panic!("child environment's parent should be root component"),
        }

        assert!(root_instance.resolve().is_ok());
        assert!(child_instance.resolve().is_ok());

        Ok(())
    }

    // Builds a model with structure `root -- child` where the child's URL is expressed in
    // the root manifest as a relative URL.
    #[fuchsia::test]
    fn build_model_with_relative_url() {
        let root_decl = ComponentDeclBuilder::new()
            .child(ChildBuilder::new().name("child").url("#child").build())
            .build();
        let child_decl = ComponentDeclBuilder::new().build();
        let root_url = make_test_url("root");
        let absolute_child_url = Url::new(&format!("{}#child", root_url)).unwrap();

        let mut decls_by_url = HashMap::new();
        decls_by_url.insert(root_url.clone(), (root_decl, None));
        decls_by_url.insert(absolute_child_url.clone(), (child_decl, None));

        let config = Arc::new(RuntimeConfig::default());
        let build_model_result = ModelBuilderForAnalyzer::new(root_url).build(
            decls_by_url,
            config,
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );
        assert_eq!(build_model_result.errors.len(), 0);
        assert!(build_model_result.model.is_some());
        let model = build_model_result.model.unwrap();
        assert_eq!(model.len(), 2);

        let child_instance =
            model.get_instance(&Moniker::parse_str("child").unwrap()).expect("child instance");

        assert_eq!(child_instance.url(), &absolute_child_url);
    }

    // Spot-checks that `route_capability` returns immediately when routing a capability from a
    // `ComponentInstanceForAnalyzer`. In addition, updates to that method should
    // be reviewed to make sure that this property holds; otherwise, `ComponentModelForAnalyzer`'s
    // sync methods may panic.
    #[fuchsia::test]
    fn route_capability_is_sync() {
        let components = vec![("root", ComponentDeclBuilder::new().build())];

        let config = Arc::new(RuntimeConfig::default());
        let url = make_test_url("root");
        let build_model_result = ModelBuilderForAnalyzer::new(url).build(
            make_decl_map(components),
            config,
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );
        assert_eq!(build_model_result.errors.len(), 0);
        assert!(build_model_result.model.is_some());
        let model = build_model_result.model.unwrap();
        assert_eq!(model.len(), 1);

        let root_instance = model.get_instance(&Moniker::root()).expect("root instance");

        // Panics if the future returned by `route_capability` was not ready immediately.
        // If no panic, discard the result.
        let _ = ComponentModelForAnalyzer::route_capability_sync(
            RouteRequest::UseProtocol(UseProtocolDecl {
                source: UseSource::Parent,
                source_name: "bar_svc".parse().unwrap(),
                source_dictionary: Default::default(),
                target_path: "/svc/hippo".parse().unwrap(),
                dependency_type: DependencyType::Strong,
                availability: Availability::Required,
            }),
            &root_instance,
        );
    }

    // Checks that `route_capability` returns immediately when routing a capability from a
    // `ComponentInstanceForAnalyzer`. In addition, updates to that method should
    // be reviewed to make sure that this property holds; otherwise, `ComponentModelForAnalyzer`'s
    // sync methods may panic.
    #[fuchsia::test]
    async fn route_storage_and_backing_directory_is_sync() {
        let components = vec![("root", ComponentDeclBuilder::new().build())];

        let config = Arc::new(RuntimeConfig::default());
        let cm_url = make_test_url("root");
        let build_model_result = ModelBuilderForAnalyzer::new(cm_url).build(
            make_decl_map(components),
            config,
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );
        assert_eq!(build_model_result.errors.len(), 0);
        assert!(build_model_result.model.is_some());
        let model = build_model_result.model.unwrap();
        assert_eq!(model.len(), 1);

        let root_instance = model.get_instance(&Moniker::root()).expect("root instance");

        // Panics if the future returned by `route_storage_and_backing_directory` was not ready immediately.
        // If no panic, discard the result.
        let _ = ComponentModelForAnalyzer::route_storage_and_backing_directory_sync(
            UseStorageDecl {
                source_name: "cache".parse().unwrap(),
                target_path: "/storage".parse().unwrap(),
                availability: Availability::Required,
            },
            &root_instance,
        )
        .await;
    }

    #[fuchsia::test]
    fn config_capability_overrides() {
        let package_value: cm_rust::ConfigValue = cm_rust::ConfigSingleValue::Uint8(1).into();
        let config_value: cm_rust::ConfigValue = cm_rust::ConfigSingleValue::Uint8(2).into();

        let config = Arc::new(RuntimeConfig::default());
        let cm_url = make_test_url("root");

        let decl = ComponentDeclBuilder::new()
            .capability(
                CapabilityBuilder::config().name("my_config").value(config_value.clone().into()),
            )
            .use_(
                UseBuilder::config()
                    .name("my_config")
                    .target_name("config")
                    .source(cm_rust::UseSource::Self_)
                    .config_type(cm_rust::ConfigValueType::Uint8),
            )
            .build();

        let mut decl_map = HashMap::<Url, (ComponentDecl, Option<ConfigFields>)>::new();
        decl_map.insert(
            make_test_url("root"),
            (
                decl,
                Some(ConfigFields {
                    fields: vec![config_encoder::ConfigField {
                        key: "config".into(),
                        value: package_value,
                        mutability: Default::default(),
                    }],
                    checksum: cm_rust::ConfigChecksum::Sha256([0; 32]),
                }),
            ),
        );

        let build_model_result = ModelBuilderForAnalyzer::new(cm_url.clone()).build(
            decl_map,
            config,
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );
        assert_eq!(build_model_result.errors.len(), 0);
        assert!(build_model_result.model.is_some());
        let model = build_model_result.model.unwrap();
        assert_eq!(model.len(), 1);

        let config = model.collect_config_by_url().unwrap();
        let config = config.get(cm_url.as_str()).unwrap();
        assert_eq!(config.fields.len(), 1);
        assert_eq!(config.fields[0].key.as_str(), "config");
        assert_eq!(config.fields[0].value, config_value);
    }

    // This checks that a component works successfully with just config capabilities
    // and no Config Value File.
    #[fuchsia::test]
    fn config_capability_only() {
        let config_value: cm_rust::ConfigValue = cm_rust::ConfigSingleValue::Uint8(2).into();

        let config = Arc::new(RuntimeConfig::default());
        let cm_url = make_test_url("root");

        let decl = ComponentDeclBuilder::new()
            .capability(
                CapabilityBuilder::config().name("my_config").value(config_value.clone().into()),
            )
            .use_(
                UseBuilder::config()
                    .name("my_config")
                    .target_name("config")
                    .source(cm_rust::UseSource::Self_)
                    .config_type(cm_rust::ConfigValueType::Uint8),
            )
            .config(cm_rust::ConfigDecl {
                fields: Vec::new(),
                checksum: cm_rust::ConfigChecksum::Sha256([0; 32]),
                value_source: cm_rust::ConfigValueSource::Capabilities(Default::default()),
            })
            .build();

        let mut decl_map = HashMap::<Url, (ComponentDecl, Option<ConfigFields>)>::new();
        decl_map.insert(make_test_url("root"), (decl, None));

        let build_model_result = ModelBuilderForAnalyzer::new(cm_url.clone()).build(
            decl_map,
            config,
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );
        assert_eq!(build_model_result.errors.len(), 0);
        assert!(build_model_result.model.is_some());
        let model = build_model_result.model.unwrap();
        assert_eq!(model.len(), 1);

        let config = model.collect_config_by_url().unwrap();
        let config = config.get(cm_url.as_str()).unwrap();
        assert_eq!(config.fields.len(), 1);
        assert_eq!(config.fields[0].key.as_str(), "config");
        assert_eq!(config.fields[0].value, config_value);
    }

    #[fuchsia::test]
    fn config_capability_optional_from_void() {
        let package_value: cm_rust::ConfigValue = cm_rust::ConfigSingleValue::Uint8(1).into();

        let config = Arc::new(RuntimeConfig::default());
        let cm_url = make_test_url("root");

        // Create and  add the root cml.
        let decl = ComponentDeclBuilder::new()
            .child(
                cm_rust_testing::ChildBuilder::new()
                    .name("child")
                    .url(&make_test_url("child").to_string()),
            )
            .offer(
                cm_rust_testing::OfferBuilder::config()
                    .source(cm_rust::OfferSource::Void)
                    .name("my_config")
                    .target(cm_rust::OfferTarget::Child(cm_rust::ChildRef {
                        name: "child".parse().unwrap(),
                        collection: None,
                    }))
                    .availability(cm_rust::Availability::Optional),
            )
            .build();

        let mut decl_map = HashMap::<Url, (ComponentDecl, Option<ConfigFields>)>::new();
        decl_map.insert(make_test_url("root"), (decl, None));

        // Create and add the child CML.
        let child_url = Url::new(make_test_url("child").to_string())
            .expect("failed to parse root component url");
        let decl = ComponentDeclBuilder::new()
            .use_(
                UseBuilder::config()
                    .name("my_config")
                    .target_name("config")
                    .source(cm_rust::UseSource::Parent)
                    .availability(cm_rust::Availability::Optional)
                    .config_type(cm_rust::ConfigValueType::Uint8),
            )
            .build();

        decl_map.insert(
            make_test_url("child"),
            (
                decl,
                Some(ConfigFields {
                    fields: vec![config_encoder::ConfigField {
                        key: "config".into(),
                        value: package_value.clone(),
                        mutability: Default::default(),
                    }],
                    checksum: cm_rust::ConfigChecksum::Sha256([0; 32]),
                }),
            ),
        );

        let build_model_result = ModelBuilderForAnalyzer::new(cm_url).build(
            decl_map,
            config,
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );
        assert_eq!(build_model_result.errors.len(), 0);
        assert!(build_model_result.model.is_some());
        let model = build_model_result.model.unwrap();
        assert_eq!(model.len(), 2);

        let config = model.collect_config_by_url().unwrap();
        let config = config.get(child_url.as_str()).unwrap();
        assert_eq!(config.fields.len(), 1);
        assert_eq!(config.fields[0].key.as_str(), "config");
        assert_eq!(config.fields[0].value, package_value);
    }

    #[fuchsia::test]
    fn config_capability_routing_error() {
        let config = Arc::new(RuntimeConfig::default());
        let cm_url = make_test_url("root");

        let decl = ComponentDeclBuilder::new()
            .use_(
                UseBuilder::config()
                    .name("my_config")
                    .target_name("config")
                    .source(cm_rust::UseSource::Parent)
                    .config_type(cm_rust::ConfigValueType::Uint8),
            )
            .config(cm_rust::ConfigDecl {
                fields: Vec::new(),
                checksum: cm_rust::ConfigChecksum::Sha256([0; 32]),
                value_source: cm_rust::ConfigValueSource::Capabilities(Default::default()),
            })
            .build();

        let mut decl_map = HashMap::<Url, (ComponentDecl, Option<ConfigFields>)>::new();
        decl_map.insert(make_test_url("root"), (decl, None));

        let build_model_result = ModelBuilderForAnalyzer::new(cm_url).build(
            decl_map,
            config,
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );
        assert_eq!(build_model_result.errors.len(), 0);
        assert!(build_model_result.model.is_some());
        let model = build_model_result.model.unwrap();
        assert_eq!(model.len(), 1);

        assert_matches!(model.collect_config_by_url(), Err(_));
    }

    // Builds a model with structure `root -- child` in which the child environment extends the root's.
    // Checks that the child has access to the inherited runner and resolver registrations through its
    // environment.
    #[fuchsia::test]
    fn environment_inherits() -> Result<()> {
        let child_env_name = "child_env";
        let child_runner_registration = RunnerRegistration {
            source_name: "child_env_runner".parse().unwrap(),
            source: RegistrationSource::Self_,
            target_name: "child_env_runner".parse().unwrap(),
        };
        let child_resolver_registration = ResolverRegistration {
            resolver: "child_env_resolver".parse().unwrap(),
            source: RegistrationSource::Self_,
            scheme: "child_resolver_scheme".into(),
        };

        let components = vec![
            (
                "root",
                ComponentDeclBuilder::new()
                    .child(ChildBuilder::new().name("child").environment(child_env_name))
                    .environment(
                        EnvironmentBuilder::new()
                            .name(child_env_name)
                            .resolver(child_resolver_registration.clone())
                            .runner(child_runner_registration.clone()),
                    )
                    .build(),
            ),
            ("child", ComponentDeclBuilder::new().build()),
        ];

        // Set up the RuntimeConfig to register the `fuchsia-boot` resolver as a built-in,
        // in addition to `builtin_runner`.
        let mut config = RuntimeConfig::default();
        config.builtin_boot_resolver = component_internal::BuiltinBootResolver::Boot;

        let builtin_runner_name: Name = "builtin_elf_runner".parse().unwrap();
        let builtin_runner_registration = RunnerRegistration {
            source_name: builtin_runner_name.clone(),
            source: RegistrationSource::Self_,
            target_name: builtin_runner_name.clone(),
        };

        let cm_url = make_test_url("root");
        let build_model_result = ModelBuilderForAnalyzer::new(cm_url).build(
            make_decl_map(components),
            Arc::new(config),
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::from_decl(&vec![builtin_runner_registration]),
        );
        assert_eq!(build_model_result.errors.len(), 0);
        assert!(build_model_result.model.is_some());
        let model = build_model_result.model.unwrap();
        assert_eq!(model.len(), 2);

        let child_instance =
            model.get_instance(&Moniker::parse_str("child").unwrap()).expect("child instance");

        let get_child_runner_result = child_instance
            .environment()
            .env()
            .get_registered_runner(&child_runner_registration.target_name)?;
        assert!(get_child_runner_result.is_some());
        let (child_runner_registrar, child_runner) = get_child_runner_result.unwrap();
        match child_runner_registrar {
            ExtendedInstanceInterface::Component(instance) => {
                assert_eq!(instance.moniker(), &Moniker::root());
            }
            ExtendedInstanceInterface::AboveRoot(_) => {
                panic!("expected child_env_runner to be registered by the root instance")
            }
        }
        assert_eq!(child_runner_registration, child_runner);

        let get_child_resolver_result = child_instance
            .environment
            .get_registered_resolver(&child_resolver_registration.scheme)?;
        assert!(get_child_resolver_result.is_some());
        let (child_resolver_registrar, child_resolver) = get_child_resolver_result.unwrap();
        match child_resolver_registrar {
            ExtendedInstanceInterface::Component(instance) => {
                assert_eq!(instance.moniker(), &Moniker::root());
            }
            ExtendedInstanceInterface::AboveRoot(_) => {
                panic!("expected child_env_resolver to be registered by the root instance")
            }
        }
        assert_eq!(child_resolver_registration, child_resolver);

        let get_builtin_runner_result =
            child_instance.environment().env().get_registered_runner(&builtin_runner_name)?;
        assert!(get_builtin_runner_result.is_some());
        let (builtin_runner_registrar, _builtin_runner) = get_builtin_runner_result.unwrap();
        match builtin_runner_registrar {
            ExtendedInstanceInterface::Component(_) => {
                panic!("expected builtin runner to be registered above the root")
            }
            ExtendedInstanceInterface::AboveRoot(_) => {}
        }

        let get_builtin_resolver_result =
            child_instance.environment.get_registered_resolver(&BOOT_SCHEME.to_string())?;
        assert!(get_builtin_resolver_result.is_some());
        let (builtin_resolver_registrar, _builtin_resolver) = get_builtin_resolver_result.unwrap();
        match builtin_resolver_registrar {
            ExtendedInstanceInterface::Component(_) => {
                panic!("expected boot resolver to be registered above the root")
            }
            ExtendedInstanceInterface::AboveRoot(_) => {}
        }

        Ok(())
    }

    fn decl(id: &str) -> ComponentDecl {
        // Identify decls by a single child named `id`.
        ComponentDeclBuilder::new().child_default(id).build()
    }

    #[fuchsia::test]
    fn get_decl_by_url_none() {
        let beta_beta_urls = vec![
            Url::new("fuchsia-pkg://test.fuchsia.com/beta#beta.cm").unwrap(),
            Url::new("fuchsia-pkg://test.fuchsia.com/beta/0#beta.cm").unwrap(),
            Url::new("fuchsia-pkg://test.fuchsia.com/beta?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap(),
            Url::new("fuchsia-pkg://test.fuchsia.com/beta/0?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap(),
        ];
        let decls_by_url_no_beta_beta = hashmap! {
            Url::new("fuchsia-pkg://test.fuchsia.com/alpha#beta.cm").unwrap() => (decl("alpha_beta"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/beta/0#alpha.cm").unwrap() => (decl("beta_alpha"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/gamma?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap() => (decl("gamma_beta"), None),
        };

        for beta_beta_url in beta_beta_urls.iter() {
            let result =
                ModelBuilderForAnalyzer::get_decl_by_url(&decls_by_url_no_beta_beta, beta_beta_url);
            assert!(result.is_ok());
            assert_eq!(None, result.ok().unwrap());
        }
    }

    #[fuchsia::test]
    fn get_decl_by_url_fuchsia_boot() {
        let fuchsia_boot_url = Url::new("fuchsia-boot:///#meta/boot.cm").unwrap();
        let fuchsia_boot_component = decl("boot");
        let decls_by_url_with_fuchsia_boot = hashmap! {
            Url::new("fuchsia-pkg://test.fuchsia.com/alpha#beta.cm").unwrap() => (decl("alpha_beta"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/beta/0#alpha.cm").unwrap() => (decl("beta_alpha"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/gamma?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap() => (decl("gamma_beta"), None),
            fuchsia_boot_url.clone() => (fuchsia_boot_component.clone(), None),
        };

        let result = ModelBuilderForAnalyzer::get_decl_by_url(
            &decls_by_url_with_fuchsia_boot,
            &fuchsia_boot_url,
        );

        assert!(result.is_ok());
        assert_eq!(Some(&(fuchsia_boot_component, None)), result.ok().unwrap());
    }

    #[fuchsia::test]
    fn get_decl_by_url_bad_url() {
        let bad_url =
            Url::new("fuchsia-pkg:///test.fuchsia.com/alpha?hash=notahexvalue#meta/alpha.cm")
                .unwrap();
        let empty_decls_by_url = hashmap! {};

        let result = ModelBuilderForAnalyzer::get_decl_by_url(&empty_decls_by_url, &bad_url);

        assert!(result.is_err());
    }

    #[fuchsia::test]
    fn get_decl_by_url_strong() {
        let beta_beta_url = Url::new("fuchsia-pkg://test.fuchsia.com/beta/0?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap();
        let beta_beta_decl = decl("beta_beta");
        let decls_by_url_with_beta_beta = hashmap! {
            Url::new("fuchsia-pkg://test.fuchsia.com/alpha#beta.cm").unwrap() => (decl("alpha_beta"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/beta/0#alpha.cm").unwrap() => (decl("beta_alpha"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/gamma?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap() => (decl("gamma_beta"), None),
            beta_beta_url.clone() => (beta_beta_decl.clone(), None),
        };

        let result =
            ModelBuilderForAnalyzer::get_decl_by_url(&decls_by_url_with_beta_beta, &beta_beta_url);

        assert!(result.is_ok());
        assert_eq!(Some(&(beta_beta_decl, None)), result.ok().unwrap());
    }

    #[fuchsia::test]
    fn get_decl_by_url_strongest() {
        let beta_beta_strong_url = Url::new("fuchsia-pkg://test.fuchsia.com/beta/0?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap();
        let beta_beta_strong_decl = decl("beta_beta_strong");
        let beta_beta_weak_url_1 =
            Url::new("fuchsia-pkg://test.fuchsia.com/beta/0#beta.cm").unwrap();
        let beta_beta_weak_decl_1 = decl("beta_beta_weak_1");
        let beta_beta_weak_url_2 = Url::new("fuchsia-pkg://test.fuchsia.com/beta?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap();
        let beta_beta_weak_decl_2 = decl("beta_beta_weak_2");
        let beta_beta_weak_url_3 = Url::new("fuchsia-pkg://test.fuchsia.com/beta#beta.cm").unwrap();
        let beta_beta_weak_decl_3 = decl("beta_beta_weak_3");
        let decls_by_url_with_4_beta_betas = hashmap! {
            beta_beta_weak_url_1 => (beta_beta_weak_decl_1, None),
            beta_beta_weak_url_2 => (beta_beta_weak_decl_2, None),
            beta_beta_weak_url_3 => (beta_beta_weak_decl_3, None),
            beta_beta_strong_url.clone() => (beta_beta_strong_decl.clone(), None),
        };

        let result = ModelBuilderForAnalyzer::get_decl_by_url(
            &decls_by_url_with_4_beta_betas,
            &beta_beta_strong_url,
        );

        assert!(result.is_ok());
        assert_eq!(Some(&(beta_beta_strong_decl, None)), result.ok().unwrap());
    }

    #[fuchsia::test]
    fn get_decl_by_url_weak() {
        let beta_beta_strong_url = Url::new("fuchsia-pkg://test.fuchsia.com/beta/0?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap();
        let beta_beta_weak_url = Url::new("fuchsia-pkg://test.fuchsia.com/beta/0?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap();
        let beta_beta_decl = decl("beta_beta");
        let decls_by_url_with_strong_beta_beta = hashmap! {
            Url::new("fuchsia-pkg://test.fuchsia.com/alpha#beta.cm").unwrap() => (decl("alpha_beta"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/beta/0#alpha.cm").unwrap() => (decl("beta_alpha"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/gamma?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap() => (decl("gamma_beta"), None),
            beta_beta_strong_url.clone() => (beta_beta_decl.clone(), None),
        };

        let result = ModelBuilderForAnalyzer::get_decl_by_url(
            &decls_by_url_with_strong_beta_beta,
            &beta_beta_weak_url,
        );

        assert!(result.is_ok());
        assert_eq!(Some(&(beta_beta_decl, None)), result.ok().unwrap());
    }

    #[fuchsia::test]
    fn get_decl_by_url_weak_any() {
        let beta_beta_url_1 = Url::new("fuchsia-pkg://test.fuchsia.com/beta/0?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap();
        let beta_beta_decl_1 = decl("beta_beta_strong");
        let beta_beta_url_2 = Url::new("fuchsia-pkg://test.fuchsia.com/beta/0#beta.cm").unwrap();
        let beta_beta_decl_2 = decl("beta_beta_weak_1");
        let beta_beta_url_3 = Url::new("fuchsia-pkg://test.fuchsia.com/beta?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap();
        let beta_beta_decl_3 = decl("beta_beta_weak_2");
        let beta_beta_weakest_url =
            Url::new("fuchsia-pkg://test.fuchsia.com/beta#beta.cm").unwrap();
        let decls_by_url_3_weak_matches = hashmap! {
            Url::new("fuchsia-pkg://test.fuchsia.com/alpha#beta.cm").unwrap() => (decl("alpha_beta"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/beta/0#alpha.cm").unwrap() => (decl("beta_alpha"), None),
            Url::new("fuchsia-pkg://test.fuchsia.com/gamma?hash=0000000000000000000000000000000000000000000000000000000000000000#beta.cm").unwrap() => (decl("gamma_beta"), None),
            beta_beta_url_1 => (beta_beta_decl_1.clone(), None),
            beta_beta_url_2 => (beta_beta_decl_2.clone(), None),
            beta_beta_url_3 => (beta_beta_decl_3.clone(), None),
        };

        let result = ModelBuilderForAnalyzer::get_decl_by_url(
            &decls_by_url_3_weak_matches,
            &beta_beta_weakest_url,
        );

        assert!(result.is_ok());
        let actual_decl = result.ok().unwrap().unwrap();
        assert!(
            beta_beta_decl_1 == actual_decl.0
                || beta_beta_decl_2 == actual_decl.0
                || beta_beta_decl_3 == actual_decl.0
        );
    }
}

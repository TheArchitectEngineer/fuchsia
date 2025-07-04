// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::digest::Digest;
use crate::{
    fplugin_serde, InflatedPrincipal, InflatedResource, PrincipalIdentifier, PrincipalType,
    ResourceReference, ZXName,
};
use core::default::Default;
use fidl_fuchsia_memory_attribution_plugin as fplugin;
use fplugin::Vmo;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
/// Consider that two floats are equals if they differ less than [FLOAT_COMPARISON_EPSILON].
const FLOAT_COMPARISON_EPSILON: f64 = 1e-10;

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct ComponentProfileResult {
    pub kernel: fplugin_serde::KernelStatistics,
    pub principals: Vec<PrincipalSummary>,
    /// Amount, in bytes, of memory that is known but remained unclaimed. Should be equal to zero.
    pub unclaimed: u64,
    #[serde(with = "fplugin_serde::PerformanceImpactMetricsDef")]
    pub performance: fplugin::PerformanceImpactMetrics,
    pub digest: Digest,
}

/// Summary view of the memory usage on a device.
///
/// This view aggregates the memory usage for each Principal, and, for each Principal, for VMOs
/// sharing the same name or belonging to the same logical group. This is a view appropriate to
/// display to developers who want to understand the memory usage of their Principal.
#[derive(Debug, PartialEq, Serialize)]
pub struct MemorySummary {
    pub principals: Vec<PrincipalSummary>,
    /// Amount, in bytes, of memory that is known but remained unclaimed. Should be equal to zero.
    pub unclaimed: u64,
}

impl MemorySummary {
    pub(crate) fn build(
        principals: &HashMap<PrincipalIdentifier, RefCell<InflatedPrincipal>>,
        resources: &HashMap<u64, RefCell<InflatedResource>>,
        resource_names: &Vec<ZXName>,
    ) -> MemorySummary {
        let mut output = MemorySummary { principals: Default::default(), unclaimed: 0 };
        for principal in principals.values() {
            output.principals.push(MemorySummary::build_one_principal(
                &principal,
                &principals,
                &resources,
                &resource_names,
            ));
        }

        output.principals.sort_unstable_by_key(|p| -(p.populated_total as i64));

        let mut unclaimed = 0;
        for (_, resource_ref) in resources {
            let resource = &resource_ref.borrow();
            if resource.claims.is_empty() {
                match &resource.resource.resource_type {
                    fplugin::ResourceType::Job(_) | fplugin::ResourceType::Process(_) => {}
                    fplugin::ResourceType::Vmo(vmo) => {
                        unclaimed += vmo.scaled_populated_bytes.unwrap();
                    }
                    _ => todo!(),
                }
            }
        }
        output.unclaimed = unclaimed;
        output
    }

    fn build_one_principal(
        principal_cell: &RefCell<InflatedPrincipal>,
        principals: &HashMap<PrincipalIdentifier, RefCell<InflatedPrincipal>>,
        resources: &HashMap<u64, RefCell<InflatedResource>>,
        resource_names: &Vec<ZXName>,
    ) -> PrincipalSummary {
        let principal = principal_cell.borrow();
        let mut output = PrincipalSummary {
            name: principal.name().to_owned(),
            id: principal.principal.identifier.0,
            principal_type: match &principal.principal.principal_type {
                PrincipalType::Runnable => "R",
                PrincipalType::Part => "P",
            }
            .to_owned(),
            committed_private: 0,
            committed_scaled: 0.0,
            committed_total: 0,
            populated_private: 0,
            populated_scaled: 0.0,
            populated_total: 0,
            attributor: principal
                .principal
                .parent
                .as_ref()
                .map(|p| principals.get(p))
                .flatten()
                .map(|p| p.borrow().name().to_owned()),
            processes: Vec::new(),
            vmos: HashMap::new(),
        };

        for resource_id in &principal.resources {
            if !resources.contains_key(resource_id) {
                continue;
            }

            let resource = resources.get(resource_id).unwrap().borrow();
            let share_count = resource
                .claims
                .iter()
                .map(|c| c.subject)
                .collect::<HashSet<PrincipalIdentifier>>()
                .len();
            match &resource.resource.resource_type {
                fplugin::ResourceType::Job(_) => todo!(),
                fplugin::ResourceType::Process(_) => {
                    output.processes.push(format!(
                        "{} ({})",
                        resource_names.get(resource.resource.name_index).unwrap().clone(),
                        resource.resource.koid
                    ));
                }
                fplugin::ResourceType::Vmo(vmo_info) => {
                    output.committed_total += vmo_info.total_committed_bytes.unwrap();
                    output.populated_total += vmo_info.total_populated_bytes.unwrap();
                    output.committed_scaled +=
                        vmo_info.scaled_committed_bytes.unwrap() as f64 / share_count as f64;
                    output.populated_scaled +=
                        vmo_info.scaled_populated_bytes.unwrap() as f64 / share_count as f64;
                    if share_count == 1 {
                        output.committed_private += vmo_info.private_committed_bytes.unwrap();
                        output.populated_private += vmo_info.private_populated_bytes.unwrap();
                    }
                    output
                        .vmos
                        .entry(
                            vmo_name_to_digest_zxname(
                                &resource_names.get(resource.resource.name_index).unwrap(),
                            )
                            .clone(),
                        )
                        .or_default()
                        .merge(vmo_info, share_count);
                }
                _ => todo!(),
            }
        }

        for (_source, attribution) in &principal.attribution_claims {
            for resource in &attribution.resources {
                if let ResourceReference::ProcessMapped {
                    process: process_mapped,
                    base: _,
                    len: _,
                } = resource
                {
                    if let Some(process_ref) = resources.get(&process_mapped) {
                        let process = process_ref.borrow();
                        output.processes.push(format!(
                            "{} ({})",
                            resource_names.get(process.resource.name_index).unwrap().clone(),
                            process.resource.koid
                        ));
                    }
                }
            }
        }

        output.processes.sort();
        output
    }
}

impl Display for MemorySummary {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

/// Summary of a Principal memory usage, and its breakdown per VMO group.
#[derive(Debug, Serialize)]
pub struct PrincipalSummary {
    /// Identifier for the Principal. This number is not meaningful outside of the memory
    /// attribution system.
    pub id: u64,
    /// Display name of the Principal.
    pub name: String,
    /// Type of the Principal.
    pub principal_type: String,
    /// Number of committed private bytes of the Principal.
    pub committed_private: u64,
    /// Number of committed bytes of all VMOs accessible to the Principal, scaled by the number of
    /// Principals that can access them.
    pub committed_scaled: f64,
    /// Total number of committed bytes of all the VMOs accessible to the Principal.
    pub committed_total: u64,
    /// Number of populated private bytes of the Principal.
    pub populated_private: u64,
    /// Number of populated bytes of all VMOs accessible to the Principal, scaled by the number of
    /// Principals that can access them.
    pub populated_scaled: f64,
    /// Total number of populated bytes of all the VMOs accessible to the Principal.
    pub populated_total: u64,

    /// Name of the Principal who gave attribution information for this Principal.
    pub attributor: Option<String>,
    /// List of Zircon processes attributed (even partially) to this Principal.
    pub processes: Vec<String>,
    /// Summary of memory usage for the VMOs accessible to this Principal, grouped by VMO name.
    pub vmos: HashMap<ZXName, VmoSummary>,
}

impl PartialEq for PrincipalSummary {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.principal_type == other.principal_type
            && self.committed_private == other.committed_private
            && (self.committed_scaled - other.committed_scaled).abs() < FLOAT_COMPARISON_EPSILON
            && self.committed_total == other.committed_total
            && self.populated_private == other.populated_private
            && (self.populated_scaled - other.populated_scaled).abs() < FLOAT_COMPARISON_EPSILON
            && self.populated_total == other.populated_total
            && self.attributor == other.attributor
            && self.processes == other.processes
            && self.vmos == other.vmos
    }
}

/// Group of VMOs sharing the same name.
#[derive(Default, Debug, Serialize)]
pub struct VmoSummary {
    /// Number of distinct VMOs under the same name.
    pub count: u64,
    /// Number of committed bytes of this VMO group only accessible by the Principal this group
    /// belongs.
    pub committed_private: u64,
    /// Number of committed bytes of this VMO group, scaled by the number of Principals that can
    /// access them.
    pub committed_scaled: f64,
    /// Total number of committed bytes of this VMO group.
    pub committed_total: u64,
    /// Number of populated bytes of this VMO group only accessible by the Principal this group
    /// belongs.
    pub populated_private: u64,
    /// Number of populated bytes of this VMO group, scaled by the number of Principals that can
    /// access them.
    pub populated_scaled: f64,
    /// Total number of populated bytes of this VMO group.
    pub populated_total: u64,
}

impl VmoSummary {
    fn merge(&mut self, vmo_info: &Vmo, share_count: usize) {
        self.count += 1;
        self.committed_total += vmo_info.total_committed_bytes.unwrap();
        self.populated_total += vmo_info.total_populated_bytes.unwrap();
        self.committed_scaled +=
            vmo_info.scaled_committed_bytes.unwrap() as f64 / share_count as f64;
        self.populated_scaled +=
            vmo_info.scaled_populated_bytes.unwrap() as f64 / share_count as f64;
        if share_count == 1 {
            self.committed_private += vmo_info.private_committed_bytes.unwrap();
            self.populated_private += vmo_info.private_populated_bytes.unwrap();
        }
    }
}

impl PartialEq for VmoSummary {
    fn eq(&self, other: &Self) -> bool {
        self.count == other.count
            && self.committed_private == other.committed_private
            && (self.committed_scaled - other.committed_scaled).abs() < FLOAT_COMPARISON_EPSILON
            && self.committed_total == other.committed_total
            && self.populated_private == other.populated_private
            && (self.populated_scaled - other.populated_scaled).abs() < FLOAT_COMPARISON_EPSILON
            && self.populated_total == other.populated_total
    }
}
const VMO_DIGEST_NAME_MAPPING: [(&str, &str); 13] = [
    ("ld\\.so\\.1-internal-heap|(^stack: msg of.*)", "[process-bootstrap]"),
    ("^blob-[0-9a-f]+$", "[blobs]"),
    ("^inactive-blob-[0-9a-f]+$", "[inactive blobs]"),
    ("^thrd_t:0x.*|initial-thread|pthread_t:0x.*$", "[stacks]"),
    ("^data[0-9]*:.*$", "[data]"),
    ("^bss[0-9]*:.*$", "[bss]"),
    ("^relro:.*$", "[relro]"),
    ("^$", "[unnamed]"),
    ("^scudo:.*$", "[scudo]"),
    ("^.*\\.so.*$", "[bootfs-libraries]"),
    ("^stack_and_tls:.*$", "[bionic-stack]"),
    ("^ext4!.*$", "[ext4]"),
    ("^dalvik-.*$", "[dalvik]"),
];

/// Returns the name of a VMO category when the name match on of the rules.
/// This is used for presentation and aggregation.
pub fn vmo_name_to_digest_name(name: &str) -> &str {
    static RULES: std::sync::LazyLock<Vec<(regex::Regex, &'static str)>> =
        std::sync::LazyLock::new(|| {
            VMO_DIGEST_NAME_MAPPING
                .iter()
                .map(|&(pattern, replacement)| (regex::Regex::new(pattern).unwrap(), replacement))
                .collect()
        });
    RULES.iter().find(|(regex, _)| regex.is_match(name.trim())).map_or(name, |rule| rule.1)
}

pub fn vmo_name_to_digest_zxname(name: &ZXName) -> &ZXName {
    static RULES: std::sync::LazyLock<Vec<(regex::bytes::Regex, ZXName)>> =
        std::sync::LazyLock::new(|| {
            VMO_DIGEST_NAME_MAPPING
                .iter()
                .map(|&(pattern, replacement)| {
                    (
                        regex::bytes::Regex::new(pattern).unwrap(),
                        ZXName::try_from_bytes(replacement.as_bytes()).unwrap(),
                    )
                })
                .collect()
        });
    RULES.iter().find(|(regex, _)| regex.is_match(name.as_bstr())).map_or(name, |rule| &rule.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_zx_test() {
        pretty_assertions::assert_eq!(
            vmo_name_to_digest_zxname(&ZXName::from_string_lossy("ld.so.1-internal-heap")),
            &ZXName::from_string_lossy("[process-bootstrap]"),
        );
    }

    #[test]
    fn rename_zx_test_small_name() {
        // Verify that we can match regular expressions anchored at both ends even when the name is
        // not taking the full size of a [ZXName].
        pretty_assertions::assert_eq!(
            vmo_name_to_digest_zxname(&ZXName::from_string_lossy("blob-1234")),
            &ZXName::from_string_lossy("[blobs]"),
        );
    }

    #[test]
    fn rename_test() {
        pretty_assertions::assert_eq!(
            vmo_name_to_digest_name("ld.so.1-internal-heap"),
            "[process-bootstrap]"
        );
        pretty_assertions::assert_eq!(
            vmo_name_to_digest_name("stack: msg of 123"),
            "[process-bootstrap]"
        );
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("blob-123"), "[blobs]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("blob-15e0da8e"), "[blobs]");
        pretty_assertions::assert_eq!(
            vmo_name_to_digest_name("inactive-blob-123"),
            "[inactive blobs]"
        );
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("thrd_t:0x123"), "[stacks]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("initial-thread"), "[stacks]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("pthread_t:0x123"), "[stacks]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("data456:"), "[data]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("bss456:"), "[bss]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("relro:foobar"), "[relro]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name(""), "[unnamed]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("scudo:primary"), "[scudo]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("libfoo.so.1"), "[bootfs-libraries]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("foobar"), "foobar");
        pretty_assertions::assert_eq!(
            vmo_name_to_digest_name("stack_and_tls:2331"),
            "[bionic-stack]"
        );
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("ext4!foobar"), "[ext4]");
        pretty_assertions::assert_eq!(vmo_name_to_digest_name("dalvik-data1234"), "[dalvik]");
    }
}

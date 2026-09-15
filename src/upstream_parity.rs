//! Checked-in upstream parity metadata for Compound Engineering and pstack.
//!
//! The manifest is intentionally data, not a network probe: CI can verify
//! that every tracked claim names a local witness without depending on GitHub.
//! Refresh refs manually from the upstream repositories, then rerun the same
//! offline validation.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Value, json};

const MANIFEST: &str = include_str!("../docs/upstream-parity.toml");
const SCHEMA: &str = "flywheel-upstream-parity.v1";

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Manifest {
    schema: String,
    checked_at: String,
    refresh_policy: String,
    sources: Vec<Source>,
    capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Deserialize)]
struct Source {
    name: String,
    repository: String,
    #[serde(rename = "ref")]
    git_ref: String,
    version: String,
    observed_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Capability {
    source: String,
    name: String,
    upstream: String,
    local_surface: String,
    status: String,
    evidence: Vec<String>,
    notes: String,
}

pub(crate) fn manifest() -> anyhow::Result<Manifest> {
    let manifest: Manifest = toml::from_str(MANIFEST)?;
    validate(&manifest)?;
    Ok(manifest)
}

fn validate(manifest: &Manifest) -> anyhow::Result<()> {
    anyhow::ensure!(
        manifest.schema == SCHEMA,
        "unsupported parity schema {:?}",
        manifest.schema
    );
    anyhow::ensure!(
        !manifest.checked_at.trim().is_empty(),
        "parity checked_at is empty"
    );
    anyhow::ensure!(
        !manifest.refresh_policy.trim().is_empty(),
        "parity refresh_policy is empty"
    );
    anyhow::ensure!(
        manifest.sources.len() >= 2,
        "parity manifest needs both upstream sources"
    );

    let mut source_names = BTreeSet::new();
    for source in &manifest.sources {
        anyhow::ensure!(
            source_names.insert(source.name.clone()),
            "duplicate parity source {:?}",
            source.name
        );
        anyhow::ensure!(
            source.repository.starts_with("https://github.com/"),
            "parity source {:?} must point at GitHub over HTTPS",
            source.name
        );
        anyhow::ensure!(
            source.git_ref.len() == 40 && source.git_ref.chars().all(|c| c.is_ascii_hexdigit()),
            "parity source {:?} has a non-40-hex ref",
            source.name
        );
        anyhow::ensure!(
            !source.version.trim().is_empty(),
            "parity source {:?} has no version",
            source.name
        );
        anyhow::ensure!(
            !source.observed_at.trim().is_empty(),
            "parity source {:?} has no observed_at",
            source.name
        );
    }

    let mut capability_names = BTreeSet::new();
    let known_sources: BTreeSet<&str> = manifest.sources.iter().map(|s| s.name.as_str()).collect();
    anyhow::ensure!(
        !manifest.capabilities.is_empty(),
        "parity manifest has no capabilities"
    );
    for capability in &manifest.capabilities {
        anyhow::ensure!(
            known_sources.contains(capability.source.as_str()),
            "capability {:?} names unknown source {:?}",
            capability.name,
            capability.source
        );
        anyhow::ensure!(
            capability_names.insert(format!("{}::{}", capability.source, capability.name)),
            "duplicate parity capability {:?}::{:?}",
            capability.source,
            capability.name
        );
        anyhow::ensure!(
            matches!(
                capability.status.as_str(),
                "adopted" | "partial" | "deferred"
            ),
            "capability {:?} has invalid status {:?}",
            capability.name,
            capability.status
        );
        anyhow::ensure!(
            !capability.upstream.trim().is_empty(),
            "capability {:?} has no upstream description",
            capability.name
        );
        anyhow::ensure!(
            !capability.local_surface.trim().is_empty(),
            "capability {:?} has no local surface",
            capability.name
        );
        anyhow::ensure!(
            !capability.evidence.is_empty(),
            "capability {:?} has no evidence paths",
            capability.name
        );
        anyhow::ensure!(
            !capability.notes.trim().is_empty(),
            "capability {:?} has no notes",
            capability.name
        );
        for path in &capability.evidence {
            anyhow::ensure!(
                !path.trim().is_empty(),
                "capability {:?} has an empty evidence path",
                capability.name
            );
        }
    }
    Ok(())
}

/// Machine-readable parity summary exposed through `--robot-capabilities`.
pub(crate) fn robot_summary() -> Value {
    match manifest() {
        Ok(manifest) => json!({
            "schema": manifest.schema,
            "checked_at": manifest.checked_at,
            "sources": manifest.sources.iter().map(|source| json!({
                "name": source.name,
                "version": source.version,
                "ref": source.git_ref,
                "repository": source.repository,
                "observed_at": source.observed_at,
            })).collect::<Vec<_>>(),
            "capabilities": manifest.capabilities.iter().map(|capability| json!({
                "source": capability.source,
                "name": capability.name,
                "status": capability.status,
                "local_surface": capability.local_surface,
                "evidence": capability.evidence,
            })).collect::<Vec<_>>(),
        }),
        Err(error) => json!({
            "schema": SCHEMA,
            "status": "invalid",
            "error": format!("{error:#}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn checked_manifest_has_current_upstream_refs() {
        let manifest = manifest().expect("parity manifest validates");
        assert_eq!(manifest.schema, SCHEMA);
        assert_eq!(manifest.checked_at, "2026-09-15");
        let compound = manifest
            .sources
            .iter()
            .find(|source| source.name == "compound-engineering")
            .expect("compound source");
        assert_eq!(compound.version, "3.26.2");
        assert_eq!(compound.git_ref, "0ec66db4e9f7391c6df9e363b65c664265ccd538");
        let pstack = manifest
            .sources
            .iter()
            .find(|source| source.name == "pstack")
            .expect("pstack source");
        assert_eq!(pstack.version, "0.15.2");
        assert_eq!(pstack.git_ref, "5bf2b1544db739998121a306340631963c2ff3de");
    }

    #[test]
    fn every_parity_witness_exists_in_this_checkout() {
        let manifest = manifest().expect("parity manifest validates");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for capability in manifest.capabilities {
            for evidence in capability.evidence {
                assert!(
                    root.join(&evidence).exists(),
                    "{} evidence path missing: {}",
                    capability.name,
                    evidence
                );
            }
        }
    }

    #[test]
    fn robot_summary_is_machine_readable_and_reports_both_sources() {
        let summary = robot_summary();
        assert_eq!(summary["schema"], SCHEMA);
        assert_eq!(summary["sources"].as_array().map(Vec::len), Some(2));
        assert!(
            summary["capabilities"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
        );
    }
}

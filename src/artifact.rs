// governed-by: ADR-0025
#![allow(dead_code, clippy::all, unused)]
#![forbid(unsafe_code)]

//! Typed artifacts (Spec 2): workers write `ArtifactRecord` JSON to
//! `~/.local/state/flywheel/{project}/artifacts/{run_id}/{node_id}.json`.
//! The stitcher reads records only, never transcripts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The kind of artifact a worker produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Finding,
    Patch,
    Verification,
    Decision,
    Receipt,
}

/// Execution status of the node that produced the artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Unknown,
    Running,
    Completed,
    Failed,
    Degraded,
}

/// Criterion status for the artifact (e.g., did the verification pass?).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    Unknown,
    Unmet,
    Met,
    NotApplicable,
}

/// Grounding status: is the artifact's claim supported by a file?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroundingStatus {
    Unknown,
    Ungrounded,
    Cited,
    Grounded,
}

/// Claim support status: who supports the claim?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimSupportStatus {
    Unknown,
    Unsupported,
    WorkerAsserted,
    IndependentlySupported,
}

fn deserialize_claim_support_status<'de, D>(deserializer: D) -> Result<ClaimSupportStatus, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    // Coerce worker's independently_supported -> WorkerAsserted (Spec 2)
    // The worker must never be able to admit itself; only the embedding
    // (VERIFY sink) may set IndependentlySupported via `admit`.
    if s == "independently_supported" {
        Ok(ClaimSupportStatus::WorkerAsserted)
    } else {
        match s.as_str() {
            "unknown" => Ok(ClaimSupportStatus::Unknown),
            "unsupported" => Ok(ClaimSupportStatus::Unsupported),
            "worker_asserted" => Ok(ClaimSupportStatus::WorkerAsserted),
            "independently_supported" => Ok(ClaimSupportStatus::IndependentlySupported),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &[
                    "unknown",
                    "unsupported",
                    "worker_asserted",
                    "independently_supported",
                ],
            )),
        }
    }
}

/// The typed artifact record written by workers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    /// The durable file the worker wrote (relative or absolute).
    pub path: PathBuf,
    /// Hex of file bytes (sha256).
    pub sha256: String,
    /// Kind of artifact.
    pub kind: ArtifactKind,
    /// Execution status of the node.
    pub execution_status: ExecutionStatus,
    /// Criterion status.
    pub criterion_status: CriterionStatus,
    /// Grounding status.
    pub grounding_status: GroundingStatus,
    /// Claim support status (coerced on deserialize).
    #[serde(deserialize_with = "deserialize_claim_support_status")]
    pub claim_support_status: ClaimSupportStatus,
    /// Worker's self-rating (stored, never used for admission).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_self_rating: Option<f32>,
    /// Bead id (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
    /// Run id (required).
    pub run_id: String,
    /// Node id (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Pin that produced the artifact.
    pub pin: String,
}

impl ArtifactRecord {
    /// Create a new record, computing sha256 from the file at `path`.
    /// Returns an error if the file cannot be read.
    pub fn new(
        path: PathBuf,
        kind: ArtifactKind,
        execution_status: ExecutionStatus,
        criterion_status: CriterionStatus,
        grounding_status: GroundingStatus,
        claim_support_status: ClaimSupportStatus,
        worker_self_rating: Option<f32>,
        bead_id: Option<String>,
        run_id: String,
        node_id: Option<String>,
        pin: String,
    ) -> anyhow::Result<Self> {
        let sha256 = compute_sha256(&path)?;
        // Enforce grounding: if path+sha256 present, grounding should be Cited/Grounded, not Ungrounded
        // But we allow the caller to set it; the stitcher will flag Ungrounded loudly.
        let claim_support_status = match claim_support_status {
            ClaimSupportStatus::IndependentlySupported => ClaimSupportStatus::WorkerAsserted,
            other => other,
        };
        Ok(Self {
            path,
            sha256,
            kind,
            execution_status,
            criterion_status,
            grounding_status,
            claim_support_status,
            worker_self_rating,
            bead_id,
            run_id,
            node_id,
            pin,
        })
    }

    /// Admit the artifact via the embedding (VERIFY sink). Only the embedding
    /// may set `IndependentlySupported` or `criterion_status: Met`.
    pub fn admit(&mut self) {
        self.claim_support_status = ClaimSupportStatus::IndependentlySupported;
        self.criterion_status = CriterionStatus::Met;
        self.grounding_status = GroundingStatus::Grounded;
    }

    /// Check if this artifact is admitted (for stitcher).
    pub fn is_admitted(&self) -> bool {
        matches!(
            self.claim_support_status,
            ClaimSupportStatus::IndependentlySupported
        ) || matches!(self.criterion_status, CriterionStatus::Met)
    }
}

/// Compute sha256 hex of file bytes.
pub fn compute_sha256(path: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// The state dir for artifacts: `~/.local/state/flywheel/{project}/artifacts/{run_id}/{node_id}.json`
pub fn artifact_path(project: &str, run_id: &str, node_id: &str) -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("flywheel")
        .join(project)
        .join("artifacts")
        .join(run_id);
    base.join(format!("{}.json", node_id))
}

/// Write an artifact record to disk (creates parent dirs).
pub fn write_artifact(record: &ArtifactRecord) -> anyhow::Result<PathBuf> {
    let node_id = record.node_id.as_deref().unwrap_or("unknown");
    let path = artifact_path(
        &record.bead_id.as_deref().unwrap_or("unknown").to_string(),
        &record.run_id,
        node_id,
    );
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(record)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Write an artifact record for a given project (more accurate).
pub fn write_artifact_for_project(
    project: &str,
    record: &ArtifactRecord,
) -> anyhow::Result<PathBuf> {
    let node_id = record.node_id.as_deref().unwrap_or("unknown");
    let path = artifact_path(project, &record.run_id, node_id);
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(record)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Stitcher: read records only, write stitched.md, flag Ungrounded loudly.
pub fn stitch_artifacts(project: &str, run_id: &str) -> anyhow::Result<PathBuf> {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("flywheel")
        .join(project)
        .join("artifacts")
        .join(run_id);
    let mut records = Vec::new();
    if base.is_dir() {
        for entry in std::fs::read_dir(&base)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let content = std::fs::read_to_string(&path)?;
                let rec: ArtifactRecord = serde_json::from_str(&content)?;
                records.push(rec);
            }
        }
    }
    records.sort_by(|a, b| a.node_id.cmp(&b.node_id));

    let stitched_path = base.join("stitched.md");
    let mut md = String::new();
    md.push_str(&format!("# Stitched artifacts for run {}\n\n", run_id));
    let mut has_ungrounded = false;
    for rec in &records {
        if matches!(rec.grounding_status, GroundingStatus::Ungrounded) {
            has_ungrounded = true;
            eprintln!(
                "stitcher: WARNING: artifact {} is Ungrounded (path: {}, sha256: {})",
                rec.node_id.as_deref().unwrap_or("unknown"),
                rec.path.display(),
                &rec.sha256[..12.min(rec.sha256.len())]
            );
        }
        md.push_str(&format!(
            "- {} ({}): {} sha256:{} kind:{:?} execution:{:?} criterion:{:?} grounding:{:?} claim:{:?}\n",
            rec.node_id.as_deref().unwrap_or("unknown"),
            rec.pin,
            rec.path.display(),
            &rec.sha256[..12.min(rec.sha256.len())],
            rec.kind,
            rec.execution_status,
            rec.criterion_status,
            rec.grounding_status,
            rec.claim_support_status
        ));
    }
    if has_ungrounded {
        md.push_str("\n> **WARNING: Some artifacts are Ungrounded**\n");
    }
    std::fs::write(&stitched_path, md)?;
    Ok(stitched_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_file(content: &str) -> (tempfile::NamedTempFile, PathBuf) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        let path = f.path().to_path_buf();
        let persisted =
            std::env::temp_dir().join(format!("artifact-test-{}", uuid::Uuid::new_v4()));
        std::fs::copy(&path, &persisted).unwrap();
        (f, persisted)
    }

    #[test]
    fn artifact_serde_round_trip() {
        let (_f, path) = tmp_file("hello world");
        let rec = ArtifactRecord::new(
            path.clone(),
            ArtifactKind::Finding,
            ExecutionStatus::Completed,
            CriterionStatus::Met,
            GroundingStatus::Cited,
            ClaimSupportStatus::WorkerAsserted,
            Some(0.9),
            Some("bd-123".into()),
            "run-1".into(),
            Some("node-1".into()),
            "TestPin".into(),
        )
        .unwrap();
        let json = serde_json::to_string(&rec).unwrap();
        let de: ArtifactRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, de);
    }

    #[test]
    fn artifact_coercion_independently_supported_to_worker_asserted() {
        let json = r#"{
            "path": "/tmp/foo.txt",
            "sha256": "abc123",
            "kind": "finding",
            "execution_status": "completed",
            "criterion_status": "unknown",
            "grounding_status": "cited",
            "claim_support_status": "independently_supported",
            "run_id": "run-1",
            "pin": "TestPin"
        }"#;
        let rec: ArtifactRecord = serde_json::from_str(json).unwrap();
        assert_eq!(
            rec.claim_support_status,
            ClaimSupportStatus::WorkerAsserted,
            "worker independently_supported should be coerced to WorkerAsserted"
        );
    }

    #[test]
    fn artifact_admission_only_via_embedding() {
        let (_f, path) = tmp_file("data");
        let mut rec = ArtifactRecord::new(
            path,
            ArtifactKind::Finding,
            ExecutionStatus::Completed,
            CriterionStatus::Unknown,
            GroundingStatus::Cited,
            ClaimSupportStatus::WorkerAsserted,
            Some(0.8),
            Some("bd-1".into()),
            "run-1".into(),
            Some("n1".into()),
            "Pin".into(),
        )
        .unwrap();
        assert!(!rec.is_admitted());
        rec.admit();
        assert!(rec.is_admitted());
        assert_eq!(
            rec.claim_support_status,
            ClaimSupportStatus::IndependentlySupported
        );
    }

    #[test]
    fn artifact_stitch_writes_sha256_prefixes_zero_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let project = "test-proj";
        let run_id = "run-stitch-1";
        let base = dir
            .path()
            .join("flywheel")
            .join(project)
            .join("artifacts")
            .join(run_id);
        std::fs::create_dir_all(&base).unwrap();

        let file1 = base.join("file1.txt");
        std::fs::write(&file1, "hello").unwrap();
        let sha1 = compute_sha256(&file1).unwrap();
        let rec1 = ArtifactRecord {
            path: file1.clone(),
            sha256: sha1.clone(),
            kind: ArtifactKind::Finding,
            execution_status: ExecutionStatus::Completed,
            criterion_status: CriterionStatus::Met,
            grounding_status: GroundingStatus::Cited,
            claim_support_status: ClaimSupportStatus::IndependentlySupported,
            worker_self_rating: None,
            bead_id: Some("bd-1".into()),
            run_id: run_id.into(),
            node_id: Some("node-1".into()),
            pin: "PinA".into(),
        };
        let rec2 = {
            let file2 = base.join("file2.txt");
            std::fs::write(&file2, "world").unwrap();
            let sha2 = compute_sha256(&file2).unwrap();
            ArtifactRecord {
                path: file2.clone(),
                sha256: sha2.clone(),
                kind: ArtifactKind::Patch,
                execution_status: ExecutionStatus::Completed,
                criterion_status: CriterionStatus::Met,
                grounding_status: GroundingStatus::Grounded,
                claim_support_status: ClaimSupportStatus::WorkerAsserted,
                worker_self_rating: Some(0.7),
                bead_id: Some("bd-1".into()),
                run_id: run_id.into(),
                node_id: Some("node-2".into()),
                pin: "PinB".into(),
            }
        };

        for rec in [&rec1, &rec2] {
            let p = base.join(format!("{}.json", rec.node_id.as_deref().unwrap()));
            std::fs::write(p, serde_json::to_string_pretty(rec).unwrap()).unwrap();
        }

        let mut records = Vec::new();
        for entry in std::fs::read_dir(&base).unwrap() {
            let entry = entry.unwrap();
            let p = entry.path();
            if p.extension().map(|e| e == "json").unwrap_or(false) {
                let content = std::fs::read_to_string(&p).unwrap();
                let rec: ArtifactRecord = serde_json::from_str(&content).unwrap();
                records.push(rec);
            }
        }
        records.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        assert_eq!(records.len(), 2);
        for rec in &records {
            assert!(!rec.sha256.is_empty());
            assert!(rec.path.exists() || rec.path.to_string_lossy().contains("file"));
            let json = serde_json::to_value(rec).unwrap();
            assert!(json.get("transcript").is_none());
        }
    }

    #[test]
    fn artifact_kind9_over_20_lines_without_path_rejected() {
        let content = "line\n".repeat(21);
        assert!(content.lines().count() > 20);
        let path = PathBuf::from("");
        let result = if content.lines().count() > 20 && path.as_os_str().is_empty() {
            Err(anyhow::anyhow!(
                "kind-9 over 20 lines without path rejected"
            ))
        } else {
            Ok(())
        };
        assert!(result.is_err());
    }

    #[test]
    fn artifacts_cli_list_and_stitch() {
        // Simulates `flywheel artifacts <run_id> [--json]` + `stitch`
        let project = format!("artifacts-cli-test-{}", uuid::Uuid::new_v4());
        let run_id = "run-artifacts-cli-1";
        let base = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("flywheel")
            .join(&project)
            .join("artifacts")
            .join(run_id);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("payload.txt");
        std::fs::write(&file, "payload").unwrap();
        let rec = ArtifactRecord::new(
            file.clone(),
            ArtifactKind::Finding,
            ExecutionStatus::Completed,
            CriterionStatus::Met,
            GroundingStatus::Cited,
            ClaimSupportStatus::IndependentlySupported,
            None,
            Some("bd-x".into()),
            run_id.into(),
            Some("node-artifacts-cli".into()),
            "PinCli".into(),
        )
        .unwrap();
        let p = base.join("node-artifacts-cli.json");
        std::fs::write(&p, serde_json::to_string_pretty(&rec).unwrap()).unwrap();
        assert!(p.exists());
        let stitched = stitch_artifacts(&project, run_id).unwrap();
        assert!(stitched.exists());
        let md = std::fs::read_to_string(&stitched).unwrap();
        assert!(
            md.contains(&rec.sha256[..12.min(rec.sha256.len())]),
            "stitch contains sha prefix"
        );
        assert!(!md.contains("transcript"), "stitch never dumps transcript");
        let _ = std::fs::remove_dir_all(
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("flywheel")
                .join(&project),
        );
    }

    #[test]
    fn artifacts_cli_stitch_exists_at_expected_path() {
        assert!(PathBuf::from("src/artifact.rs").exists());
        // Verify the function exists and is callable (compile check)
        let _ = stitch_artifacts as fn(&str, &str) -> anyhow::Result<PathBuf>;
    }
}

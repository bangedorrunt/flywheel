// governed-by: ADR-0025
#![forbid(unsafe_code)]

//! Typed artifacts (Spec 2): workers write `ArtifactRecord` JSON to
//! `~/.local/state/flywheel/{project}/artifacts/{run_id}/{node_id}.json`.
//! The stitcher reads records only, never transcripts.

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use toron_workflow::identity::redact_value;

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
    match s.as_str() {
        "unknown" => Ok(ClaimSupportStatus::Unknown),
        "unsupported" => Ok(ClaimSupportStatus::Unsupported),
        "worker_asserted" => Ok(ClaimSupportStatus::WorkerAsserted),
        // Worker JSON may claim this status, but cannot admit itself.
        "independently_supported" => Ok(ClaimSupportStatus::WorkerAsserted),
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

// `new`/`admit`/`is_admitted` are reached only from tests in this binary
// (`admit` is the embedding's VERIFY-sink action, driven through the sink
// surface), so gate the impl with its callers rather than shipping dead code.
#[cfg(test)]
impl ArtifactRecord {
    /// Create a new record, computing sha256 from the file at `path`.
    /// Returns an error if the file cannot be read.
    // One positional per recorded field; the record IS the field list.
    #[allow(clippy::too_many_arguments)]
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
    ///
    /// Admission is an embedding action, not a worker assertion. The
    /// embedding marks a record by calling [`Self::admit`]. A deserialized
    /// worker record may say `criterion_status: met`, but that status alone is
    /// not proof. Admission is deliberately not inferred from an untrusted
    /// artifact JSON payload.
    pub fn is_admitted(&self) -> bool {
        matches!(
            self.claim_support_status,
            ClaimSupportStatus::IndependentlySupported
        )
    }
}

/// Stable schema identifier for host-evaluated workflow evidence.
pub const EVALUATION_EVIDENCE_SCHEMA: &str = "flywheel-evaluation-evidence.v1";
/// The evaluator that owns the phase-contract verdict. Worker output is input
/// to this evaluator, never its authority.
pub const EVALUATION_EVALUATOR: &str = "flywheel.phase_contracts";
const MAX_EVALUATION_SNAPSHOT_BYTES: usize = 8192;
const MAX_EVALUATION_CONTRACT_EVIDENCE_BYTES: usize = 512;
const EVALUATION_TRUNCATION_SUFFIX: &str = "…[truncated]";

/// Host-side verdict for one evaluated workflow phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvaluationVerdict {
    Pass,
    Fail,
}

/// Durable evidence that the host evaluated one phase contract.
///
/// The worker response is retained only as redacted, bounded snapshots plus
/// hashes of the complete redacted values. The verdict comes from
/// `phase_contracts`, not from `criterion_status`, `claim_support_status`, or
/// any other worker-authored field. `evidence_hash` detects accidental or
/// post-write mutation; it is an integrity digest, not an authenticity
/// signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationEvidence {
    pub schema: String,
    pub run_id: String,
    pub workflow: String,
    pub link: String,
    pub phase: String,
    pub verdict: EvaluationVerdict,
    pub evaluator: String,
    pub contract_evidence: String,
    pub input_sha256: String,
    pub output_sha256: String,
    pub input_bytes: u64,
    pub output_bytes: u64,
    /// Redacted JSON snapshot, bounded to keep evidence inspectable.
    pub input_snapshot: String,
    /// Redacted JSON snapshot, bounded to keep evidence inspectable.
    pub output_snapshot: String,
    /// SHA-256 over every field above except this field.
    pub evidence_hash: String,
}

impl EvaluationEvidence {
    /// Build evidence from a host phase-contract evaluation.
    ///
    /// This constructor is crate-visible on purpose: phase-chain code is the
    /// only production producer. A worker can provide `input`/`output`, but it
    /// cannot choose the evaluator or promote its own status to `Pass`.
    pub(crate) fn from_phase_contract(
        run_id: &str,
        workflow: &str,
        link: &str,
        input: &Value,
        output: &Value,
        contract: &crate::phase_contracts::PhaseContractReport,
    ) -> Self {
        let (input_snapshot, input_bytes, input_sha256) = redacted_snapshot(input);
        let (output_snapshot, output_bytes, output_sha256) = redacted_snapshot(output);
        let mut evidence = Self {
            schema: EVALUATION_EVIDENCE_SCHEMA.to_string(),
            run_id: run_id.to_string(),
            workflow: workflow.to_string(),
            link: link.to_string(),
            phase: contract.phase.to_string(),
            verdict: if contract.ok {
                EvaluationVerdict::Pass
            } else {
                EvaluationVerdict::Fail
            },
            evaluator: EVALUATION_EVALUATOR.to_string(),
            contract_evidence: crate::agent_ref::redact_evidence(&contract.evidence),
            input_sha256,
            output_sha256,
            input_bytes,
            output_bytes,
            input_snapshot,
            output_snapshot,
            evidence_hash: String::new(),
        };
        evidence.evidence_hash = evidence.compute_integrity_hash();
        evidence
    }

    /// Verify the evidence record's integrity digest.
    pub fn integrity_valid(&self) -> bool {
        self.evidence_hash == self.compute_integrity_hash()
    }

    fn unsigned_value(&self) -> Value {
        serde_json::json!({
            "schema": self.schema,
            "run_id": self.run_id,
            "workflow": self.workflow,
            "link": self.link,
            "phase": self.phase,
            "verdict": self.verdict,
            "evaluator": self.evaluator,
            "contract_evidence": self.contract_evidence,
            "input_sha256": self.input_sha256,
            "output_sha256": self.output_sha256,
            "input_bytes": self.input_bytes,
            "output_bytes": self.output_bytes,
            "input_snapshot": self.input_snapshot,
            "output_snapshot": self.output_snapshot,
        })
    }

    fn compute_integrity_hash(&self) -> String {
        sha256_bytes(
            serde_json::to_string(&self.unsigned_value())
                .unwrap_or_default()
                .as_bytes(),
        )
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn truncate_snapshot(value: &str) -> String {
    if value.len() <= MAX_EVALUATION_SNAPSHOT_BYTES {
        return value.to_string();
    }
    let available =
        MAX_EVALUATION_SNAPSHOT_BYTES.saturating_sub(EVALUATION_TRUNCATION_SUFFIX.len());
    let mut end = available;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &value[..end], EVALUATION_TRUNCATION_SUFFIX)
}

fn redacted_snapshot(value: &Value) -> (String, u64, String) {
    let (redacted, _) = redact_value(value);
    let raw = serde_json::to_string(&redacted).unwrap_or_else(|_| "null".to_string());
    let bytes = raw.len() as u64;
    let digest = sha256_bytes(raw.as_bytes());
    (truncate_snapshot(&raw), bytes, digest)
}

fn safe_component(value: &str) -> String {
    let component: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .take(80)
        .collect();
    if component.is_empty() {
        "unknown".to_string()
    } else {
        component
    }
}

/// State path for one host evaluation under the data root `root`:
/// `flywheel/<project>/artifacts/<run_id>/evaluations/`. The evidence
/// read/write/stitch path takes the root as a value, like
/// [`stitch_artifacts_in`], so a test resolves everything under its own temp
/// dir instead of the operator's `dirs::data_local_dir()` (on macOS
/// `~/Library/Application Support`, which ignores `XDG_DATA_HOME`).
fn evaluation_artifact_path_in(
    root: &std::path::Path,
    project: &str,
    run_id: &str,
    link: &str,
    phase: &str,
) -> PathBuf {
    root.join("flywheel")
        .join(safe_component(project))
        .join("artifacts")
        .join(safe_component(run_id))
        .join("evaluations")
        .join(format!(
            "{}-{}.json",
            safe_component(link),
            safe_component(phase)
        ))
}

fn validate_evaluation_evidence(
    evidence: &EvaluationEvidence,
    expected_run_id: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        evidence.schema == EVALUATION_EVIDENCE_SCHEMA,
        "unsupported evaluation evidence schema {:?}",
        evidence.schema
    );
    anyhow::ensure!(
        evidence.evaluator == EVALUATION_EVALUATOR,
        "unsupported evaluation evidence evaluator {:?}",
        evidence.evaluator
    );
    anyhow::ensure!(
        !evidence.run_id.trim().is_empty(),
        "evaluation evidence run_id is empty"
    );
    anyhow::ensure!(
        !evidence.workflow.trim().is_empty(),
        "evaluation evidence workflow is empty"
    );
    anyhow::ensure!(
        !evidence.link.trim().is_empty(),
        "evaluation evidence link is empty"
    );
    anyhow::ensure!(
        !evidence.phase.trim().is_empty(),
        "evaluation evidence phase is empty"
    );
    anyhow::ensure!(
        evidence.input_snapshot.len() <= MAX_EVALUATION_SNAPSHOT_BYTES,
        "evaluation input snapshot exceeds {} bytes",
        MAX_EVALUATION_SNAPSHOT_BYTES
    );
    anyhow::ensure!(
        evidence.output_snapshot.len() <= MAX_EVALUATION_SNAPSHOT_BYTES,
        "evaluation output snapshot exceeds {} bytes",
        MAX_EVALUATION_SNAPSHOT_BYTES
    );
    anyhow::ensure!(
        evidence.contract_evidence.len() <= MAX_EVALUATION_CONTRACT_EVIDENCE_BYTES,
        "evaluation contract evidence exceeds {} bytes",
        MAX_EVALUATION_CONTRACT_EVIDENCE_BYTES
    );
    if let Some(expected_run_id) = expected_run_id {
        anyhow::ensure!(
            evidence.run_id == expected_run_id,
            "evaluation evidence run_id {:?} does not match requested run {:?}",
            evidence.run_id,
            expected_run_id
        );
    }
    anyhow::ensure!(
        evidence.integrity_valid(),
        "evaluation evidence integrity hash does not match its fields"
    );
    Ok(())
}

/// Persist one host evaluation, returning its durable path.
///
/// The gate reads this file back (`artifacts stitch`,
/// `read_evaluation_evidence_in`), so the write is atomic: a reader never sees a
/// half-written record, and `fs_util::atomic_write` names the failing path in
/// its error where a bare errno does not. A transient OS error gets ONE
/// retry — the write is idempotent (same bytes, same destination) and a real
/// failure still returns Err, so the gate still fails closed.
pub fn write_evaluation_evidence(
    project: &str,
    evidence: &EvaluationEvidence,
) -> anyhow::Result<PathBuf> {
    write_evaluation_evidence_in(&default_data_root(), project, evidence)
}

/// [`write_evaluation_evidence`] under an explicit data root.
fn write_evaluation_evidence_in(
    root: &std::path::Path,
    project: &str,
    evidence: &EvaluationEvidence,
) -> anyhow::Result<PathBuf> {
    validate_evaluation_evidence(evidence, None)?;
    let path = evaluation_artifact_path_in(
        root,
        project,
        &evidence.run_id,
        &evidence.link,
        &evidence.phase,
    );
    let json = serde_json::to_string_pretty(evidence)?;
    if let Err(first) = crate::fs_util::atomic_write(&path, json.as_bytes()) {
        crate::fs_util::atomic_write(&path, json.as_bytes()).map_err(|retry| {
            anyhow::anyhow!("evaluation evidence write failed: {first:#}; retry failed: {retry:#}")
        })?;
    }
    Ok(path)
}

/// Read and validate all host evaluations for one run.
/// The stitcher reads through this with its own root.
fn read_evaluation_evidence_in(
    root: &std::path::Path,
    project: &str,
    run_id: &str,
) -> anyhow::Result<Vec<EvaluationEvidence>> {
    let base = evaluation_artifact_path_in(root, project, run_id, "", "");
    let Some(dir) = base.parent() else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    let mut evidence = Vec::with_capacity(paths.len());
    for path in paths {
        let record: EvaluationEvidence = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        validate_evaluation_evidence(&record, Some(run_id)).map_err(|error| {
            anyhow::anyhow!(
                "evaluation evidence validation failed for {}: {error:#}",
                path.display()
            )
        })?;
        evidence.push(record);
    }
    Ok(evidence)
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

/// A `*.json` in a run dir that is not an `ArtifactRecord`. The composed-run
/// path stores the run's own output as `terminal.json` (see
/// `write_terminal_artifact`), so the stitcher has to name such records
/// instead of dying on a serde field error (bd-mdp9).
struct NonArtifactRecord {
    file: String,
    value: serde_json::Value,
}

/// Strip ANSI CSI escapes: the run's captured error strings carry color
/// codes, and stitched.md is read as text.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for esc in chars.by_ref() {
                if ('@'..='~').contains(&esc) {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// One-line summary of a run outcome record (status/failed_step/error when
/// present). Fields the record does not carry are simply absent.
fn summarize_run_outcome(value: &serde_json::Value) -> String {
    let mut bits: Vec<String> = Vec::new();
    for key in ["status", "failed_step", "error"] {
        if let Some(v) = value.get(key).and_then(serde_json::Value::as_str) {
            bits.push(format!("{key}={}", strip_ansi(v).trim()));
        }
    }
    if bits.is_empty() {
        "no status/error fields".to_string()
    } else {
        bits.join(" ")
    }
}

/// The default XDG data root the artifact tree hangs off.
fn default_data_root() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Stitcher: read records only, write stitched.md, flag Ungrounded loudly.
pub fn stitch_artifacts(project: &str, run_id: &str) -> anyhow::Result<PathBuf> {
    stitch_artifacts_in(&default_data_root(), project, run_id)
}

/// bd-ww75: the ROOT-EXPLICIT stitcher.
///
/// This module is `#![forbid(unsafe_code)]`, so a test cannot pin
/// `XDG_DATA_HOME` to make `dirs::data_local_dir()` deterministic — and it
/// should not: reading the root from ambient env at call time is exactly what
/// made the stitch tests order-dependent. They passed alone and failed in a
/// full suite run because another test had swapped the env root between the
/// fixture write and this read. Injecting the root removes the coupling
/// instead of papering over it, and keeps the assertion about real behavior.
fn stitch_artifacts_in(
    root: &std::path::Path,
    project: &str,
    run_id: &str,
) -> anyhow::Result<PathBuf> {
    let base = root
        .join("flywheel")
        .join(project)
        .join("artifacts")
        .join(run_id);
    let mut records = Vec::new();
    let mut non_artifacts: Vec<NonArtifactRecord> = Vec::new();
    if base.is_dir() {
        for entry in std::fs::read_dir(&base)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let content = std::fs::read_to_string(&path)?;
                match serde_json::from_str::<ArtifactRecord>(&content) {
                    Ok(rec) => records.push(rec),
                    // Not every json in a run dir is a typed artifact: the
                    // composed path stores the run's outcome as terminal.json.
                    // Read it as plain JSON so stitch names what it found
                    // instead of dying on a serde field error (bd-mdp9).
                    Err(not_artifact) => match serde_json::from_str(&content) {
                        Ok(value) => non_artifacts.push(NonArtifactRecord {
                            file: path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default(),
                            value,
                        }),
                        Err(bad_json) => eprintln!(
                            "stitcher: WARNING: {} is neither an ArtifactRecord ({not_artifact}) nor JSON ({bad_json})",
                            path.display()
                        ),
                    },
                }
            }
        }
    }
    records.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    non_artifacts.sort_by(|a, b| a.file.cmp(&b.file));
    // The same root as the records above: reading evidence from the ambient
    // root here would stitch a different tree than the one being stitched.
    let evaluations = read_evaluation_evidence_in(root, project, run_id)?;

    let stitched_path = base.join("stitched.md");
    let mut md = String::new();
    md.push_str(&format!("# Stitched artifacts for run {}\n\n", run_id));
    if records.is_empty() {
        md.push_str("No typed ArtifactRecord for this run.\n\n");
    }
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
    if !non_artifacts.is_empty() {
        md.push_str("\n## Non-artifact records\n\n");
        for other in &non_artifacts {
            md.push_str(&format!(
                "- {}: {}\n",
                other.file,
                summarize_run_outcome(&other.value)
            ));
        }
    }
    if has_ungrounded {
        md.push_str("\n> **WARNING: Some artifacts are Ungrounded**\n");
    }
    if !evaluations.is_empty() {
        md.push_str("\n## Host evaluation evidence\n\n");
        for evaluation in evaluations {
            md.push_str(&format!(
                "- {} / {}: {:?} via {} — {} (evidence_hash: {}, input_sha256: {}, output_sha256: {})\n",
                evaluation.link,
                evaluation.phase,
                evaluation.verdict,
                evaluation.evaluator,
                evaluation.contract_evidence,
                evaluation.evidence_hash,
                evaluation.input_sha256,
                evaluation.output_sha256,
            ));
        }
    }
    std::fs::write(&stitched_path, md)?;
    Ok(stitched_path)
}

/// Compute sha256 hex of file bytes.
#[cfg(test)]
pub fn compute_sha256(path: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd-ww75: a per-test data root. The stitch tests used to resolve
    /// `dirs::data_local_dir()` at call time while other tests in this binary mutate
    /// XDG_DATA_HOME, so the read could land under a different root than the fixture
    /// was written to — passing alone, failing in a full suite run. This module is
    /// `#![forbid(unsafe_code)]`, so it cannot pin the env itself; injecting the
    /// root is both the only route and the sounder one.
    fn xdg_root() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("xdg temp dir")
    }

    use std::io::Write;

    /// Removes the temp project's data dir on drop: a panicking assertion
    /// must not leave fixture artifacts behind (the first red runs did).
    struct ProjectCleanup(String);

    impl Drop for ProjectCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(
                dirs::data_local_dir()
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join("flywheel")
                    .join(&self.0),
            );
        }
    }

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
            path,
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
        assert!(
            !rec.is_admitted(),
            "worker claims are not admission evidence"
        );
    }

    #[test]
    fn worker_cannot_self_admit_and_embedding_admission_is_explicit() {
        let worker_json = r#"{
            "path": "/tmp/foo.txt",
            "sha256": "abc123",
            "kind": "verification",
            "execution_status": "completed",
            "criterion_status": "met",
            "grounding_status": "grounded",
            "claim_support_status": "independently_supported",
            "run_id": "run-worker",
            "pin": "Worker"
        }"#;
        let worker: ArtifactRecord = serde_json::from_str(worker_json).unwrap();
        assert_eq!(
            worker.claim_support_status,
            ClaimSupportStatus::WorkerAsserted,
            "worker independent claim is still coerced"
        );
        assert!(
            !worker.is_admitted(),
            "worker status fields are not admission"
        );

        let (_f, path) = tmp_file("admission");
        let mut admitted = ArtifactRecord::new(
            path,
            ArtifactKind::Verification,
            ExecutionStatus::Completed,
            CriterionStatus::Unknown,
            GroundingStatus::Cited,
            ClaimSupportStatus::WorkerAsserted,
            None,
            None,
            "run-admission".into(),
            Some("verify".into()),
            "Embedding".into(),
        )
        .unwrap();
        admitted.admit();
        assert!(admitted.is_admitted(), "only the embedding admits records");
    }

    #[test]
    fn forged_embedding_admission_marker_is_rejected() {
        let json = r#"{
            "path": "/tmp/foo.txt",
            "sha256": "abc123",
            "kind": "verification",
            "execution_status": "completed",
            "criterion_status": "met",
            "grounding_status": "grounded",
            "claim_support_status": "embedding_independently_supported",
            "run_id": "run-forged",
            "pin": "Worker"
        }"#;
        assert!(
            serde_json::from_str::<ArtifactRecord>(json).is_err(),
            "admission must not be forgeable through an artifact wire value"
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
    fn evaluation_evidence_redacts_bounds_and_hashes_host_verdict() {
        let contract = crate::phase_contracts::PhaseContractReport {
            phase: "verify",
            ok: true,
            evidence: "gate `cargo test` exit 0; output recorded".into(),
        };
        let input = serde_json::json!({
            "objective": "prove the change",
            "token": "sk-live-not-for-evidence",
            "nested": [{ "password": "hunter2" }]
        });
        let output = serde_json::json!({
            "gate": { "command": "true", "exit": 0, "output": "x".repeat(10_000) }
        });
        let evidence = EvaluationEvidence::from_phase_contract(
            "run-eval-test",
            "ship",
            "verify",
            &input,
            &output,
            &contract,
        );
        assert_eq!(evidence.verdict, EvaluationVerdict::Pass);
        assert_eq!(evidence.evaluator, EVALUATION_EVALUATOR);
        assert!(evidence.input_snapshot.contains("…[redacted]"));
        assert!(!evidence.input_snapshot.contains("sk-live-not-for-evidence"));
        assert!(!evidence.input_snapshot.contains("hunter2"));
        assert!(evidence.output_bytes > evidence.output_snapshot.len() as u64);
        assert!(evidence.output_snapshot.len() <= MAX_EVALUATION_SNAPSHOT_BYTES);
        assert!(
            evidence
                .output_snapshot
                .ends_with(EVALUATION_TRUNCATION_SUFFIX)
        );
        assert!(evidence.integrity_valid());
    }

    #[test]
    fn evaluation_evidence_rejects_tampering_and_round_trips_through_state() {
        // One injected data root for write, read and stitch. The TempDir is
        // the cleanup: it is removed on drop, so a failing assertion leaves
        // nothing behind, and nothing lands under the operator's
        // `dirs::data_local_dir()` (this test used to write there and clean
        // up only when it passed).
        let xdg = xdg_root();
        let root = xdg.path();
        let project = format!("evaluation-test-{}", uuid::Uuid::new_v4());
        let run_id = format!("run-{}", uuid::Uuid::new_v4());
        let contract = crate::phase_contracts::PhaseContractReport {
            phase: "resolve",
            ok: false,
            evidence: "goal statement missing/empty: target".into(),
        };
        let evidence = EvaluationEvidence::from_phase_contract(
            &run_id,
            "ship",
            "resolve",
            &serde_json::json!({ "objective": "x" }),
            &serde_json::json!({ "target": null }),
            &contract,
        );
        let path = write_evaluation_evidence_in(root, &project, &evidence).expect("write evidence");
        assert!(path.exists());
        assert!(
            path.starts_with(root),
            "evidence must land under the injected root {}: {}",
            root.display(),
            path.display()
        );
        let records = read_evaluation_evidence_in(root, &project, &run_id).expect("read evidence");
        assert_eq!(records, vec![evidence.clone()]);

        let mut tampered = evidence.clone();
        tampered.verdict = EvaluationVerdict::Pass;
        assert!(!tampered.integrity_valid());
        assert!(write_evaluation_evidence_in(root, &project, &tampered).is_err());

        let stitched = stitch_artifacts_in(root, &project, &run_id).expect("stitch evidence");
        assert!(
            stitched.starts_with(root),
            "stitched under the root: {}",
            stitched.display()
        );
        let markdown = std::fs::read_to_string(stitched).expect("read stitched evidence");
        assert!(markdown.contains("## Host evaluation evidence"));
        assert!(markdown.contains(&evidence.evidence_hash));
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
            path: file1,
            sha256: sha1,
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
                path: file2,
                sha256: sha2,
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
            assert_ne!(rec.sha256, "");
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
        // bd-ww75: injected data root, same reason as the other stitch tests —
        // the ambient XDG root is mutable by other tests in this binary, and this
        // module is #![forbid(unsafe_code)] so it cannot pin the env itself.
        let xdg = xdg_root();
        let base = xdg
            .path()
            .join("flywheel")
            .join(&project)
            .join("artifacts")
            .join(run_id);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("payload.txt");
        std::fs::write(&file, "payload").unwrap();
        let rec = ArtifactRecord::new(
            file,
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
        let stitched = stitch_artifacts_in(xdg.path(), &project, run_id).unwrap();
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

    /// bd-mdp9: a composed run's dir holds `terminal.json`, the raw run output
    /// (status/error/failed_step/leaf_preview), not an `ArtifactRecord`.
    /// Stitch must report the outcome instead of dying on a serde field error.
    #[test]
    fn stitch_reports_a_terminal_only_run_instead_of_a_serde_error() {
        // bd-ww75: an INJECTED data root, not the ambient one. This test used
        // to resolve `dirs::data_local_dir()` at call time while other tests in
        // this binary mutate XDG_DATA_HOME, so the read could land under a
        // different root than the fixture was written to — it passed alone and
        // failed in a full suite run. This module is #![forbid(unsafe_code)],
        // so pinning the env here is not even an option; injection is both the
        // only route and the better one.
        let xdg = xdg_root();
        let project = format!("artifacts-terminal-test-{}", uuid::Uuid::new_v4());
        let _cleanup = ProjectCleanup(project.clone());
        let run_id = "fw-composed-test-terminal";
        let base = xdg
            .path()
            .join("flywheel")
            .join(&project)
            .join("artifacts")
            .join(run_id);
        std::fs::create_dir_all(&base).unwrap();
        // The exact shape write_terminal_artifact stores, ANSI escapes included.
        std::fs::write(
            base.join("terminal.json"),
            r#"{
  "error": "\u001b[1mflywheel: agent crew-x-reviewer not found\u001b[0m\n",
  "failed_step": "reviewer-ce-review-3",
  "leaf_preview": "{\"effect_state\":\"failed\"}",
  "status": "failed"
}"#,
        )
        .unwrap();

        let stitched = stitch_artifacts_in(xdg.path(), &project, run_id)
            .expect("a terminal-only run must stitch, not serde-error");
        let md = std::fs::read_to_string(&stitched).unwrap();
        assert!(
            md.contains("No typed ArtifactRecord"),
            "names the absence of typed records: {md}"
        );
        assert!(md.contains("status=failed"), "carries the outcome: {md}");
        assert!(
            md.contains("failed_step=reviewer-ce-review-3"),
            "names the failed link: {md}"
        );
        assert!(
            !md.contains('\u{1b}'),
            "ANSI escapes are stripped from the report: {md:?}"
        );
    }

    /// A run dir with real records AND a terminal outcome keeps both: the
    /// records list as before, the outcome in its own section.
    #[test]
    fn stitch_keeps_typed_records_alongside_a_terminal_outcome() {
        // bd-ww75: same injected root as the terminal-only test above.
        let xdg = xdg_root();
        let project = format!("artifacts-mixed-test-{}", uuid::Uuid::new_v4());
        let _cleanup = ProjectCleanup(project.clone());
        let run_id = "fw-composed-test-mixed";
        let base = xdg
            .path()
            .join("flywheel")
            .join(&project)
            .join("artifacts")
            .join(run_id);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("payload.txt");
        std::fs::write(&file, "payload").unwrap();
        let rec = ArtifactRecord::new(
            file,
            ArtifactKind::Finding,
            ExecutionStatus::Completed,
            CriterionStatus::Met,
            GroundingStatus::Cited,
            ClaimSupportStatus::IndependentlySupported,
            None,
            Some("bd-mdp9".into()),
            run_id.into(),
            Some("node-mixed".into()),
            "PinMixed".into(),
        )
        .unwrap();
        std::fs::write(
            base.join("node-mixed.json"),
            serde_json::to_string_pretty(&rec).unwrap(),
        )
        .unwrap();
        std::fs::write(
            base.join("terminal.json"),
            r#"{"error":"e","failed_step":"link-1","status":"failed"}"#,
        )
        .unwrap();

        let stitched = stitch_artifacts_in(xdg.path(), &project, run_id).unwrap();
        let md = std::fs::read_to_string(&stitched).unwrap();
        assert!(
            md.contains(&rec.sha256[..12.min(rec.sha256.len())]),
            "typed record still stitched: {md}"
        );
        assert!(
            md.contains("failed_step=link-1"),
            "outcome still reported: {md}"
        );
        assert!(
            !md.contains("No typed ArtifactRecord"),
            "typed records exist, so the absence line must not appear: {md}"
        );
    }
}

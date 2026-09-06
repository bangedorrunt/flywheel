//! Intent-overlap detector + tiered proposer (ADR-0011 Spec 3, P11.3b,
//! bd-foh8.4): the automation layer on top of P11.3a's manual collab.
//!
//! A **detector** turns overlapping `status:intent` declarations (the P11.2
//! intent board) into concrete split proposals WITHOUT a human/LLM in the
//! loop:
//!
//! - **Path classification** [`is_trivial_source`]: README/config/docs paths
//!   are TRIVIAL — overlapping them never triggers auto-coalesce (a doc edit
//!   does not warrant a coalition).
//! - **Overlap detection** [`intent_overlap`]: two intents overlap when their
//!   path globs intersect symmetrically (reuses the mailbox's `overlaps`).
//! - **Tiered proposer** [`propose_split`] — deterministic rules first:
//!   1. **split-by-directory** — overlapping globs with a common parent
//!      directory get a per-crew subpath split (the shared dir boundary);
//!   2. **sequence-by-dependency** — when one intent's paths are a strict
//!      subset (or the overlap is one-directional), sequence the two crews
//!      instead of splitting (first finisher releases, second proceeds);
//!   3. **merge** — identical path sets from the same goal → one crew holds
//!      the merged set, the other stands down.
//!      Rules are pure + unit-tested; the LLM planner arm (a P10.1 headless run)
//!      fires ONLY when a crew rejects the rule-split (wired by the caller).
//! - **Auto-coalesce gate** [`auto_coalesce`]: overlap on NON-TRIVIAL source
//!   between two crews that are NOT already in a coalition → the detector
//!   proposes an ad-hoc coalition + the split. Trivial overlap never
//!   coalesces.
//!
//! Functional core / imperative shell: everything here is pure; the
//! board read + proposal dispatch live in `coalition.rs` / `main.rs`.
//!
//! governed-by: ADR-0011 (Spec 3, Spec 4)

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use toron_core::mailbox::overlaps;

/// Path suffixes / name fragments that make a path TRIVIAL (doc/config —
/// overlapping them never triggers auto-coalesce).
pub const TRIVIAL_SUFFIXES: &[&str] = &[
    "README",
    "readme",
    ".md",
    ".txt",
    ".toml",
    ".yaml",
    ".yml",
    ".json",
    ".cfg",
    ".ini",
    "docs/",
    "doc/",
    "CHANGELOG",
    "LICENSE",
];

/// Is a path TRIVIAL (doc/config)? A path is trivial when any suffix marker
/// appears in its final component (or a `docs/`-style directory fragment).
/// Pure + unit-tested — the auto-coalesce gate.
pub fn is_trivial_source(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let last = normalized.rsplit('/').next().unwrap_or(&normalized);
    TRIVIAL_SUFFIXES
        .iter()
        .any(|s| last.contains(s) || normalized.contains(s))
}

/// One declared intent (the detector's view of the board).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentView {
    /// Agent (crew member) that declared the intent.
    pub agent: String,
    /// The path glob declared (`project:path` d-tag, project part stripped).
    pub path: String,
    /// Optional reason (e.g. the bead id).
    pub reason: String,
}

/// The kind of rule the proposer found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProposalKind {
    /// Split by directory boundary (each crew takes a subpath).
    SplitByDirectory,
    /// Sequence by dependency (one crew first, the other after).
    SequenceByDependency,
    /// Merge identical sets (one crew holds, the other stands down).
    Merge,
    /// No deterministic rule applies — escalate to the LLM planner.
    EscalateToPlanner,
}

/// One concrete proposal for an overlapping intent pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitProposal {
    /// The proposer's tier (`rule` | `llm`).
    pub tier: String,
    /// The applied rule.
    pub kind: ProposalKind,
    /// The two agents in the overlap.
    pub agents: [String; 2],
    /// Their overlapping paths.
    pub paths: [String; 2],
    /// The proposed split: agent → path glob each crew should hold.
    pub assignment: BTreeMap<String, String>,
    /// Human-readable rationale (why this rule).
    pub rationale: String,
}

/// The detector's full output for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DetectionReport {
    /// Overlapping pairs found on the board (trivial overlaps included —
    /// they are reported but never auto-coalesce).
    pub overlaps: Vec<Overlap>,
    /// Rule-split proposals for non-trivial overlaps.
    pub proposals: Vec<SplitProposal>,
    /// Pairs that would auto-coalesce (non-trivial overlap + crews not in a
    /// coalition — the caller forms the ad-hoc coalition).
    pub auto_coalesce: Vec<Overlap>,
}

/// One overlapping intent pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlap {
    /// First agent.
    pub a: String,
    /// Second agent.
    pub b: String,
    /// Agent A's path.
    pub path_a: String,
    /// Agent B's path.
    pub path_b: String,
    /// True when both paths are non-trivial source.
    pub non_trivial: bool,
}

/// Detect all overlapping pairs among the board's intents. Pure + O(n²) on
/// the board (boards are crew-sized; the mailbox already collapses by path).
pub fn detect_overlaps(intents: &[IntentView]) -> Vec<Overlap> {
    let mut out = Vec::new();
    for (i, a) in intents.iter().enumerate() {
        for b in intents.iter().skip(i + 1) {
            if a.agent == b.agent {
                continue; // same declarer — not an inter-crew overlap
            }
            if !overlaps(&a.path, &b.path) {
                continue;
            }
            out.push(Overlap {
                a: a.agent.clone(),
                b: b.agent.clone(),
                path_a: a.path.clone(),
                path_b: b.path.clone(),
                non_trivial: !is_trivial_source(&a.path) && !is_trivial_source(&b.path),
            });
        }
    }
    out
}

/// The tiered rule proposer: apply deterministic rules to one overlap,
/// returning a concrete split (or the escalate signal). Pure — the caller
/// decides LLM escalation on `EscalateToPlanner`.
pub fn propose_split(a: &IntentView, b: &IntentView) -> SplitProposal {
    let agents = [a.agent.clone(), b.agent.clone()];
    let paths = [a.path.clone(), b.path.clone()];
    let mut assignment = BTreeMap::new();

    // Rule 3: MERGE — identical (or symmetric-glob-equivalent) path sets.
    if a.path == b.path {
        // First declarer holds the merged set; the second stands down.
        assignment.insert(agents[0].clone(), a.path.clone());
        let rationale = format!(
            "identical path sets ({}) — {} holds the merged set, {} stands down",
            a.path,
            agents[0].clone(),
            agents[1].clone()
        );
        return SplitProposal {
            tier: "rule".into(),
            kind: ProposalKind::Merge,
            agents,
            paths,
            assignment,
            rationale,
        };
    }

    // Rule 2: SEQUENCE — strict containment: one intent's glob covers the
    // other's. The covering crew finishes first, then the covered one.
    if glob_covers(&a.path, &b.path) || glob_covers(&b.path, &a.path) {
        let (first, second) = if glob_covers(&a.path, &b.path) {
            (a, b)
        } else {
            (b, a)
        };
        assignment.insert(first.agent.clone(), first.path.clone());
        assignment.insert(second.agent.clone(), second.path.clone());
        let rationale = format!(
            "{}'s glob covers {}'s — sequence by dependency: {} first (releases), then {}",
            first.agent, second.agent, first.agent, second.agent
        );
        return SplitProposal {
            tier: "rule".into(),
            kind: ProposalKind::SequenceByDependency,
            agents,
            paths,
            assignment,
            rationale,
        };
    }

    // Rule 1: SPLIT BY DIRECTORY — common parent dir → each crew takes its
    // own subpath (the shared boundary is the split line).
    if let Some(parent) = common_parent(&a.path, &b.path) {
        assignment.insert(agents[0].clone(), a.path.clone());
        assignment.insert(agents[1].clone(), b.path.clone());
        let rationale = format!(
            "overlap under shared dir `{parent}` — split at the directory boundary: {} holds {}, {} holds {}",
            agents[0].clone(),
            a.path,
            agents[1].clone(),
            b.path
        );
        return SplitProposal {
            tier: "rule".into(),
            kind: ProposalKind::SplitByDirectory,
            agents,
            paths,
            assignment,
            rationale,
        };
    }

    // No deterministic rule — the LLM planner arm (P10.1 headless run).
    SplitProposal {
        tier: "llm".into(),
        kind: ProposalKind::EscalateToPlanner,
        agents,
        paths,
        assignment,
        rationale: "no deterministic rule applies (no common dir, no containment, no merge) — escalate to the LLM planner".into(),
    }
}

/// Run the full detector: overlaps + rule proposals + auto-coalesce gate.
///
/// `in_coalition(agent)` tells whether a crew already belongs to a coalition
/// (the caller resolves it from the coalition registry) — auto-coalesce only
/// fires for pairs where NEITHER crew is affiliated.
pub fn run_detector(
    intents: &[IntentView],
    in_coalition: &dyn Fn(&str) -> bool,
) -> DetectionReport {
    let overlaps = detect_overlaps(intents);
    let mut report = DetectionReport {
        overlaps: overlaps.clone(),
        ..Default::default()
    };
    for o in &overlaps {
        if !o.non_trivial {
            continue; // trivial overlap: reported, never coalesced
        }
        let a = IntentView {
            agent: o.a.clone(),
            path: o.path_a.clone(),
            reason: String::new(),
        };
        let b = IntentView {
            agent: o.b.clone(),
            path: o.path_b.clone(),
            reason: String::new(),
        };
        report.proposals.push(propose_split(&a, &b));
        // Auto-coalesce gate: non-trivial source + NEITHER crew affiliated.
        if !in_coalition(&o.a) && !in_coalition(&o.b) {
            report.auto_coalesce.push(o.clone());
        }
    }
    report
}

/// Does glob `outer` cover glob `inner` (every path `inner` matches is also
/// matched by `outer`)? Directory-prefix containment (the practical case for
/// intent splits); falls back to equality. Pure + unit-tested.
fn glob_covers(outer: &str, inner: &str) -> bool {
    if outer == inner {
        return true;
    }
    let a = outer.trim_end_matches('/');
    let b = inner.trim_end_matches('/');
    // `src/**` covers `src/foo.rs`; `src/` covers `src/foo.rs`.
    if let Some(stripped) = a.strip_suffix("**") {
        let dir = stripped.trim_end_matches('/');
        return b.starts_with(&format!("{dir}/")) || b.starts_with(dir);
    }
    b.starts_with(&format!("{a}/"))
}

/// The common parent directory of two path globs (the split boundary), if
/// any. Pure + unit-tested.
pub fn common_parent(a: &str, b: &str) -> Option<String> {
    let parts_a: Vec<&str> = a
        .trim_end_matches('/')
        .trim_end_matches('*')
        .split('/')
        .filter(|p| !p.is_empty() && *p != "*" && !p.contains('*'))
        .collect();
    let parts_b: Vec<&str> = b
        .trim_end_matches('/')
        .trim_end_matches('*')
        .split('/')
        .filter(|p| !p.is_empty() && *p != "*" && !p.contains('*'))
        .collect();
    let mut parent = String::new();
    for (pa, pb) in parts_a.iter().zip(parts_b.iter()) {
        if pa == pb {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(pa);
        } else {
            break;
        }
    }
    if parent.is_empty() {
        None
    } else {
        Some(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iv(agent: &str, path: &str) -> IntentView {
        IntentView {
            agent: agent.into(),
            path: path.into(),
            reason: String::new(),
        }
    }

    /// README/config/docs are trivial; source is not.
    #[test]
    fn trivial_source_classifier() {
        assert!(is_trivial_source("README.md"));
        assert!(is_trivial_source("docs/guide.md"));
        assert!(is_trivial_source("src/config.toml"));
        assert!(is_trivial_source("Cargo.toml"));
        assert!(!is_trivial_source("src/main.rs"));
        assert!(!is_trivial_source("src/mailbox/reservations.rs"));
        assert!(!is_trivial_source("crates/toron-core/src/lib.rs"));
    }

    /// Symmetric overlap detection, same-declarer excluded.
    #[test]
    fn overlap_detection_finds_pairs() {
        let board = vec![
            iv("oc", "src/**"),
            iv("pi", "src/foo.rs"),
            iv("oc", "docs/guide.md"), // same declarer — not a pair with itself
        ];
        let overlaps = detect_overlaps(&board);
        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].a, "oc");
        assert_eq!(overlaps[0].b, "pi");
        assert!(overlaps[0].non_trivial);
    }

    /// Trivial overlaps are reported but never coalesce.
    #[test]
    fn trivial_overlap_never_coalesces() {
        let board = vec![iv("oc", "docs/**"), iv("pi", "docs/guide.md")];
        let report = run_detector(&board, &|_| false);
        assert_eq!(report.overlaps.len(), 1);
        assert!(!report.overlaps[0].non_trivial);
        assert!(report.auto_coalesce.is_empty(), "trivial never coalesces");
        assert!(report.proposals.is_empty(), "no rule proposal for trivial");
    }

    /// Identical path sets → MERGE (first declarer holds, second stands down).
    #[test]
    fn identical_sets_merge() {
        let proposal = propose_split(&iv("oc", "src/foo.rs"), &iv("pi", "src/foo.rs"));
        assert_eq!(proposal.kind, ProposalKind::Merge);
        assert_eq!(proposal.tier, "rule");
        assert_eq!(
            proposal.assignment.get("oc").map(String::as_str),
            Some("src/foo.rs")
        );
        assert!(!proposal.assignment.contains_key("pi"), "pi stands down");
    }

    /// Strict glob containment → SEQUENCE (covering crew first).
    #[test]
    fn containment_sequences() {
        let proposal = propose_split(&iv("oc", "src/**"), &iv("pi", "src/foo.rs"));
        assert_eq!(proposal.kind, ProposalKind::SequenceByDependency);
        assert_eq!(
            proposal.assignment.get("oc").map(String::as_str),
            Some("src/**")
        );
        assert_eq!(
            proposal.assignment.get("pi").map(String::as_str),
            Some("src/foo.rs")
        );
        assert!(proposal.rationale.contains("oc"), "{proposal:?}");
    }

    /// Shared parent directory → SPLIT BY DIRECTORY.
    #[test]
    fn shared_dir_splits() {
        let proposal = propose_split(&iv("oc", "src/mailbox/**"), &iv("pi", "src/workflow/**"));
        assert_eq!(proposal.kind, ProposalKind::SplitByDirectory);
        assert!(proposal.rationale.contains("src"), "{proposal:?}");
    }

    /// No rule applies → ESCALATE to the LLM planner (no common parent dir,
    /// no containment, no merge).
    #[test]
    fn no_rule_escalates() {
        let proposal = propose_split(&iv("oc", "crates/*.rs"), &iv("pi", "src/*.rs"));
        assert_eq!(proposal.kind, ProposalKind::EscalateToPlanner);
        assert_eq!(proposal.tier, "llm");
    }

    /// Auto-coalesce fires only for non-trivial overlaps where neither crew
    /// is already in a coalition.
    #[test]
    fn auto_coalesce_gate() {
        let board = vec![iv("oc", "src/**"), iv("pi", "src/foo.rs")];
        let report = run_detector(&board, &|_| false);
        assert_eq!(
            report.auto_coalesce.len(),
            1,
            "unaffiliated non-trivial pair"
        );
        // One crew already affiliated → no auto-coalesce.
        let report2 = run_detector(&board, &|a| a == "oc");
        assert!(
            report2.auto_coalesce.is_empty(),
            "oc is in a coalition already"
        );
    }

    /// Common parent: `src/**` and `src/foo.rs` share `src`; unrelated
    /// trees share nothing.
    #[test]
    fn common_parent_finds_boundary() {
        assert_eq!(
            common_parent("src/**", "src/foo.rs").as_deref(),
            Some("src")
        );
        assert_eq!(
            common_parent("crates/a/**", "crates/b/**").as_deref(),
            Some("crates")
        );
        assert_eq!(common_parent("crates/a/**", "docs/guide.md"), None);
        assert_eq!(
            common_parent("src/foo.rs", "src/foo.rs").as_deref(),
            Some("src/foo.rs")
        );
    }

    /// Glob coverage: `src/**` covers `src/foo.rs`; `crates/a/**` does not
    /// cover `crates/b/**`.
    #[test]
    fn glob_cover_rules() {
        assert!(glob_covers("src/**", "src/foo.rs"));
        assert!(glob_covers("src", "src/foo.rs"));
        assert!(glob_covers("src/foo.rs", "src/foo.rs"));
        assert!(!glob_covers("crates/a/**", "crates/b/**"));
        assert!(!glob_covers("src/foo.rs", "src/bar.rs"));
    }

    /// Full detector output shape: overlaps + proposals + coalesce sets.
    #[test]
    fn detector_report_shape() {
        let board = vec![
            iv("oc", "src/**"),
            iv("pi", "src/workflow/**"),
            iv("gk", "src/README.md"),
        ];
        let report = run_detector(&board, &|_| false);
        assert_eq!(
            report.overlaps.len(),
            2,
            "oc-pi source overlap + oc-gk trivial overlap"
        );
        assert!(
            report
                .proposals
                .iter()
                .any(|p| p.kind == ProposalKind::SequenceByDependency),
            "{:?}",
            report.proposals
        );
        // gk's README overlap is trivial — not in the coalesce set.
        assert!(
            report.auto_coalesce.iter().all(|o| o.non_trivial),
            "coalesce set is non-trivial only: {:?}",
            report.auto_coalesce
        );
        assert_eq!(
            report.auto_coalesce.len(),
            1,
            "only the oc-pi source pair coalesces"
        );
    }
}

// governed-by: ADR-0009
//! Project scaffold for `flywheel init` (bd-bhri).
//!
//! Beyond the directory + beads tracker, `init` lays the project-scope
//! compound spine so every topology compounds from day one:
//!   - `.flywheel/stages.toml` — the project tier (seed < repo < user < project)
//!   - `.flywheel/standing-orders.md` — the project fence + compound orders
//!   - chiebukuro — the single learning store (record-lesson; no repo-side corpus)
//!
//! Re-runnable: existing files are never overwritten (reported as kept).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One scaffolded artifact: relative path + content.
pub struct Artifact {
    /// Path relative to the project root.
    pub rel: &'static str,
    /// File content written when the path does not yet exist.
    pub content: &'static str,
}

/// The project-scope compound scaffold (ADR-0009). Content is generic on
/// purpose — project specifics arrive via later edits, never init guesses.
pub fn artifacts() -> Vec<Artifact> {
    vec![
        Artifact {
            rel: ".flywheel/stages.toml",
            content: r#"# Project-authored stages (ADR-0022 §3.8 project tier; ADR-0009 ce- loop).
# Field-level override: a [stages.<name>] entry replaces the seed/user entry
# of the same name; a new name adds a stage. Later tiers win.
# Names are semantic only (p-code aliases retired by ADR-0009); `stage list`
# shows name + tier + role + phase. Valid phases:
# resolve | plan | implement | review | verify | compound.
#
# Example (delete or replace):
# [stages.my-check]
# name = "my-check"
# role = "reviewer"
# phase = "review"
# prompt = "Check ... against this repo's conventions. Quote file:line evidence."
"#,
        },
        Artifact {
            rel: ".flywheel/standing-orders.md",
            content: r#"# Project fence — crews (AUTHORED; `standing-orders regen` preserves this file).

## COMPOUND ORDERS (ADR-0009)

- **Terminal states** in every report: `terminal: done | clean_skip | blocked | failed`.
  clean_skip is a SUCCESS outcome (the gate judged the work N/A); a missing report is
  unknown — never assumed failed.
- **`settled:` decisions** (captain briefing go/no-go, crew shape, model picks) are not
  yours to improve. A real defect inside a settled approach still reports at full severity.
- **Open-mode compound**: after verified non-trivial work, run the learning gate once —
  `flywheel workflow run ce-compound --execute --inputs '{"source":"<what>"}'`. The
  three-part gate (non-obvious ∧ durable ∧ material) decides; most work clean-skips.
- **Independence**: two lenses reasoned in one pane are two perspectives, never
  "independently confirmed" — claim corroboration only across separately-dispatched contexts.
"#,
        },
    ]
}

/// The AGENTS.md pointer `init` suggests (printed, never auto-edited — the
/// repo owns its instruction file; CE's discoverability rule).
pub const AGENTS_POINTER: &str = "Learnings live in chiebukuro (ADR-0009) — `flywheel memory check-trap \"<keywords>\"` before planning; record-lesson the moment a root cause is confirmed, in every mode, bead or not.";

/// Write every missing scaffold artifact under `project_root`. Returns the
/// (created, kept) relative paths. Pure IO, no prompts, no overwrites.
/// Ensures the project layout first (owner contract: the scaffold owns
/// the initial tree, so every manifest dir exists even before its first
/// file lands).
pub fn scaffold(project_root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    crate::project_state::ensure_layout(project_root)?;
    let mut created = Vec::new();
    let mut kept = Vec::new();
    for artifact in artifacts() {
        let path = project_root.join(artifact.rel);
        if path.is_file() {
            kept.push(PathBuf::from(artifact.rel));
            continue;
        }
        crate::project_state::write_atomic(&path, artifact.content.as_bytes())
            .with_context(|| format!("scaffold {}", path.display()))?;
        created.push(PathBuf::from(artifact.rel));
    }
    Ok((created, kept))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// init scaffolds the three compound artifacts with the load-bearing
    /// markers (tier comment, compound orders, learning gate).
    #[test]
    fn scaffold_creates_compound_artifacts() {
        let dir = std::env::temp_dir().join(format!("fw-init-{}", uuid::Uuid::new_v4()));
        let (created, kept) = scaffold(&dir).expect("scaffold");
        assert_eq!(created.len(), 2, "two artifacts: {created:?}");
        assert!(kept.is_empty(), "fresh dir keeps nothing");
        let stages = std::fs::read_to_string(dir.join(".flywheel/stages.toml")).unwrap();
        assert!(stages.contains("compound"), "phase vocabulary named");
        let orders = std::fs::read_to_string(dir.join(".flywheel/standing-orders.md")).unwrap();
        assert!(
            orders.contains("COMPOUND ORDERS"),
            "fence carries compound orders"
        );
        assert!(orders.contains("clean_skip"), "terminal states present");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Re-running init never overwrites project-authored content.
    #[test]
    fn scaffold_rerun_keeps_existing() {
        let dir = std::env::temp_dir().join(format!("fw-init-keep-{}", uuid::Uuid::new_v4()));
        let before = scaffold(&dir).expect("first scaffold");
        assert_eq!(before.0.len(), 2);
        std::fs::write(dir.join(".flywheel/stages.toml"), "# MINE\n").unwrap();
        let (created, kept) = scaffold(&dir).expect("second scaffold");
        assert!(created.is_empty(), "nothing created twice");
        assert_eq!(kept.len(), 2, "both kept: {kept:?}");
        let stages = std::fs::read_to_string(dir.join(".flywheel/stages.toml")).unwrap();
        assert_eq!(stages, "# MINE\n", "authored content untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

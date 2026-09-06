// governed-by: ADR-0009
//! Project scaffold for `flywheel init` (bd-bhri).
//!
//! Beyond the directory + beads tracker, `init` lays the project-scope
//! compound spine so every topology compounds from day one:
//!   - `.flywheel/stages.toml` — the project tier (seed < repo < user < project)
//!   - `.flywheel/standing-orders.md` — the project fence + compound orders
//!   - `docs/learnings/README.md` — the learnings corpus convention
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
        Artifact {
            rel: "docs/learnings/README.md",
            content: r#"# Learnings corpus (ADR-0009)

One learning per file, git-tracked, `rg`-able. This is the compound-engineering **return
arrow**: `ce-brainstorm` / the wave GATE / the brief's LEARNINGS field read it before work;
the `ce-compound` gate writes to it after verified work; `ce-sweep` maintains it.

## The gate (all three must hold — completion alone never qualifies)

1. **Non-obvious** — the reasoning is not recoverable from the final code, tests, types, or
   comments.
2. **Durable** — an invariant, root cause, rejected approach, or convention useful beyond the
   exact diff.
3. **Material** — forgetting it would plausibly cause recurrence or substantial rediscovery.

Counterfactual: if the doc vanished, would the next crew redo this investigation? If not,
`clean_skip`.

## Schema

```markdown
---
track: bug            # bug | knowledge — decides the sections
module: src/crew.rs   # open vocabulary, reuse what the corpus already uses
problem_type: integration_issue
tags: [dispatch]
bead: bd-xxxx         # provenance
created: 2026-09-06
---
## Symptoms            # bug track: Symptoms / What Didn't Work / Solution /
## What didn't work    #         Why This Works / Prevention
## Solution
## Prevention
# knowledge track instead: Context / Guidance / When to Apply
```

- High overlap with an existing entry ⇒ UPDATE it (stamp `last_updated`), never duplicate.
- A learning contradicting standing-orders/AGENTS.md guidance **outranks ordinary staleness** —
  resolve by which side current code follows; report a wrong guidance file, never edit it here
  (`ce-sweep` owns that).
- The learning ships in the SAME commit as the work that produced it.

## Maintenance

`flywheel workflow run ce-sweep --execute --inputs '{"project":"<slug>"}'` —
Keep / Update / Consolidate / Replace / Delete, evidence per verdict.
"#,
        },
    ]
}

/// The AGENTS.md pointer `init` suggests (printed, never auto-edited — the
/// repo owns its instruction file; CE's discoverability rule).
pub const AGENTS_POINTER: &str = "Learnings live in `docs/learnings/` (ADR-0009) — `rg -il \"<keywords>\" docs/learnings/` before planning; the ce-compound gate decides capture after verified work.";

/// Write every missing scaffold artifact under `project_root`. Returns the
/// (created, kept) relative paths. Pure IO, no prompts, no overwrites.
pub fn scaffold(project_root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut created = Vec::new();
    let mut kept = Vec::new();
    for artifact in artifacts() {
        let path = project_root.join(artifact.rel);
        if path.is_file() {
            kept.push(PathBuf::from(artifact.rel));
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&path, artifact.content)
            .with_context(|| format!("write {}", path.display()))?;
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
        assert_eq!(created.len(), 3, "three artifacts: {created:?}");
        assert!(kept.is_empty(), "fresh dir keeps nothing");
        let stages = std::fs::read_to_string(dir.join(".flywheel/stages.toml")).unwrap();
        assert!(stages.contains("compound"), "phase vocabulary named");
        let orders = std::fs::read_to_string(dir.join(".flywheel/standing-orders.md")).unwrap();
        assert!(
            orders.contains("COMPOUND ORDERS"),
            "fence carries compound orders"
        );
        assert!(orders.contains("clean_skip"), "terminal states present");
        let readme = std::fs::read_to_string(dir.join("docs/learnings/README.md")).unwrap();
        assert!(readme.contains("clean_skip"), "learning gate present");
        assert!(readme.contains("ce-sweep"), "maintenance path present");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Re-running init never overwrites project-authored content.
    #[test]
    fn scaffold_rerun_keeps_existing() {
        let dir = std::env::temp_dir().join(format!("fw-init-keep-{}", uuid::Uuid::new_v4()));
        let before = scaffold(&dir).expect("first scaffold");
        assert_eq!(before.0.len(), 3);
        std::fs::write(dir.join(".flywheel/stages.toml"), "# MINE\n").unwrap();
        let (created, kept) = scaffold(&dir).expect("second scaffold");
        assert!(created.is_empty(), "nothing created twice");
        assert_eq!(kept.len(), 3, "all three kept: {kept:?}");
        let stages = std::fs::read_to_string(dir.join(".flywheel/stages.toml")).unwrap();
        assert_eq!(stages, "# MINE\n", "authored content untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

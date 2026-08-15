// governed-by: ADR-0019
// Wave-3 standing-orders generator (bd-w3-standing-orders-admu, ADR-0019
// Spec 2): the GLOBAL standing orders are GENERATED from the principle
// registry + the wave-1 criticals — numbered lines, one constraint each,
// names only (never pasted leaves). Project fences at
// `<project>/.flywheel/standing-orders.md` are AUTHORED and are preserved
// byte-identical on every regenerate (the layering guarantee: regen writes
// ONLY the global path). The brief's STANDING field references the paths
// (w1-brief contract); nothing is ever pasted into a dispatch.
//
// CLI wiring (`flywheel standing-orders regen`) lands in main.rs after the
// principle-registry CLI (bd-w3-registry-cli-21qb) — see the channel note.

use crate::principles::PRINCIPLE_NAMES;

/// The wave-1 critical constraints that MUST appear in the generated global
/// standing orders (ADR-0019 Spec 1/2/3 + the wave-1 contract). One
/// constraint per line — these are the load-bearing lifecycle rules that
/// replaced the 2KB lifecycle novel (`bead_prompt`/`resume_prompt`).
pub const WAVE1_CRITICALS: [&str; 5] = [
    "claim before work — never work an unclaimed bead",
    "reserve before edit — hold an exclusive reservation on every path you write",
    "gate-evidence close — close only with a legal ledger verdict bound to the commit",
    "commit-before-close — the commit message carries the bead id",
    "no worktree — the loop and playbooks never create or use worktrees",
];

/// Generate the GLOBAL standing orders (`~/.config/flywheel/standing-orders.md`):
/// numbered lines, one constraint each — the wave-1 criticals first, then the
/// 21 canonical principle names (names only; leaves live in the registry).
pub fn global_standing_orders() -> String {
    let mut out = String::new();
    out.push_str("# Standing orders — generated from the principle registry (ADR-0019).\n");
    out.push_str("# Regenerate with `flywheel standing-orders regen`. Project fences at\n");
    out.push_str("# <project>/.flywheel/standing-orders.md are AUTHORED and preserved.\n");
    let mut n = 0usize;
    for c in WAVE1_CRITICALS {
        n += 1;
        out.push_str(&format!("{n}. {c}\n"));
    }
    for name in PRINCIPLE_NAMES {
        n += 1;
        out.push_str(&format!("{n}. {name}\n"));
    }
    out
}

/// Regen the GLOBAL standing orders. Writes ONLY `global_path` — project
/// fences (`<project>/.flywheel/standing-orders.md`) are never in the write
/// set, which IS the layering guarantee: an authored fence survives every
/// regenerate byte-identical. Callers that want the project files verified
/// (preflight) read them separately — never write them here.
pub fn regen_global(global_path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = global_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(global_path, global_standing_orders())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every non-comment line is `N. <one constraint>` — numbered, one
    /// constraint each, no empty lines.
    #[test]
    fn global_is_numbered_one_constraint_per_line() {
        let text = global_standing_orders();
        let lines: Vec<&str> = text.lines().collect();
        assert!(!lines.is_empty());
        let mut n = 0usize;
        for line in lines {
            if line.starts_with('#') {
                continue;
            }
            n += 1;
            let rest = line.strip_prefix(&format!("{n}. "));
            assert!(rest.is_some(), "line {n} numbered: {line:?}");
            let constraint = rest.unwrap();
            assert!(!constraint.trim().is_empty(), "line {n} has a constraint");
            assert!(
                !constraint.contains("  "),
                "line {n} is ONE constraint: {constraint:?}"
            );
        }
        assert_eq!(
            n,
            WAVE1_CRITICALS.len() + PRINCIPLE_NAMES.len(),
            "26 numbered lines"
        );
    }

    /// The wave-1 criticals are present verbatim (acceptance).
    #[test]
    fn global_contains_wave1_criticals() {
        let text = global_standing_orders();
        for critical in WAVE1_CRITICALS {
            assert!(text.contains(critical), "global carries: {critical}");
        }
        for phrase in [
            "claim before work",
            "reserve before edit",
            "gate-evidence close",
            "commit-before-close",
            "no worktree",
        ] {
            assert!(
                text.contains(phrase),
                "global carries the critical {phrase:?}"
            );
        }
    }

    /// Every canonical principle name appears as its own numbered line —
    /// names only, never a pasted leaf.
    #[test]
    fn global_contains_every_principle_name() {
        let text = global_standing_orders();
        for name in PRINCIPLE_NAMES {
            assert!(
                text.lines().any(|l| l.ends_with(&format!(" {name}"))),
                "principle {name} has its own line"
            );
        }
        // names only: the leaf texts are NOT in the file
        assert!(!text.contains("minimize-reader-load —"), "no leaf text");
    }

    /// THE LAYERING GUARANTEE: regen writes only the global path — an
    /// authored project fence is byte-identical (content AND last-modified)
    /// after a regenerate.
    #[test]
    fn regen_preserves_project_fence_byte_identical() {
        let dir = std::env::temp_dir().join(format!("standing-orders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let fence_dir = dir.join("proj").join(".flywheel");
        std::fs::create_dir_all(&fence_dir).unwrap();
        let fence = fence_dir.join("standing-orders.md");
        let authored =
            "# PROJECT FENCES — hand-authored, never clobbered.\n1. this project requires X\n";
        std::fs::write(&fence, authored).unwrap();
        // record mtime + content
        let before_meta = std::fs::metadata(&fence).unwrap();
        let before_modified = before_meta.modified().unwrap();
        let global = dir.join("config").join("standing-orders.md");
        // regen (twice — regen is idempotent and never touches the fence)
        regen_global(&global).unwrap();
        regen_global(&global).unwrap();
        assert_eq!(
            std::fs::read_to_string(&fence).unwrap(),
            authored,
            "authored fence byte-identical after regen"
        );
        let after_meta = std::fs::metadata(&fence).unwrap();
        assert_eq!(
            before_modified,
            after_meta.modified().unwrap(),
            "fence not even touched (mtime unchanged)"
        );
        // the global was (re)generated with the full content
        let g = std::fs::read_to_string(&global).unwrap();
        assert!(g.contains("claim before work"), "global regenerated");
        assert!(g.contains("prove-it-works"), "principle names present");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regen creates the global path's parent directories.
    #[test]
    fn regen_creates_global_with_parents() {
        let dir = std::env::temp_dir().join(format!("standing-orders-nest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let global = dir.join("a").join("b").join("standing-orders.md");
        regen_global(&global).unwrap();
        assert!(global.exists(), "global written through created parents");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

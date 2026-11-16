//! One owner for the project `.flywheel/` layout (wave6-b, pruned wave-heal:
//! only helpers with a live in-scope writer survive; stranded paths stay
//! inventoried in the manifest, not as code).
//!
//! Path helpers, the layout manifest, and atomic writes live here
//! so the dozen writers across the tree share one vocabulary. Paths and
//! formats are byte-identical to the ad-hoc joins this replaces; only the
//! construction site moves.
//!
//! [`write_atomic`] writes to a same-dir temp file and renames over the
//! target (the spawn-registry/cron-checkpoint discipline): a crash leaves
//! either the old file or the new one, never a partial write. Callers that
//! need best-effort semantics keep `let _ =` — the helper only reports.

use std::fmt;
use std::path::{Path, PathBuf};

/// A write/read failure that names the path it happened on.
#[derive(Debug)]
pub struct ProjectStateError {
    /// What was attempted: `"create_dir"`, `"write"`, or `"rename"`.
    pub op: &'static str,
    /// The file (or directory) the op targeted.
    pub path: PathBuf,
    /// The underlying I/O failure.
    pub source: std::io::Error,
}

impl fmt::Display for ProjectStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.op, self.path.display(), self.source)
    }
}

impl std::error::Error for ProjectStateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The project overlay root: `<root>/.flywheel`.
pub fn dot_flywheel(root: &Path) -> PathBuf {
    root.join(".flywheel")
}

/// `<root>/.flywheel/standing-orders.md` (project fence; read by
/// preflight, authored, kept).
pub fn standing_orders_md(root: &Path) -> PathBuf {
    dot_flywheel(root).join("standing-orders.md")
}

/// `<root>/.flywheel/agent-names.json` (project-local pane→name bindings).
pub fn agent_names_json(root: &Path) -> PathBuf {
    dot_flywheel(root).join("agent-names.json")
}

/// `<root>/.flywheel/judgment-packs/` (project tier; read by
/// judgment::find_pack_for).
pub fn judgment_packs_dir(root: &Path) -> PathBuf {
    dot_flywheel(root).join("judgment-packs")
}

/// `<root>/.flywheel/agent-state/` (per-agent context files).
pub fn agent_state_dir(root: &Path) -> PathBuf {
    dot_flywheel(root).join("agent-state")
}

/// `<root>/.flywheel/receipts/` (bead receipts).
pub fn receipts_dir(root: &Path) -> PathBuf {
    dot_flywheel(root).join("receipts")
}

/// `<root>/.flywheel/crons/` (project cron tier).
/// The wave artifact dir (`.flywheel/wave/`): the gate writes
/// `<label>-ready.json` here and DISPATCH reads it (ADR-0021 D4.5).
pub fn wave_dir(root: &Path) -> PathBuf {
    root.join(".flywheel").join("wave")
}

pub fn crons_dir(root: &Path) -> PathBuf {
    dot_flywheel(root).join("crons")
}

/// One receipt file: `<root>/.flywheel/receipts/<bead>-<sha>.<kind>.txt`.
/// `kind` is the gate name (`unit-test-verified`, `command-verified`, …).
pub fn receipt_path(root: &Path, bead: &str, sha: &str, kind: &str) -> PathBuf {
    receipts_dir(root).join(format!("{bead}-{sha}.{kind}.txt"))
}

/// File or directory manifest row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    File,
    Dir,
}

/// One known `.flywheel/` path: relative location, shape, and the writer
/// (or writer lane) that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutEntry {
    /// Path relative to `<root>/.flywheel` (trailing `/` marks dirs).
    pub rel: &'static str,
    pub kind: LayoutKind,
    /// Owning writer; `stranded:main/cmd` marks lanes this bead must not touch.
    pub owner: &'static str,
}

/// The known project layout (wave6-b manifest): inventory, including
/// stranded paths no in-scope writer may touch. `ensure_layout` creates
/// every [`LayoutKind::Dir`] entry; live files go through the helpers above.
pub const LAYOUT: &[LayoutEntry] = &[
    LayoutEntry {
        rel: "stages.toml",
        kind: LayoutKind::File,
        owner: "stages::upsert_stage_in_file (+init_scaffold seed)",
    },
    LayoutEntry {
        rel: "standing-orders.md",
        kind: LayoutKind::File,
        owner: "project-authored (init_scaffold seed; global regen in standing_orders)",
    },
    LayoutEntry {
        rel: "compose-allowlist.toml",
        kind: LayoutKind::File,
        owner: "operator-authored; stranded:main.rs load_compose_allowlist",
    },
    LayoutEntry {
        rel: "agent-names.json",
        kind: LayoutKind::File,
        owner: "state::save_agent_names",
    },
    LayoutEntry {
        rel: "workflows/",
        kind: LayoutKind::Dir,
        owner: "workflow_catalog::write_workflow_yaml",
    },
    LayoutEntry {
        rel: "judgment-packs/",
        kind: LayoutKind::Dir,
        owner: "operator-authored packs; read by judgment::pack_dirs",
    },
    LayoutEntry {
        rel: "agent-state/",
        kind: LayoutKind::Dir,
        owner: "agentstate::state_file_path",
    },
    LayoutEntry {
        rel: "receipts/",
        kind: LayoutKind::Dir,
        owner: "verdict::write_receipt",
    },
    LayoutEntry {
        rel: "plans/",
        kind: LayoutKind::Dir,
        owner: "stranded:main.rs plans path",
    },
    LayoutEntry {
        rel: "reports/",
        kind: LayoutKind::Dir,
        owner: "crew reports (no Rust writer in tree)",
    },
    LayoutEntry {
        rel: "wave/",
        kind: LayoutKind::Dir,
        owner: "wave workflows (agent-written markers)",
    },
    LayoutEntry {
        rel: "crons/",
        kind: LayoutKind::Dir,
        owner: "cron::project_crons_dir",
    },
    LayoutEntry {
        rel: "loop-run-verify-failed/",
        kind: LayoutKind::Dir,
        owner: "stranded:main.rs failed-head marker",
    },
    LayoutEntry {
        rel: "reopened/",
        kind: LayoutKind::Dir,
        owner: "stranded:main.rs reopen marker",
    },
];

/// Create every manifest directory under `<root>/.flywheel`. Idempotent
/// (`create_dir_all`); files are never touched.
pub fn ensure_layout(root: &Path) -> Result<(), ProjectStateError> {
    for entry in LAYOUT {
        if entry.kind != LayoutKind::Dir {
            continue;
        }
        let dir = dot_flywheel(root).join(entry.rel);
        std::fs::create_dir_all(&dir).map_err(|source| ProjectStateError {
            op: "create_dir",
            path: dir,
            source,
        })?;
    }
    Ok(())
}

/// Atomically replace `path` with `bytes`: create the parent dir, write a
/// same-dir temp file (`<name>.tmp`), rename over the target. A crash
/// leaves the old file or the new one, never a partial write.
/// Best-effort callers keep `let _ =` — the helper only reports.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ProjectStateError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| ProjectStateError {
            op: "create_dir",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let file_name = path
        .file_name()
        .map(|n| format!("{}.tmp", n.to_string_lossy()))
        .unwrap_or_else(|| "flywheel.tmp".to_string());
    let tmp = path.with_file_name(file_name);
    std::fs::write(&tmp, bytes).map_err(|source| ProjectStateError {
        op: "write",
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| ProjectStateError {
        op: "rename",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest covers every known project file from the wave6-b brief,
    /// each exactly once.
    #[test]
    fn manifest_covers_the_known_layout() {
        let mut rels: Vec<&str> = LAYOUT.iter().map(|e| e.rel).collect();
        rels.sort_unstable();
        for known in [
            "stages.toml",
            "standing-orders.md",
            "workflows/",
            "judgment-packs/",
            "agent-state/",
            "receipts/",
            "plans/",
            "compose-allowlist.toml",
            "agent-names.json",
            "reports/",
            "wave/",
            "crons/",
            "loop-run-verify-failed/",
            "reopened/",
        ] {
            assert!(
                rels.contains(&known),
                "manifest must cover {known}: {rels:?}"
            );
        }
        let unique: std::collections::HashSet<&str> = rels.iter().copied().collect();
        assert_eq!(unique.len(), rels.len(), "no duplicate manifest rows");
    }

    /// Path helpers join the project root, byte-identical to the ad-hoc joins.
    #[test]
    fn helpers_build_the_documented_paths() {
        let root = Path::new("/proj");
        assert_eq!(
            standing_orders_md(root),
            PathBuf::from("/proj/.flywheel/standing-orders.md")
        );
        assert_eq!(
            agent_names_json(root),
            PathBuf::from("/proj/.flywheel/agent-names.json")
        );
        assert_eq!(
            agent_state_dir(root),
            PathBuf::from("/proj/.flywheel/agent-state")
        );
        assert_eq!(crons_dir(root), PathBuf::from("/proj/.flywheel/crons"));
        assert_eq!(
            judgment_packs_dir(root),
            PathBuf::from("/proj/.flywheel/judgment-packs")
        );
        assert_eq!(
            receipts_dir(root),
            PathBuf::from("/proj/.flywheel/receipts")
        );
        assert_eq!(
            receipt_path(root, "bd-1", "abc", "unit-test-verified"),
            PathBuf::from("/proj/.flywheel/receipts/bd-1-abc.unit-test-verified.txt")
        );
    }

    /// `ensure_layout` creates every manifest dir and is idempotent.
    #[test]
    fn ensure_layout_creates_dirs_and_reruns_clean() {
        let dir = std::env::temp_dir().join(format!("flywheel-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_layout(&dir).expect("first ensure");
        for entry in LAYOUT {
            if entry.kind == LayoutKind::Dir {
                assert!(
                    dot_flywheel(&dir).join(entry.rel).is_dir(),
                    "dir exists: {}",
                    entry.rel
                );
            }
        }
        ensure_layout(&dir).expect("second ensure is a no-op");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed write leaves no partial file and no temp droppings.
    #[test]
    fn write_atomic_fails_clean_without_leftovers() {
        let dir = std::env::temp_dir().join(format!("flywheel-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("base dir");
        // Round-trip first: bytes land intact.
        let path = dir.join("note.txt");
        write_atomic(&path, b"hello").expect("write works");
        assert_eq!(std::fs::read(&path).expect("read back"), b"hello");
        // Break the parent (a file where a dir must be): the write fails,
        // the old file is untouched, and no *.tmp survives.
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, b"x").expect("blocker");
        let bad = blocker.join("note.txt");
        let err = write_atomic(&bad, b"new").expect_err("must fail");
        assert!(
            err.to_string().contains(&blocker.display().to_string()),
            "the error names the path: {err}"
        );
        assert_eq!(std::fs::read(&path).expect("old file intact"), b"hello");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp droppings: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// governed-by: ADR-0012
//! State migration (bd-asr2.6, D2 inventory): adopt a dispatcher's state
//! dir byte-identically. Embedded and daemon hosts share one layout, so a
//! same-host adoption is a copy; the inventory below is the contract both
//! sides honor (pid, gen, project, unanswered, audit, wake, journals plus
//! retained generations).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Inventory filename predicates: (dir, matches).
fn inventory_match(dir: &str, name: &str) -> bool {
    match dir {
        "doorbell" => {
            (name.ends_with(".pid")
                || name.ends_with(".gen")
                || name.ends_with(".project")
                || name.ends_with(".unanswered")
                || name.ends_with(".audit.jsonl"))
                && !name.contains("..")
        }
        "journals" => {
            name.starts_with("doorbell-journal-")
                && name.ends_with(".jsonl")
                && !name.contains("..")
        }
        "" => name.starts_with("doorbell-wake-") && name.ends_with(".json") && !name.contains(".."),
        _ => false,
    }
}

/// Copy the inventory from `from` to `to`, returning copied relative paths.
/// Byte-identical (no conversion); existing destination files are refused,
/// never merged — adoption is explicit, not a union.
pub fn migrate(from: &Path, to: &Path) -> Result<Vec<String>> {
    let mut copied = Vec::new();
    for dir in ["doorbell", "journals", ""] {
        let src_dir = if dir.is_empty() {
            from.to_path_buf()
        } else {
            from.join(dir)
        };
        let Ok(entries) = std::fs::read_dir(&src_dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !inventory_match(dir, &name) {
                continue;
            }
            let rel: PathBuf = if dir.is_empty() {
                PathBuf::from(&name)
            } else {
                PathBuf::from(dir).join(&name)
            };
            let dst = to.join(&rel);
            if dst.exists() {
                anyhow::bail!("refusing to overwrite existing {}", dst.display());
            }
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            let bytes = std::fs::read(entry.path()).with_context(|| "read source")?;
            std::fs::write(&dst, &bytes).with_context(|| format!("write {}", dst.display()))?;
            // Byte-identical proof at copy time, not later.
            let back = std::fs::read(&dst).with_context(|| "re-read")?;
            if back != bytes {
                anyhow::bail!("copy mismatch on {}", dst.display());
            }
            copied.push(rel.display().to_string());
        }
    }
    copied.sort();
    Ok(copied)
}

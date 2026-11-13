//! __plan-fence internal command: br lint + dep cycles fence and bead-set digest convergence (ADR-0021 D7).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::br::run_br;
use crate::output::print_json;

/// Subcommands for `flywheel __plan-fence`.
#[derive(Subcommand, Clone, Debug)]
pub enum PlanFenceAction {
    /// Exit 0 only when `br lint` is clean and `br dep cycles` is 0.
    Fence {
        /// Print one JSON object (`{"check","ok","detail"...}`).
        #[arg(long)]
        json: bool,
    },
    /// Compare the open bead-set digest with the stored state.
    Converged {
        /// Path holding the previous digest (written on change).
        #[arg(long)]
        state: PathBuf,
        /// Print one JSON object (`{"check","ok","detail"...}`).
        #[arg(long)]
        json: bool,
    },
}

/// Pure decision core: the fence passes only when both checks are clean.
pub fn fence_verdict(lint_issues: usize, cycle_count: usize) -> bool {
    lint_issues == 0 && cycle_count == 0
}

/// Pure decision core: `true` only when the new digest equals the stored one.
pub fn converged(new_digest: &str, previous: Option<&str>) -> bool {
    Some(new_digest) == previous
}

/// Pure decision core: SHA-256 hex digest over the normalized open bead set.
///
/// Normalization: sorted by id; only id, title, description, status,
/// dependencies (sorted, empty when absent). Accepts either the full
/// `br list --status open --json` envelope (`{"issues":[...]}`) or a bare
/// array. Volatile fields (`updated_at`, counts, revisions) never enter the
/// digest, so a round that changes nothing looks unchanged.
pub fn digest(beads_json: &Value) -> String {
    let issues: &[Value] = match beads_json {
        Value::Array(items) => items,
        Value::Object(_) => beads_json
            .get("issues")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };
    let mut rows: Vec<Value> = issues
        .iter()
        .map(|issue| {
            let mut deps: Vec<String> = match issue.get("dependencies") {
                Some(Value::Array(deps)) => deps
                    .iter()
                    .filter_map(|d| match d {
                        Value::String(s) => Some(s.clone()),
                        Value::Object(map) => map
                            .get("depends_on_id")
                            .or_else(|| map.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            deps.sort();
            deps.dedup();
            json!({
                "id": issue.get("id").and_then(Value::as_str).unwrap_or(""),
                "title": issue.get("title").and_then(Value::as_str).unwrap_or(""),
                "description": issue.get("description").and_then(Value::as_str).unwrap_or(""),
                "status": issue.get("status").and_then(Value::as_str).unwrap_or(""),
                "dependencies": deps,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .cmp(&b.get("id").and_then(Value::as_str))
    });
    let canonical = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_owned());
    sha256_hex(canonical.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn lint_issue_count(v: &Value) -> usize {
    if let Some(n) = v.get("total").and_then(Value::as_u64) {
        return n as usize;
    }
    if let Some(n) = v.get("issues").and_then(Value::as_u64) {
        return n as usize;
    }
    v.get("results")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

fn lint_offenders(v: &Value) -> Vec<String> {
    v.get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(|r| {
                    r.get("id")
                        .or_else(|| r.get("issue_id"))
                        .or_else(|| r.get("bead"))
                        .or_else(|| r.get("bead_id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cycle_count(v: &Value) -> usize {
    v.get("count")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or_else(|| {
            v.get("cycles")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
        })
}

fn cycle_offenders(v: &Value) -> Vec<String> {
    v.get("cycles")
        .and_then(Value::as_array)
        .map(|cycles| {
            let mut ids: Vec<String> = Vec::new();
            for c in cycles {
                collect_id_strings(c, &mut ids);
            }
            ids.sort();
            ids.dedup();
            ids
        })
        .unwrap_or_default()
}

fn collect_id_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_id_strings(item, out);
            }
        }
        Value::Object(map) => {
            for key in ["id", "issue_id", "depends_on_id", "bead", "bead_id"] {
                if let Some(s) = map.get(key).and_then(Value::as_str) {
                    out.push(s.to_owned());
                }
            }
        }
        _ => {}
    }
}

fn dependencies_for(bead_id: &str) -> Vec<String> {
    let raw = match run_br(None, &["dep", "list", bead_id, "--json"]) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    let v: Value = match serde_json::from_str(raw.trim()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let edges: &[Value] = match &v {
        Value::Array(items) => items,
        Value::Object(_) => v
            .get("edges")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };
    let mut deps: Vec<String> = edges
        .iter()
        .filter_map(|e| {
            e.get("depends_on_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    deps.sort();
    deps.dedup();
    deps
}

/// `flywheel __plan-fence fence [--json]`: exit 0 only when `br lint`
/// reports zero issues AND `br dep cycles` reports count 0.
pub(crate) fn cmd_fence(json: bool) -> Result<()> {
    let lint_raw = run_br(None, &["lint", "--json"])?;
    let lint_json: Value = serde_json::from_str(lint_raw.trim())
        .map_err(|e| anyhow::anyhow!("br lint bad JSON: {e}: {lint_raw}"))?;
    let cycles_raw = run_br(None, &["dep", "cycles", "--json"])?;
    let cycles_json: Value = serde_json::from_str(cycles_raw.trim())
        .map_err(|e| anyhow::anyhow!("br dep cycles bad JSON: {e}: {cycles_raw}"))?;
    let lint_n = lint_issue_count(&lint_json);
    let cycle_n = cycle_count(&cycles_json);
    let ok = fence_verdict(lint_n, cycle_n);
    let mut failed: Vec<String> = Vec::new();
    if lint_n > 0 {
        failed.push("lint".to_owned());
    }
    if cycle_n > 0 {
        failed.push("cycles".to_owned());
    }
    if json {
        print_json(&json!({
            "check": "fence",
            "ok": ok,
            "detail": {
                "lint_issues": lint_n,
                "lint_offenders": lint_offenders(&lint_json),
                "cycle_count": cycle_n,
                "cycle_offenders": cycle_offenders(&cycles_json),
                "failed": failed,
            }
        }));
    } else if ok {
        println!("fence: ok (lint 0 issues, 0 cycles)");
    } else {
        println!(
            "fence: FAILED ({}); lint offenders: {:?}; cycle offenders: {:?}",
            failed.join("+"),
            lint_offenders(&lint_json),
            cycle_offenders(&cycles_json)
        );
    }
    if ok {
        Ok(())
    } else {
        bail!("plan fence failed: {}", failed.join("+"))
    }
}

/// `flywheel __plan-fence converged --state <path> [--json]`: digest the
/// open bead set, compare with the stored digest, write on change.
pub(crate) fn cmd_converged(state: &Path, json: bool) -> Result<()> {
    let list_raw = run_br(None, &["list", "--status", "open", "--json"])?;
    let mut list_json: Value = serde_json::from_str(list_raw.trim())
        .map_err(|e| anyhow::anyhow!("br list bad JSON: {e}"))?;
    if let Some(issues) = list_json.get_mut("issues").and_then(Value::as_array_mut) {
        for issue in issues.iter_mut() {
            if issue.get("dependencies").is_none()
                && let Some(id) = issue.get("id").and_then(Value::as_str).map(str::to_owned)
            {
                let deps = dependencies_for(&id);
                if !deps.is_empty() {
                    issue["dependencies"] =
                        Value::Array(deps.into_iter().map(Value::String).collect());
                }
            }
        }
    }
    let new_digest = digest(&list_json);
    let previous = std::fs::read_to_string(state)
        .ok()
        .map(|s| s.trim().to_owned());
    let is_converged = converged(&new_digest, previous.as_deref().filter(|s| !s.is_empty()));
    if json {
        print_json(&json!({
            "check": "converged",
            "ok": is_converged,
            "detail": {
                "digest": new_digest,
                "state": state.display().to_string(),
            }
        }));
    } else {
        println!("converged: {is_converged} {new_digest}");
    }
    if is_converged {
        Ok(())
    } else {
        if let Some(parent) = state.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(state, format!("{new_digest}\n"))?;
        bail!("bead set changed (digest {new_digest})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fence_passes_only_on_double_zero() {
        assert!(fence_verdict(0, 0));
        assert!(!fence_verdict(1, 0));
        assert!(!fence_verdict(0, 1));
        assert!(!fence_verdict(2, 3));
    }

    #[test]
    fn digest_stable_under_reordering_and_ignores_updated_at() {
        let a = json!({"issues": [
            {"id": "bd-2", "title": "B", "description": "d", "status": "open", "updated_at": "t1"},
            {"id": "bd-1", "title": "A", "description": "c", "status": "open", "updated_at": "t2"},
        ]});
        let b = json!({"issues": [
            {"id": "bd-1", "title": "A", "description": "c", "status": "open", "updated_at": "t3"},
            {"id": "bd-2", "title": "B", "description": "d", "status": "open", "updated_at": "t4"},
        ]});
        assert_eq!(digest(&a), digest(&b));
    }

    #[test]
    fn digest_sees_content_and_dependency_changes() {
        let base = json!({"issues": [
            {"id": "bd-1", "title": "A", "description": "c", "status": "open"},
        ]});
        let renamed = json!({"issues": [
            {"id": "bd-1", "title": "A2", "description": "c", "status": "open"},
        ]});
        let with_dep = json!({"issues": [
            {"id": "bd-1", "title": "A", "description": "c", "status": "open", "dependencies": ["bd-0"]},
        ]});
        assert_ne!(digest(&base), digest(&renamed));
        assert_ne!(digest(&base), digest(&with_dep));
    }

    #[test]
    fn converged_only_on_equal_previous() {
        assert!(converged("abc", Some("abc")));
        assert!(!converged("abc", Some("abd")));
        assert!(!converged("abc", None));
        assert!(!converged("abc", Some("")));
    }
}

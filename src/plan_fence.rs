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
    /// ADR-0021 D7 rule 3: a round may not drop a requirement from coverage
    /// without moving it to non-goals with a reason.
    Coverage {
        /// Path holding the requirement baseline (seeded by the first round).
        #[arg(long)]
        state: PathBuf,
        /// The round's reply JSON: `{"coverage": {req: [ids]}, "non_goals": [...]}`.
        #[arg(long)]
        map: PathBuf,
        /// Print one JSON object (`{"check","ok","detail"...}`).
        #[arg(long)]
        json: bool,
    },
}

/// Pure decision core: requirements that left BOTH the coverage map and the
/// non-goals list — the exact shrinkage D7 forbids. Sorted, deduped.
pub fn coverage_shrink(
    requirements: &[String],
    covered: &std::collections::BTreeSet<String>,
    non_goals: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let mut missing: Vec<String> = requirements
        .iter()
        .filter(|r| !covered.contains(*r) && !non_goals.contains(*r))
        .cloned()
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// The requirement set a round maps: every coverage key plus every non-goal.
fn mapped_requirements(map: &Value) -> std::collections::BTreeSet<String> {
    let mut out: std::collections::BTreeSet<String> = map
        .get("coverage")
        .and_then(Value::as_object)
        .map(|c| c.keys().cloned().collect())
        .unwrap_or_default();
    for n in map
        .get("non_goals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(s) = n.as_str() {
            out.insert(s.to_owned());
        } else {
            for key in ["requirement", "name", "id"] {
                if let Some(s) = n.get(key).and_then(Value::as_str) {
                    out.insert(s.to_owned());
                }
            }
        }
    }
    out
}

/// `flywheel __plan-fence coverage --state <path> --map <path> [--json]`.
///
/// The FIRST round seeds the baseline: its map names the requirements the grill
/// produced, so later rounds are measured against what the plan once covered.
/// A round passes when every baseline requirement is still covered or has a
/// non-goals entry; the baseline never shrinks.
pub(crate) fn cmd_coverage(state: &Path, map: &Path, json: bool) -> Result<()> {
    let raw = std::fs::read_to_string(map)
        .map_err(|e| anyhow::anyhow!("read covereage map {}: {e}", map.display()))?;
    let round: Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow::anyhow!("coverage map {} is not JSON: {e}", map.display()))?;
    let requirements: Vec<String> = match std::fs::read_to_string(state)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
        .and_then(|v| v.get("requirements").and_then(Value::as_array).cloned())
    {
        Some(rows) => rows
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        None => {
            // First round: seed the baseline from this map and pass it through.
            let seeded: Vec<String> = mapped_requirements(&round).into_iter().collect();
            std::fs::create_dir_all(state.parent().unwrap_or(Path::new(".")))
                .map_err(|e| anyhow::anyhow!("create {}: {e}", state.display()))?;
            std::fs::write(state, json!({ "requirements": seeded }).to_string())
                .map_err(|e| anyhow::anyhow!("seed {}: {e}", state.display()))?;
            seeded
        }
    };
    let covered: std::collections::BTreeSet<String> = round
        .get("coverage")
        .and_then(Value::as_object)
        .map(|c| c.keys().cloned().collect())
        .unwrap_or_default();
    let mut non_goals: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for n in round
        .get("non_goals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(s) = n.as_str() {
            non_goals.insert(s.to_owned());
        } else {
            for key in ["requirement", "name", "id"] {
                if let Some(s) = n.get(key).and_then(Value::as_str) {
                    non_goals.insert(s.to_owned());
                }
            }
        }
    }
    let missing = coverage_shrink(&requirements, &covered, &non_goals);
    let ok = missing.is_empty();
    if json {
        print_json(&json!({
            "check": "coverage",
            "ok": ok,
            "detail": {
                "requirements": requirements.len(),
                "covered": covered.len(),
                "non_goals": non_goals.len(),
                "dropped": missing,
            }
        }));
    } else if ok {
        println!(
            "coverage: ok ({} requirements, {} covered, {} non-goals)",
            requirements.len(),
            covered.len(),
            non_goals.len()
        );
    } else {
        println!(
            "coverage: FAILED (dropped without a non-goal: {})",
            missing.join(", ")
        );
    }
    if ok {
        Ok(())
    } else {
        bail!(
            "plan coverage shrank: {} dropped without a non-goals entry",
            missing.join(", ")
        )
    }
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
/// revision, priority, issue_type, dependencies (sorted, empty when absent).
/// `revision` bumps on ANY bead edit (including fields `br list` never
/// carries, like VERIFY or principles), so a content change can never look
/// unchanged. Accepts either the full `br list --status open --json`
/// envelope (`{"issues":[...]}`) or a bare array. Volatile fields
/// (`updated_at`, counts) never enter the digest, so a round that changes
/// nothing looks unchanged.
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
                "revision": issue.get("revision").and_then(Value::as_u64).unwrap_or(0),
                "priority": issue.get("priority").and_then(Value::as_u64).unwrap_or(0),
                "issue_type": issue.get("issue_type").and_then(Value::as_str).unwrap_or(""),
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

/// Pure decision core: the bead set is only digestible when fully listed.
/// `br list` paginates; a `has_more: true` envelope is a partial surface
/// and must never converge.
pub fn page_is_complete(list_json: &Value) -> bool {
    !list_json
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Pure parse of `br dep list <id> --json`: a bare array or an object
/// carrying `edges`. Malformed output is an error, never an empty list —
/// a failed read must never converge.
fn parse_dep_edges(raw: &str) -> Result<Vec<String>> {
    let v: Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow::anyhow!("br dep list bad JSON: {e}: {raw}"))?;
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
    Ok(deps)
}

fn dependencies_for(bead_id: &str) -> Result<Vec<String>> {
    let raw = run_br(None, &["dep", "list", bead_id, "--json"])?;
    parse_dep_edges(&raw).map_err(|e| anyhow::anyhow!("{bead_id}: {e:#}"))
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
    let list_raw = run_br(
        None,
        &["list", "--status", "open", "--json", "--limit", "0"],
    )?;
    let mut list_json: Value = serde_json::from_str(list_raw.trim())
        .map_err(|e| anyhow::anyhow!("br list bad JSON: {e}"))?;
    if !page_is_complete(&list_json) {
        if json {
            print_json(&json!({
                "check": "converged",
                "ok": false,
                "detail": {
                    "reason": "has_more",
                    "state": state.display().to_string(),
                }
            }));
        } else {
            println!("converged: false (br list has_more — partial surface)");
        }
        bail!("bead set incomplete (br list has_more)")
    };
    if let Some(issues) = list_json.get_mut("issues").and_then(Value::as_array_mut) {
        for issue in issues.iter_mut() {
            if issue.get("dependencies").is_none()
                && let Some(id) = issue.get("id").and_then(Value::as_str).map(str::to_owned)
            {
                let deps = dependencies_for(&id)?;
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

    /// ADR-0021 D7 rule 3 / acceptance 4: a round that drops a requirement
    /// from coverage without a non-goals entry is the failure the fence exists
    /// to catch; a non-goals entry is the sanctioned way out.
    #[test]
    fn coverage_shrink_names_every_dropped_requirement() {
        use std::collections::BTreeSet;
        let reqs: Vec<String> = ["r1", "r2", "r3"].iter().map(|s| s.to_string()).collect();
        let set =
            |items: &[&str]| -> BTreeSet<String> { items.iter().map(|s| s.to_string()).collect() };
        assert!(
            coverage_shrink(&reqs, &set(&["r1", "r2", "r3"]), &set(&[])).is_empty(),
            "full coverage is not a shrink"
        );
        assert_eq!(
            coverage_shrink(&reqs, &set(&["r1"]), &set(&["r2", "r3"])),
            Vec::<String>::new(),
            "a declared non-goal is a sanctioned exit"
        );
        assert_eq!(
            coverage_shrink(&reqs, &set(&["r1"]), &set(&["r3"])),
            vec!["r2".to_string()],
            "the dropped requirement is named"
        );
        assert_eq!(
            coverage_shrink(&reqs, &set(&[]), &set(&[])),
            vec!["r1".to_string(), "r2".to_string(), "r3".to_string()],
            "every dropped requirement is named, sorted"
        );
    }

    /// The CLI seeds the baseline from the FIRST round's map and never lets it
    /// shrink: a later round is measured against what the plan once covered.
    #[test]
    fn the_coverage_fence_seeds_once_and_never_shrinks() {
        let dir = std::env::temp_dir().join(format!("fw-coverage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let state = dir.join("coverage.json");
        let map = dir.join("round.json");

        std::fs::write(
            &map,
            json!({"coverage": {"r1": ["bd-1"], "r2": ["bd-2"]}, "non_goals": []}).to_string(),
        )
        .expect("round 1 map");
        cmd_coverage(&state, &map, true).expect("the first round seeds the baseline");

        // Round 2 keeps everything: passes.
        std::fs::write(
            &map,
            json!({"coverage": {"r1": ["bd-1"], "r2": ["bd-2", "bd-3"]}, "non_goals": []})
                .to_string(),
        )
        .expect("round 2 map");
        cmd_coverage(&state, &map, true).expect("a non-shrinking round passes");

        // Round 3 drops r2 with no non-goal: fails, naming it.
        std::fs::write(
            &map,
            json!({"coverage": {"r1": ["bd-1"]}, "non_goals": []}).to_string(),
        )
        .expect("round 3 map");
        let err = cmd_coverage(&state, &map, true).expect_err("a dropped requirement fails");
        assert!(err.to_string().contains("r2"), "{err}");

        // Round 4 drops r2 but declares it a non-goal: passes.
        std::fs::write(
            &map,
            json!({"coverage": {"r1": ["bd-1"]}, "non_goals": ["r2"]}).to_string(),
        )
        .expect("round 4 map");
        cmd_coverage(&state, &map, true).expect("a declared non-goal passes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_differs_on_revision_bump() {
        let rev1 = json!({"issues": [
            {"id": "bd-1", "title": "A", "description": "c", "status": "open",
             "revision": 1, "priority": 2, "issue_type": "feature"},
        ]});
        let rev2 = json!({"issues": [
            {"id": "bd-1", "title": "A", "description": "c", "status": "open",
             "revision": 2, "priority": 2, "issue_type": "feature"},
        ]});
        assert_ne!(digest(&rev1), digest(&rev2));
    }

    #[test]
    fn page_complete_only_without_has_more() {
        assert!(page_is_complete(&json!({"issues": [], "has_more": false})));
        assert!(page_is_complete(&json!({"issues": []})));
        assert!(!page_is_complete(&json!({"issues": [], "has_more": true})));
    }

    #[test]
    fn dep_edges_reject_malformed_output() {
        assert!(parse_dep_edges("not json").is_err());
        assert!(parse_dep_edges("{\"cycles\": []}").is_ok());
        let edges = parse_dep_edges(
            "[{\"issue_id\":\"bd-1\",\"depends_on_id\":\"bd-0\",\"type\":\"blocks\"}]",
        )
        .expect("array edge shape parses");
        assert_eq!(edges, vec!["bd-0".to_owned()]);
    }

    #[test]
    fn dep_lookup_failure_propagates() {
        let _env = crate::config::lock_env();
        let prior = std::env::var_os("FLYWHEEL_BR_BIN");
        unsafe { std::env::set_var("FLYWHEEL_BR_BIN", "false") };
        let result = dependencies_for("bd-1");
        match prior {
            Some(v) => unsafe { std::env::set_var("FLYWHEEL_BR_BIN", v) },
            None => unsafe { std::env::remove_var("FLYWHEEL_BR_BIN") },
        }
        assert!(result.is_err(), "a failed br read must never converge");
    }
}

//! Dispatch bindings (flywheel ADR-0022 D8, bd-6setu.2): the durable record of
//! WHICH pin ran WHICH step of WHICH run.
//!
//! The binder needs the authors of a step's inputs, and the only honest source
//! is what this host actually dispatched: a row is written BEFORE the pane
//! prompt, keyed `(run_id, node_path, step)`, so a replay writes the same key
//! instead of a second claim. Headless, the loop's `Send` and plane-S write no
//! row — nothing there knows the run, and a guessed author is worse than none.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One dispatch binding: the pin that took a step, in one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingRow {
    pub run_id: String,
    pub node_path: String,
    pub step: String,
    pub pin: String,
}

impl BindingRow {
    /// The replay key: one row per (run, node, step). A later append for the
    /// same key never adds a second claim.
    #[must_use]
    pub fn key(&self) -> (String, String, String) {
        (
            self.run_id.clone(),
            self.node_path.clone(),
            self.step.clone(),
        )
    }
}

/// `state_dir/dispatch_bindings/<project>.jsonl`.
#[must_use]
pub fn bindings_file(project: &str) -> PathBuf {
    crate::config::state_dir()
        .join("dispatch_bindings")
        .join(format!("{}.jsonl", crate::state::state_key(project)))
}

/// Every binding row this host wrote for `project`, oldest first.
///
/// FAIL CLOSED (bd-6setu.16 H2): a store that cannot be read, or a torn row,
/// is an error — never an empty list. An empty answer would let the binder fall
/// back to its planner heuristic and hand a review to its author, which is the
/// one thing this store exists to prevent. A missing file is the only empty
/// answer: nothing has been dispatched yet.
///
/// # Errors
///
/// Returns the read error, or one naming the torn line.
pub fn all(project: &str) -> Result<Vec<BindingRow>> {
    let body = match std::fs::read_to_string(bindings_file(project)) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!(
                "dispatch bindings unreadable at {}: refusing to guess the author",
                bindings_file(project).display()
            )));
        }
    };
    let mut rows = Vec::new();
    for (n, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row = serde_json::from_str::<BindingRow>(line).with_context(|| {
            format!(
                "dispatch bindings torn at {} line {}: refusing to guess the author",
                bindings_file(project).display(),
                n + 1
            )
        })?;
        rows.push(row);
    }
    Ok(rows)
}

/// Record that `pin` took `step` at `node_path` in `run_id`, before the prompt.
/// Idempotent on the key, so a replay adds nothing.
///
/// # Errors
///
/// Returns the write error when the store cannot be appended; a binding that
/// silently failed to persist would let a review bind its own author.
pub fn append(project: &str, run_id: &str, node_path: &str, step: &str, pin: &str) -> Result<()> {
    if run_id.trim().is_empty() || pin.trim().is_empty() {
        return Ok(());
    }
    let row = BindingRow {
        run_id: run_id.to_string(),
        node_path: node_path.to_string(),
        step: step.to_string(),
        pin: pin.to_string(),
    };
    if all(project)?.iter().any(|r| r.key() == row.key()) {
        return Ok(());
    }
    let path = bindings_file(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("dispatch bindings dir {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(&row).context("binding row serializes")?;
    line.push('\n');
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("dispatch bindings open {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("dispatch bindings append {}", path.display()))?;
    // bd-6setu.16 H2: the row must survive a crash before the prompt, or the
    // journal claims a binding that no reader will see.
    file.sync_all()
        .with_context(|| format!("dispatch bindings fsync {}", path.display()))?;
    Ok(())
}

/// The pins that took any of `steps` in `run_id`. A review step's authors are
/// the pins bound to the steps its stage READS, in the SAME run: nothing from
/// another run, and never a pin that never ran a step here.
///
/// # Errors
///
/// Propagates a failed read: a caller that cannot see the rows must refuse the
/// bind rather than bind blind.
pub fn authors_for(
    project: &str,
    run_id: &str,
    steps: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    Ok(all(project)?
        .into_iter()
        .filter(|r| r.run_id == run_id && steps.contains(&r.step))
        .map(|r| r.pin)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `state_dir` is process-global; `lock_env` gives a temp state home and
    /// serializes these tests, and the project key keeps them apart.
    fn temp_project(name: &str) -> String {
        format!("bind-{}-{name}", std::process::id())
    }

    /// bd-6setu.2: one row per (run, node, step) — a replay of the same step
    /// writes the same key and adds nothing.
    #[test]
    fn one_row_per_step_and_a_replay_keeps_the_key() {
        let _g = crate::config::lock_env();
        let project = temp_project("one-row");
        append(&project, "run-1", "plan", "review", "Ada").unwrap();
        append(&project, "run-1", "plan", "review", "Ada").unwrap();
        append(&project, "run-1", "plan", "review", "Grace").unwrap();
        let rows = all(&project).expect("readable store");
        assert_eq!(
            rows.len(),
            1,
            "the key, not the append count, decides: {rows:?}"
        );
        assert_eq!(rows[0].pin, "Ada");
    }

    /// Authors come from THIS run and only the steps asked for: a second run
    /// with the same step names never lends its pins to the first.
    #[test]
    fn two_runs_do_not_share_authors() {
        let _g = crate::config::lock_env();
        let project = temp_project("two-runs");
        append(&project, "run-1", "n1", "execute", "Ada").unwrap();
        append(&project, "run-2", "n1", "execute", "Grace").unwrap();
        let steps: BTreeSet<String> = ["execute".to_string()].into_iter().collect();
        assert_eq!(
            authors_for(&project, "run-1", &steps).expect("readable store"),
            ["Ada".to_string()].into_iter().collect::<BTreeSet<_>>()
        );
        let other: BTreeSet<String> = ["review".to_string()].into_iter().collect();
        assert!(
            authors_for(&project, "run-1", &other)
                .expect("readable store")
                .is_empty(),
            "a step nobody ran in this run has no author"
        );
    }

    /// A row with no run or no pin is not written: an empty scope cannot be a
    /// claim about who ran what.
    #[test]
    fn an_empty_scope_writes_nothing() {
        let _g = crate::config::lock_env();
        let project = temp_project("empty-scope");
        append(&project, "", "plan", "review", "Ada").unwrap();
        append(&project, "run-1", "plan", "review", "").unwrap();
        assert!(all(&project).expect("readable store").is_empty());
    }

    /// bd-6setu.16 H2: the store is EVIDENCE. A torn row or an unreadable file
    /// must refuse the bind, never answer "no rows" — an empty answer hands a
    /// review to its author through the planner fallback.
    #[test]
    fn a_torn_row_refuses_instead_of_guessing() {
        let _g = crate::config::lock_env();
        let project = temp_project("torn-row");
        append(&project, "run-1", "plan", "execute", "Ada").unwrap();
        let path = bindings_file(&project);
        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str("{\"run_id\":\"run-1\",\"node_path\":\"pl\n");
        std::fs::write(&path, body).unwrap();
        let err = all(&project).expect_err("a torn row is not an empty store");
        assert!(
            format!("{err:#}").contains("torn"),
            "the refusal names the tear: {err:#}"
        );
        let steps: BTreeSet<String> = ["execute".to_string()].into_iter().collect();
        assert!(
            authors_for(&project, "run-1", &steps).is_err(),
            "authors must refuse too: guessing here binds the author"
        );
    }

    /// A row is readable by a SECOND handle the moment append returns, which is
    /// what journal-before-prompt buys (the write is synced, not buffered).
    #[test]
    fn a_row_is_visible_to_a_second_reader_after_append() {
        let _g = crate::config::lock_env();
        let project = temp_project("durable");
        append(&project, "run-1", "plan", "execute", "Ada").unwrap();
        let rows = all(&project).expect("readable store");
        assert_eq!(rows.len(), 1, "append is durable before it returns");
    }
}

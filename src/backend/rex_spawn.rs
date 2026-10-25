//! Pure rex crew layout: a [`crate::crew::CrewPlan`] rendered as a rex
//! `session.new_split` / `session.create` layout value.
//!
//! governed-by: ADR-0002
//!
//! No I/O, no clock, no spawn. The caller runs the value once, at the edge
//! (a single `rex do` script or `session.create` with `initial_windows`).
//! A dropped value performs nothing. Labels are `role` for a single member
//! and `role-N` (1-based, plan order) above that; the spawn edge renames
//! blocks to the registry names afterwards (`agent_rename`).
//!
//! The live `session.new_split` schema has no top-level `env` key and the
//! `com.superlogical.terminal.shell` flavor's `options` has no `env` key
//! either (`rex api call block_creator.list`), so a model pin that travels
//! by env arrives as an `env K=V` prefix inside `command` — never as a
//! second quoting story. `command` reaches the program unchanged through
//! the login shell, so no shell-quoting is invented here.

// The layout is the landing surface for the rex spawn door; nothing wires
// it yet, and this is a bin crate where `pub` does not exempt dead code.
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::crew::CrewPlan;

/// The rex flavor for every crew block: a terminal running the member's
/// command (or an interactive shell when the command is empty).
pub const CREW_SHELL_FLAVOR: &str = "com.superlogical.terminal.shell";

/// Render a crew plan as a rex layout value.
///
/// `commands` maps a role to its program argv (already `env K=V`-prefixed
/// when a pin travels by env). A role with no entry — or an empty argv —
/// becomes an interactive shell block. `cwd` is the server-side directory
/// every block starts in (omitted when `None` or empty).
///
/// An empty plan renders as `Null`: the caller refuses that before spawn
/// (`pane::ensure_crew`), and a `Null` layout sent nowhere starts nothing.
pub fn crew_layout(
    plan: &CrewPlan,
    commands: &BTreeMap<String, Vec<String>>,
    cwd: Option<&str>,
) -> Value {
    let mut nodes: Vec<Value> = Vec::new();
    for (role, count) in &plan.members {
        let command = commands.get(role).cloned().unwrap_or_default();
        for ordinal in 1..=(*count).max(1) {
            let label = if *count <= 1 {
                role.clone()
            } else {
                format!("{role}-{ordinal}")
            };
            nodes.push(crew_block(&label, &command, cwd));
        }
    }
    // Right-leaning fold: split(a, split(b, c)) traverses before-first as
    // a, b, c, so layout order always matches plan order.
    while nodes.len() > 1 {
        let after = nodes.pop().expect("two nodes");
        let before = nodes.pop().expect("two nodes");
        nodes.push(split_node(before, after));
    }
    nodes.pop().unwrap_or(Value::Null)
}

/// One crew block: the shell flavor with its label and only the options
/// the schema accepts (`command`, `cwd`). No `env` key exists.
fn crew_block(label: &str, command: &[String], cwd: Option<&str>) -> Value {
    let mut options = Map::new();
    if !command.is_empty() {
        options.insert(
            "command".to_string(),
            Value::Array(command.iter().map(|a| Value::String(a.clone())).collect()),
        );
    }
    if let Some(dir) = cwd.filter(|d| !d.is_empty()) {
        options.insert("cwd".to_string(), Value::String(dir.to_string()));
    }
    let mut block = Map::new();
    block.insert(
        "flavor".to_string(),
        Value::String(CREW_SHELL_FLAVOR.to_string()),
    );
    block.insert("label".to_string(), Value::String(label.to_string()));
    if !options.is_empty() {
        block.insert("options".to_string(), Value::Object(options));
    }
    Value::Object(Map::from_iter([(
        "block".to_string(),
        Value::Object(block),
    )]))
}

fn split_node(before: Value, after: Value) -> Value {
    let mut split = Map::new();
    split.insert(
        "direction".to_string(),
        Value::String("horizontal".to_string()),
    );
    split.insert("before".to_string(), before);
    split.insert("after".to_string(), after);
    Value::Object(Map::from_iter([(
        "split".to_string(),
        Value::Object(split),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> CrewPlan {
        CrewPlan {
            members: BTreeMap::from([("coder".to_string(), 2), ("planner".to_string(), 1)]),
        }
    }

    /// One plan renders one layout: labeled shell blocks in plan order, the
    /// role command carried verbatim, no `env` key anywhere.
    #[test]
    fn one_plan_renders_labeled_shell_blocks_in_plan_order() {
        let commands = BTreeMap::from([(
            "coder".to_string(),
            vec!["opencode".to_string(), "run".to_string()],
        )]);
        let layout = crew_layout(&plan(), &commands, Some("/tmp/work"));
        let text = serde_json::to_string(&layout).expect("layout serializes");
        assert!(
            !text.contains("\"env\""),
            "the live schema has no env key: {text}"
        );
        assert_eq!(
            collect_blocks(&layout),
            vec![
                "coder-1".to_string(),
                "coder-2".to_string(),
                "planner".to_string()
            ],
            "plan order, 1-based ordinals: {text}"
        );
        assert_eq!(
            block_command(&layout, "coder-1"),
            Some(vec!["opencode".to_string(), "run".to_string()]),
            "argv rides unchanged: {text}"
        );
        assert_eq!(
            block_command(&layout, "planner"),
            None,
            "no entry means an interactive shell: {text}"
        );
        for label in collect_blocks(&layout) {
            assert_eq!(
                block_flavor(&layout, &label).as_deref(),
                Some(CREW_SHELL_FLAVOR)
            );
            assert_eq!(block_cwd(&layout, &label).as_deref(), Some("/tmp/work"));
        }
    }

    fn collect_blocks(layout: &Value) -> Vec<String> {
        let mut out = Vec::new();
        collect_into(layout, &mut out);
        out
    }

    fn collect_into(node: &Value, out: &mut Vec<String>) {
        if let Some(block) = node.get("block") {
            if let Some(label) = block.get("label").and_then(Value::as_str) {
                out.push(label.to_string());
            }
            return;
        }
        if let Some(split) = node.get("split") {
            for side in ["before", "after"] {
                if let Some(child) = split.get(side) {
                    collect_into(child, out);
                }
            }
        }
    }

    fn find_block<'a>(layout: &'a Value, label: &str) -> Option<&'a Value> {
        if let Some(block) = layout.get("block") {
            return (block.get("label").and_then(Value::as_str) == Some(label)).then_some(block);
        }
        layout.get("split").and_then(|split| {
            ["before", "after"]
                .iter()
                .filter_map(|side| split.get(*side))
                .find_map(|child| find_block(child, label))
        })
    }

    fn block_command(layout: &Value, label: &str) -> Option<Vec<String>> {
        let block = find_block(layout, label)?;
        block
            .get("options")?
            .get("command")?
            .as_array()
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
    }

    fn block_flavor(layout: &Value, label: &str) -> Option<String> {
        find_block(layout, label)?
            .get("flavor")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn block_cwd(layout: &Value, label: &str) -> Option<String> {
        find_block(layout, label)?
            .get("options")?
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

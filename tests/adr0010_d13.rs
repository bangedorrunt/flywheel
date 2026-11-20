// governed-by: ADR-0010 D13 (bd-eo39) — captain/steer subagent-guard.
//! Deny delegation-shaped harness tools on the dispatcher-owning pane:
//! stems agent|subagent|task|workflow|spawn|dispatch|worktree|delegate,
//! excluding mcp__* and the observe/stop + session-local todo allowlist.
//! Crew panes are exempt (FLYWHEEL_ROLE=crew, injected at pane.split — the
//! captain rides the root workspace env). The fence is a tool-surface gate
//! (`flywheel __subagent-guard`), NOT a prompt; template standing orders are
//! defense-in-depth only.
//! Structural assertions scan source (D0/D14 pattern); behavioral tests live
//! in src/main.rs (mod adr0010_d13_tests).
//! Test names: adr0010_d13_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d13`.

mod support;

static MAIN: std::sync::LazyLock<String> = std::sync::LazyLock::new(support::crate_source);
const SHIM: &str = include_str!("../src/hc.rs");
const SWARM: &str = include_str!("../integrations/prompts/swarm.md.tpl");
const LOOP: &str = include_str!("../integrations/prompts/loop.md.tpl");
const OPEN: &str = include_str!("../integrations/prompts/open.md.tpl");

#[test]
fn adr0010_d13_fence_command_present() {
    for needle in ["__subagent-guard", "fn delegation_deny", "DELEGATION_STEMS"] {
        assert!(MAIN.contains(needle), "main.rs missing {needle}");
    }
}

#[test]
fn adr0010_d13_all_eight_stems_and_mcp_exclusion() {
    for stem in [
        "agent", "subagent", "task", "workflow", "spawn", "dispatch", "worktree", "delegate",
    ] {
        assert!(
            MAIN.contains(&format!("\"{stem}\"")),
            "stem {stem} in DELEGATION_STEMS"
        );
    }
    assert!(
        MAIN.contains("mcp__"),
        "mcp__* tools must be excluded from the deny (toron MCP stays reachable)"
    );
}

#[test]
fn adr0010_d13_observe_stop_and_todo_allowlist() {
    for allow in [
        "taskoutput",
        "taskstop",
        "tasklist",
        "taskcreate",
        "taskupdate",
    ] {
        assert!(
            MAIN.contains(&format!("\"{allow}\"")),
            "allowlist entry {allow} present (observe/stop + session-local todo stay legal)"
        );
    }
}

#[test]
fn adr0010_d13_crew_exemption_env_marker_at_pane_split() {
    assert!(
        SHIM.contains("FLYWHEEL_ROLE"),
        "pane.split env must carry the crew role marker"
    );
    assert!(
        MAIN.contains("FLYWHEEL_ROLE"),
        "the guard must read the crew role marker for exemption"
    );
}

#[test]
fn adr0010_d13_template_standing_orders_are_defense_in_depth() {
    for (name, src) in [("swarm", SWARM), ("loop", LOOP), ("open", OPEN)] {
        assert!(
            src.contains("__subagent-guard") || src.contains("subagent-guard"),
            "{name}.md.tpl missing the subagent-guard standing order"
        );
    }
}

#[test]
fn adr0010_d13_no_tracked_claude_only_deny_list() {
    // bd-b2kaa legitimately ships Claude-compatible HOOK config at
    // ~/.claude/settings.json. D13 was written for a different thing: no
    // Claude *permission deny list* may ship from flywheel. Guard that SHAPE —
    // a `permissions` object carrying `deny`/`deny_list` — whatever path it is
    // written to, rather than banning a path.
    fn deny_shape(src: &str) -> bool {
        let mut rest = src;
        while let Some(i) = rest.find("\"permissions\"") {
            let window = &rest[i..(i + 240).min(rest.len())];
            if window.contains("\"deny\":") || window.contains("\"deny_list\":") {
                return true;
            }
            rest = &rest[i + 13..];
        }
        false
    }
    assert!(
        !deny_shape(&MAIN),
        "no Claude permissions deny list may ship from this bead"
    );
    // Seeded case: the same predicate must reject a real deny list, so the
    // narrowed guard is not vacuous.
    let seeded = r#"{"permissions":{"deny":["Bash(*)"]}}"#;
    assert!(
        deny_shape(seeded),
        "the guard must fail on a real deny list"
    );
}

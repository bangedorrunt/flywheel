// governed-by: ADR-0010 D0 (bd-aucr) — Wave A.0 gate.
//! One live WorkspaceModel; poll_mail_digests deleted as wake/Msg producer.
//! Digest *rendering* (MailDigest struct, `__mail-digest` leaf) may remain as
//! a Cmd consumer of already-classified rows — only the durable producer dies.
//!
//! Test names: adr0010_d0_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d0`.

const DURABLE: &str = include_str!("../src/durable.rs");
const MAIN: &str = include_str!("../src/main.rs");

/// D0 producer deletion: no durable `poll_mail_digests` definition and no
/// resonate registration. A registered leaf is an available Msg producer even
/// when the live loop stops calling it (GS-030 only removed the call site).
#[test]
fn adr0010_d0_digest_producer_deleted() {
    assert!(
        !DURABLE.contains("async fn poll_mail_digests"),
        "D0 (bd-aucr): poll_mail_digests still defined as wake producer in durable.rs — delete it"
    );
    assert!(
        !DURABLE.contains("register(poll_mail_digests)"),
        "D0 (bd-aucr): poll_mail_digests still registered in durable.rs — delete the registration"
    );
    assert!(
        !DURABLE.contains("struct MailDigestArgs"),
        "D0 (bd-aucr): MailDigestArgs survives its only consumer — delete it with the producer"
    );
}

/// D0 one Model: every `WorkspaceModel::default()` site in the live files is
/// the single doorbell owner, a replay fold, a throwaway fold, or a test.
/// Anything else is a second live owner and fails.
#[test]
fn adr0010_d0_one_live_workspace_model() {
    check_file("durable.rs", DURABLE);
    check_file("main.rs", MAIN);
}

fn check_file(name: &str, src: &str) {
    let needle = "WorkspaceModel::default()";
    let lines: Vec<&str> = src.lines().collect();
    // current fn name + whether it is a #[test] fn, tracked as we scan
    let mut cur_fn = "<top>".to_string();
    let mut cur_test = false;
    for (idx, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        if let Some(fn_name) = fn_def_name(t) {
            cur_fn = fn_name;
            let from = idx.saturating_sub(4);
            cur_test = lines[from..idx]
                .iter()
                .any(|a| a.trim() == "#[test]" || a.trim().ends_with("::test]"));
        }
        if !line.contains(needle) {
            continue;
        }
        // Replay folds (pure tick over journal + local render, no dispatcher,
        // no binds, no writes) are not live owners: cmd_doorbell_why,
        // cmd_doorbell_afk (bd-65he reverify: read-only digest render).
        // Throwaway folds (GS-048 reclassify, bd-dv22): one synthetic
        // MaterialContext tick over a Default-seeded model, rendered to
        // stderr, model dropped. No dispatcher, no binds, no sends, no
        // presence publish — the single scalar `save_session_topology`
        // write converges with the live path (same file, same value
        // domain) and is required for loop-less sticky-open; it produces
        // no wake and owns nothing. Proven structurally below.
        let allowed = cur_test
            || cur_fn == "tick_loop_owner"
            || (name == "main.rs" && cur_fn == "cmd_internal_doorbell")
            || (name == "main.rs" && cur_fn == "cmd_doorbell_why")
            || (name == "main.rs" && cur_fn == "cmd_doorbell_afk")
            || (name == "main.rs" && cur_fn == "cmd_reclassify");
        assert!(
            allowed,
            "D0 (bd-aucr): {name}:{} live WorkspaceModel::default() inside fn {cur_fn} — \
             second live owner (allowed: tick_loop_owner, cmd_internal_doorbell, \
             cmd_doorbell_why/cmd_doorbell_afk replay, cmd_reclassify throwaway, tests)",
            idx + 1
        );
    }
}

/// Name the function defined by a trimmed line, if it defines one.
fn fn_def_name(t: &str) -> Option<String> {
    let rest = t
        .strip_prefix("pub async fn ")
        .or_else(|| t.strip_prefix("async fn "))
        .or_else(|| t.strip_prefix("pub fn "))
        .or_else(|| t.strip_prefix("fn "))?;
    Some(rest.chars().take_while(|c| *c != '(').collect())
}

/// bd-dv22: the throwaway-fold property, proven structurally instead of by
/// fn-name alone. The non-owner allowed folds (why/afk/reclassify) must
/// never execute commands or inject prompts: no `handle_workspace_cmds`
/// (the dispatch sink), no `agent.prompt(` (the prompt hop), no presence
/// publish. If a future fold gains any of these, it is a live owner by
/// construction and this test fails naming it — the allowlist above cannot
/// silently cover it.
#[test]
fn adr0010_d0_throwaway_folds_never_dispatch() {
    for name in ["cmd_doorbell_why", "cmd_doorbell_afk", "cmd_reclassify"] {
        let body = fn_body(MAIN, name);
        for forbidden in [
            "handle_workspace_cmds",
            "agent.prompt(",
            "presence.publish",
            "presence_publish",
        ] {
            assert!(
                !body.contains(forbidden),
                "D0 (bd-dv22): throwaway fold {name} contains {forbidden:?} — \
                 it dispatches, so it is a live owner, not a throwaway"
            );
        }
    }
}

/// Slice one top-level fn body (def line through its balanced close).
fn fn_body(src: &str, name: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let start = lines
        .iter()
        .position(|l| {
            let t = l.trim_start();
            fn_def_name(t).as_deref() == Some(name)
        })
        .unwrap_or_else(|| panic!("D0 (bd-dv22): fn {name} not found"));
    let mut depth = 0;
    let mut started = false;
    let mut end = start;
    for (j, line) in lines.iter().enumerate().skip(start) {
        depth += line.chars().filter(|c| *c == '{').count() as i32
            - line.chars().filter(|c| *c == '}').count() as i32;
        if depth > 0 {
            started = true;
        }
        if started && depth == 0 {
            end = j;
            break;
        }
    }
    lines[start..=end].join("\n")
}

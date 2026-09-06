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
        let allowed = cur_test
            || cur_fn == "tick_loop_owner"
            || (name == "main.rs" && cur_fn == "cmd_internal_doorbell")
            || (name == "main.rs" && cur_fn == "cmd_doorbell_why");
        assert!(
            allowed,
            "D0 (bd-aucr): {name}:{} live WorkspaceModel::default() inside fn {cur_fn} — \
             second live owner (allowed: tick_loop_owner, cmd_internal_doorbell, \
             cmd_doorbell_why replay, tests)",
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

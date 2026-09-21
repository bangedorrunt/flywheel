//! ADR-0005 live contract: captain with/without loop, doorbell handler,
//! ring/queue core, and the live kind-9 channel-post → watch → doorbell path.
//!
//! governed-by: ADR-0005

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// Column-0 `fn name(` — skips tripwire string literals that quote the needle.
/// Handles `pub(crate) fn` too (bd-mngn: handlers migrate into cmd/ modules).
fn find_toplevel_fn(src: &str, name: &str) -> usize {
    let needle = format!("\nfn {name}(");
    if let Some(i) = src.find(&needle) {
        return i + 1;
    }
    let needle = format!("\npub(crate) fn {name}(");
    src.find(&needle)
        .map(|i| i + 1)
        .unwrap_or_else(|| panic!("expected toplevel `fn {name}(`"))
}

/// SC-D9: `/swarm-mode` must not require loop supervision for peer wake.
#[test]
fn sc_d9_swarm_playbook_loop_optional_for_peer_wake() {
    let tpl = read("integrations/prompts/swarm.md.tpl");
    assert!(
        tpl.contains("Keep wake\n   coordination separate")
            || tpl.contains("peer-aware wake")
            || tpl.contains("Keep wake separate from supervision"),
        "swarm playbook must separate wake from supervision, got a snippet: {}",
        tpl.lines()
            .find(|l| l.contains("SPAWN") || l.contains("loop run"))
            .unwrap_or("")
    );
    assert!(
        tpl.contains("doorbell") || tpl.contains("Layer 0"),
        "swarm spawn step must name the doorbell / Layer 0"
    );
}

/// SC-D10: `/loop-mode` must not teach a 5-min inbox timer as the driver.
#[test]
fn sc_d10_loop_playbook_is_not_a_five_min_poll() {
    let tpl = read("integrations/prompts/loop.md.tpl");
    assert!(
        !tpl.contains("inbox every 5 min"),
        "loop playbook still teaches a 5-min inbox poll"
    );
    assert!(
        tpl.contains("[doorbell]") || tpl.contains("doorbell"),
        "loop playbook must name the doorbell as wake"
    );
}

/// SC-D1 herdr-mock: kind-9 `@pin` routes at the edge but triages to
/// routine (bd-l72k) — no `[doorbell]` inject, no `loop run`. Two pins
/// registered; the mention is counted, never prompted.
/// ADR-0010 D3: the mock `--status` is evidence only; trusted presence
/// arrives via a 20001 self-attest line (first stdin line).
#[test]
fn sc_d1_kind9_at_pin_is_routine_without_prompt() {
    let presence = r#"{"kind":20001,"tags":[["p","BlueRidge"]],"content":"idle"}"#;
    let kind9 = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","steer-x"]],"content":"@BlueRidge hi"}"#;
    let stdin = format!("{presence}\n{kind9}");
    let stdout = run_doorbell_once(&stdin, &["BlueRidge=idle", "LilacChapel=idle"]);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or(serde_json::json!(null));
    assert_eq!(
        v,
        serde_json::json!([]),
        "bd-l72k: routine kind-9 spends zero prompts, got {stdout}"
    );
    assert!(
        !stdout.contains("loop run"),
        "doorbell path must not invoke loop, got {stdout}"
    );
}

/// Live wire: kind-9 `p` is npub hex. Plain kind-9 triages to routine
/// (bd-l72k), so npub resolution is proven through an urgent-tagged line —
/// the SelfIndex npub lookup is identical either way.
/// ADR-0010 D3: trusted presence via 20001 self-attest first.
#[test]
fn sc_d1_kind9_live_npub_p_tag_urgent_rings() {
    let npub = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    let presence = r#"{"kind":20001,"tags":[["p","BlueRidge"]],"content":"idle"}"#;
    let kind9 = format!(
        r#"{{"kind":9,"id":"ab","tags":[["p","{npub}"],["toron-kind","ack-sla-expired"],["h","steer-x"]],"content":"@BlueRidge hi"}}"#
    );
    let stdin = format!("{presence}\n{kind9}");
    let stdout = run_doorbell_once(&stdin, &["BlueRidge=idle", "LilacChapel=idle"]);
    assert!(
        stdout.contains("[doorbell]"),
        "urgent npub line must become a doorbell, got {stdout}"
    );
    assert!(stdout.contains("BlueRidge"), "got {stdout}");
    assert!(!stdout.contains("LilacChapel"), "got {stdout}");
}

/// SC-D1 / SC-D3: working pin is queued, not injected (no interrupt).
#[test]
fn sc_d1_working_pin_is_not_injected() {
    let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"]],"content":"@BlueRidge hi"}"#;
    let stdout = run_doorbell_once(line, &["BlueRidge=working"]);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or(serde_json::json!(null));
    assert_eq!(
        v,
        serde_json::json!([]),
        "working pin must not inject, got {stdout}"
    );
}

fn run_doorbell_once(stdin_line: &str, status: &[&str]) -> String {
    let mut cmd = Command::new(flywheel());
    cmd.args([
        "__doorbell",
        "--project",
        "toron",
        "--workspace",
        "sc-d1-mock",
        "--once",
    ]);
    for s in status {
        cmd.args(["--status", s]);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn __doorbell --once");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin.write_all(stdin_line.as_bytes()).expect("write line");
        stdin.write_all(b"\n").expect("write nl");
    }
    let out = child.wait_with_output().expect("wait --once");
    assert!(
        out.status.success(),
        "__doorbell --once failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// SC-D1 wiring (bd-asr2.5 topology): `cmd_internal_doorbell` selects the
/// host; the embedded live path is `doorbell_host::run_embedded` driving
/// doorbell-rt's Brain around `RtHost` (tick decides, journal first,
/// herdr prompt delivers). Same contracts, new addresses.
#[test]
fn sc_d1_live_dispatcher_uses_apply_watch_line() {
    // bd-mngn: cmd_internal_doorbell moved to src/cmd/doorbell.rs, where it is
    // the last fn in the file — so the dispatch slice runs to EOF.
    let cmd = read("src/cmd/doorbell.rs");
    let start = find_toplevel_fn(&cmd, "cmd_internal_doorbell");
    let dispatch = &cmd[start..];
    // Host selection lives in the command: daemon mode never runs a loop.
    assert!(
        dispatch.contains("resolve_host") && dispatch.contains("run_embedded"),
        "cmd must select the host and run the embedded binding"
    );
    assert!(
        !dispatch.contains("loop run"),
        "doorbell must not start the loop"
    );
    let host = read("src/doorbell_host.rs");
    // bd-yw4s: the reducer is the decider on the live path — tick owns
    // Watch/Status/Died/Sweep/AckSla arms via the Host binding.
    assert!(
        host.contains("workspace::tick"),
        "host binding must decide via workspace::tick"
    );
    assert!(
        host.matches("workspace::Msg::").count() >= 3,
        "host binding must construct Msgs (watch/status/ack-sla)"
    );
    let brain_journal = read("crates/doorbell-rt/src/brain.rs");
    assert!(
        brain_journal.contains("append_journal"),
        "live path must journal ticks for `doorbell why`"
    );
    assert!(
        host.contains("doorbell_prompt"),
        "live inject must go through the herdr prompt delivery path"
    );
    // One watch source: in-process bus watch with the CLI child fallback.
    assert!(
        host.contains("subscribe_trusted") && host.contains("spawn_watch_child"),
        "in-process watch first, CLI child fallback"
    );
    assert!(
        !host.contains("9,7,43001"),
        "watch filter stays tight: reactions (kind 7) are not wake-worthy (bd-qsed.4)"
    );
    assert!(
        host.contains("20001") && host.contains("30013") && host.contains("toron_kind(14)"),
        "watch must include presence 20001, profile 30013, and rumor 14"
    );
    assert!(
        host.contains("inbox_filtered") && host.contains("mail_inbox_fallback_argv"),
        "plane P sweep is fetch_inbox / toron mail inbox --since"
    );
    let brain = read("crates/doorbell-rt/src/brain.rs");
    assert!(
        brain.contains("take_allowed") && brain.contains("sync_journal"),
        "brain must enforce 5/30/3 budget behind the write-ahead journal"
    );
    // Push-only status spine: no snapshot poll fallback (sweep retries
    // coverage instead — DELTA vs the old refresh_status, same guarantee).
    assert!(
        !host.contains("agent_list"),
        "no snapshot poll; forwarder push + per-sweep respawn cover status"
    );
}

/// SC-D1 handler: identity + doorbell + since_ts; poll is safety-net only.
#[test]
fn sc_d1_handler_requires_heard_signal() {
    let tpl = read("integrations/peer-aware.md.tpl");
    assert!(
        tpl.contains("120s") && (tpl.contains("unanswered") || tpl.contains("presence")),
        "handler must teach 120s heard-or-deaf"
    );
    assert!(
        tpl.contains("captain's current dispatch is authoritative"),
        "peer-aware handler must not outrank captain dispatch"
    );
    assert!(
        tpl.contains("do not\ninvent implementation work")
            || tpl.contains("do not invent implementation work"),
        "peer-aware handler must not invent a competing task"
    );
}

#[test]
fn sc_d1_peer_template_is_handler_not_timer() {
    let tpl = read("integrations/peer-aware.md.tpl");
    assert!(tpl.contains("{mail_identity}"), "identity prime must stay");
    assert!(tpl.contains("[doorbell]"), "handler must teach [doorbell]");
    assert!(
        tpl.contains("--since"),
        "pull uses incremental since cursor"
    );
    assert!(
        tpl.contains("Safety-net"),
        "crash-to-next-spawn safety-net must be named, not the driver"
    );
    assert!(
        !tpl.contains("every {poll_interval_minutes} minutes\n  of idle"),
        "5-min idle watch must not be the driver"
    );
}

/// SC-D7: `--no-peer-aware` still documented as identity-only opt-out.
#[test]
fn sc_d7_no_peer_aware_keeps_identity_opts_out_of_handler() {
    let tpl = read("integrations/peer-aware.md.tpl");
    assert!(tpl.contains("--no-peer-aware"), "opt-out must be named");
    assert!(
        tpl.contains("identity line still injects") || tpl.contains("never of the identity line"),
        "opt-out must keep the identity line"
    );
}

/// SC-D4: /peer-aware-mode attaches (same lock), never forks.
#[test]
fn sc_d4_peer_aware_command_attaches() {
    let tpl = read("integrations/prompts/peer-aware.md.tpl");
    assert!(tpl.contains("ATTACH") || tpl.contains("attach"));
    assert!(tpl.contains("never forks") || tpl.contains("never fork"));
    assert!(tpl.contains("--budget") || tpl.contains("5/30/3"));
}

/// SC-D5: digest first line is dispatcher liveness; stale lock reloads.
#[test]
fn sc_d5_digest_and_stale_lock_named() {
    let yaml = read("src/builtin_workflows.rs");
    assert!(
        yaml.contains("dispatcher: up|down") || yaml.contains("`dispatcher` field"),
        "awareness-digest names dispatcher liveness"
    );
    let src = read("src/doorbell.rs");
    assert!(src.contains("dispatcher_status_line"));
    assert!(src.contains("LockAction::Reloaded"));
}

/// SC-D6: captain without loop — digest is project cron, no loop run.
#[test]
fn sc_d6_digest_needs_no_loop() {
    let src = read("src/builtin_workflows.rs");
    assert!(src.contains("awareness-digest"));
    assert!(src.contains("no loop run"));
    assert!(src.contains("scope: project") || src.contains("CronScope::Project"));
}

/// SC-D7: spawn source injects identity_prime and skips dispatcher.
#[test]
fn sc_d7_spawn_identity_without_dispatcher() {
    let src = read("src/main.rs");
    let start = find_toplevel_fn(&src, "cmd_spawn");
    // bd-mngn: cmd_internal_doorbell moved to cmd/doorbell.rs —
    // doorbell_project_workspaces is the next surviving fn marker.
    let end = find_toplevel_fn(&src, "doorbell_project_workspaces");
    let body = &src[start..end];
    assert!(body.contains("doorbell::identity_prime"));
    assert!(body.contains("if peer_aware"));
    assert!(body.contains("doorbell::ensure_running"));
}

/// `/swarm-mode` pins wave-swarm; it must not re-run `decide` (that diverts).
#[test]
fn swarm_mode_pins_wave_swarm_without_reclassify() {
    let tpl = read("integrations/prompts/swarm.md.tpl");
    assert!(
        tpl.contains("--parts wave-swarm"),
        "swarm-mode must pin compose --parts wave-swarm"
    );
    assert!(
        !tpl.contains("1. CLASSIFY:"),
        "slash /swarm-mode must not re-run decide as step 1"
    );
}

/// `/loop-mode` pins `loop-mode run --work`; it must not re-run `decide`.
#[test]
fn loop_mode_pins_loop_run_without_reclassify() {
    let tpl = read("integrations/prompts/loop.md.tpl");
    assert!(
        tpl.contains("loop-mode run") && tpl.contains("--work"),
        "loop-mode must pin flywheel loop-mode run <project> --work"
    );
    assert!(
        !tpl.contains("1. CLASSIFY:"),
        "slash /loop-mode must not re-run decide as step 1"
    );
}

/// `/swarm-mode peer-aware=off` maps to `--no-peer-aware`.
#[test]
fn swarm_and_loop_modes_are_distinct() {
    let swarm = read("integrations/prompts/swarm.md.tpl");
    let loop_tpl = read("integrations/prompts/loop.md.tpl");
    assert!(swarm.contains("multi-crew") && swarm.contains("captain-led"));
    assert!(loop_tpl.contains("solo mode") && loop_tpl.contains("current harness"));
    assert!(loop_tpl.contains("opposite of /swarm-mode"));
}

/// `/swarm-mode peer-aware=off` maps to `--no-peer-aware`.
#[test]
fn swarm_peer_aware_off_maps_to_flag() {
    let tpl = read("integrations/prompts/swarm.md.tpl");
    assert!(
        tpl.contains("peer-aware=off") && tpl.contains("--no-peer-aware"),
        "swarm must map peer-aware=off to --no-peer-aware"
    );
    assert!(
        !tpl.contains("poll inbox 5min"),
        "swarm brief must not teach a 5-min inbox poll"
    );
}

/// grok adapter must not fake $ARGUMENTS (D2 / §8).
#[test]
fn grok_frontmatter_does_not_claim_dollar_arguments() {
    let tpl = read("integrations/prompts/frontmatter-grok.md.tpl");
    assert!(
        !tpl.contains("$ARGUMENTS"),
        "grok frontmatter must not claim $ARGUMENTS interpolation"
    );
}

/// cmd_integration writes peer-aware.md + swarm.md + loop.md.
#[test]
fn integration_writes_orchestration_commands() {
    let src = read("src/main.rs");
    let start = src.find("fn cmd_integration(").expect("cmd_integration");
    let body = &src[start..start + 4000];
    assert!(body.contains("ORCHESTRATION_COMMANDS"));
    assert!(body.contains("format!(\"{name}.md\")") || body.contains("{name}.md"));
}

/// Kill tears down the pid file.
#[test]
fn sc_kill_stops_doorbell() {
    let src = read("src/main.rs");
    let start = find_toplevel_fn(&src, "cmd_kill");
    // cmd_kill now carries the ADR-0025 parent-death policy (stop/continue/
    // adopt + --reap-crews) before the doorbell teardown — window wide
    // enough for the policy block plus the stop call.
    let body = &src[start..start + 12000];
    assert!(body.contains("doorbell::stop"));
}

/// Captain CLI still offers spawn, loop, and the integration commands.
#[test]
fn captain_triggers_spawn_loop_and_integration_exist() {
    let exe = flywheel();
    for (args, needle) in [
        (["spawn", "--help"].as_slice(), "profile"),
        (["loop", "--help"].as_slice(), "work"),
        (["integration", "--help"].as_slice(), "kind"),
    ] {
        let out = Command::new(exe).args(args).output().expect("help");
        assert!(
            out.status.success(),
            "{} --help failed: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.to_lowercase().contains(needle) || text.contains(needle),
            "{} --help missing {needle:?}: {text}",
            args[0]
        );
    }
}

/// ADR D5 table is the captain-mode contract.
#[test]
fn adr_d5_names_collab_without_loop_and_bead_dispatch() {
    let adr = read("docs/decisions/0005-always-on-collab-baseline-and-command-surface.md");
    assert!(
        adr.contains("### D5 — Captain with and without a loop"),
        "D5 heading missing"
    );
    assert!(adr.contains("**Open**"), "no-loop captain mode missing");
    assert!(
        adr.contains("**Solo loop + beads**"),
        "loop captain mode missing"
    );
    assert!(adr.contains("SC-D9"), "scenario SC-D9 missing from §9");
}

fn toron_bin() -> PathBuf {
    if let Ok(p) = std::env::var("TORON_BIN") {
        return PathBuf::from(p);
    }
    let sibling = repo_root()
        .parent()
        .unwrap_or(Path::new("/Users/bangedorrunt/workspace"))
        .join("toron/target/debug/toron");
    if sibling.exists() {
        return sibling;
    }
    PathBuf::from("toron")
}

fn toron_run(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(toron_bin())
        .args(args)
        .arg("--json")
        .arg("--no-input")
        .env("TORON_MCP_DATA_DIR", data_dir)
        .output()
        .unwrap_or_else(|e| panic!("spawn toron {}: {e}", toron_bin().display()))
}

async fn live_mailbox(data_dir: &Path, project: &str, names: &[&str]) {
    use toron_core::bus::in_process::{InProcessBus, InProcessBusConfig};
    use toron_core::index::schema;
    use toron_core::mailbox::Mailbox;
    use toron_core::nostr::identity::AgentProfile;
    fs::create_dir_all(data_dir).expect("mkdir");
    let db = data_dir.join("index.sqlite");
    let pool = schema::connect(db.to_str().unwrap())
        .await
        .expect("connect");
    schema::migrate(&pool).await.expect("migrate");
    let bus = InProcessBus::open(pool.clone(), InProcessBusConfig::default())
        .await
        .expect("bus");
    let mailbox = Mailbox::new(Arc::new(bus), data_dir.to_path_buf(), None).with_index_pool(pool);
    for name in names {
        mailbox
            .register_agent(project, &AgentProfile::new(*name))
            .await
            .expect("register");
    }
}

/// SC-D1 live: real kind-9 `--mention` post, real `toron watch` NDJSON
/// (npub `p` tag), then `__doorbell --once` injects `[doorbell]` for the pin.
#[tokio::test]
async fn sc_d1_live_kind9_channel_post_rings() {
    let poster = "LiveWillow";
    let target = "PaleHarbor";
    let project = "doorbell-live";
    let channel = format!("steer-d1-{}", std::process::id());
    let dir = std::env::temp_dir().join(format!("fw-sc-d1-live-{}", std::process::id()));
    live_mailbox(&dir, project, &[poster, target]).await;

    let created = toron_run(
        &dir,
        &[
            "channel",
            "create",
            &channel,
            "--name",
            "sc-d1 live",
            "--project",
            project,
            "--as",
            poster,
        ],
    );
    assert!(
        created.status.success(),
        "channel create: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let joined = toron_run(
        &dir,
        &[
            "channel",
            "join",
            &channel,
            "--project",
            project,
            "--as",
            target,
        ],
    );
    assert!(
        joined.status.success(),
        "channel join: {}",
        String::from_utf8_lossy(&joined.stderr)
    );

    let body = format!("@{target} take the repro");
    let posted = toron_run(
        &dir,
        &[
            "channel",
            "post",
            &channel,
            "--body",
            &body,
            "--mention",
            target,
            "--project",
            project,
            "--as",
            poster,
        ],
    );
    assert!(
        posted.status.success(),
        "channel post: {}",
        String::from_utf8_lossy(&posted.stderr)
    );
    let post: serde_json::Value = serde_json::from_slice(&posted.stdout).expect("post JSON");
    let mentions = post["mentions"].as_array().expect("mentions");
    assert_eq!(mentions.len(), 1, "one mention npub");
    let npub = mentions[0].as_str().expect("npub str");
    assert_eq!(npub.len(), 64, "live p-tag is npub hex, got {npub}");
    assert_ne!(npub, target, "p-tag must not be the pin name");

    let watched = toron_run(
        &dir,
        &[
            "watch",
            "--project",
            project,
            "--channel",
            &channel,
            "--kinds",
            "9",
            "--once",
            "--timeout-ms",
            "15000",
        ],
    );
    assert!(
        watched.status.success(),
        "watch --once: {}",
        String::from_utf8_lossy(&watched.stderr)
    );
    let watch_txt = String::from_utf8_lossy(&watched.stdout);
    let line = watch_txt
        .lines()
        .find(|l| l.trim().starts_with('{'))
        .unwrap_or_else(|| panic!("watch stdout has no JSON line: {watch_txt}"));
    let ev: serde_json::Value = serde_json::from_str(line).expect("watch JSON");
    assert_eq!(ev["kind"], 9, "watch event is kind 9: {line}");
    let tags = ev["tags"].as_array().expect("tags");
    let p_vals: Vec<&str> = tags
        .iter()
        .filter_map(|t| {
            let a = t.as_array()?;
            (a.first()?.as_str()? == "p").then(|| a.get(1)?.as_str())?
        })
        .collect();
    assert!(
        p_vals.iter().any(|v| v.len() == 64 && *v != target),
        "live watch p-tag is npub, got {p_vals:?}"
    );
    let content = ev["content"].as_str().unwrap_or("");
    assert!(
        content.contains(&format!("@{target}")),
        "content has @pin, got {content}"
    );

    let stdout = {
        // ADR-0010 D3: trusted presence (20001 self-attest) before the kind-9
        // line; `--status` alone is evidence-only and never prompts.
        // bd-l72k: the live routine line triages silent — the assertions
        // above already proved the live wire shape (npub p-tag, @content).
        let presence = format!(r#"{{"kind":20001,"tags":[["p","{target}"]],"content":"idle"}}"#);
        let stdin = format!("{presence}\n{line}");
        run_doorbell_once(
            &stdin,
            &[&format!("{target}=idle"), &format!("{poster}=idle")],
        )
    };
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or(serde_json::json!(null));
    assert_eq!(
        v,
        serde_json::json!([]),
        "bd-l72k: live routine line spends zero prompts, got {stdout}"
    );
    let _ = fs::remove_dir_all(&dir);
}

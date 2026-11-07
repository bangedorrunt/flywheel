//! `flywheel loop` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[cfg(test)]
use crate::config::Config;
#[cfg(test)]
use crate::hc::AgentInfo;
#[cfg(test)]
use crate::hc::AgentStatus;
#[cfg(test)]
use crate::{backend, durable, state, verdict};
#[cfg(test)]
use serde_json::{Value, json};
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;

/// Subcommands of `flywheel loop` (bd-fw-stale-loop-processes-gac
/// restructure): `run` (default), `status`, `list`, `prune`.
#[derive(Subcommand)]
pub(crate) enum LoopCmd {
    /// Run the monitoring loop for a project.
    Run {
        project: String,
        /// poll interval in seconds (default 300 = 5 min); the SETTLED/base
        /// cadence — used when nobody is working
        #[arg(long, default_value = "300")]
        interval: u64,
        /// fast cadence while the swarm is working/blocked/unknown or right
        /// after a dispatch (default 30s; floor 5s — values below are clamped
        /// with a warning). Only affects the durable workflow; the sync loop
        /// (--once/--local) polls at --interval.
        #[arg(long, default_value = "30")]
        interval_active: u64,
        /// comma-separated instruction queue (stage names p1..p8 or raw prompts)
        #[arg(long)]
        queue: Option<String>,
        /// use a workflow from workflows.toml as the instruction queue
        #[arg(long)]
        workflow: Option<String>,
        /// text sent to idle agents when the queue is empty (default: none)
        #[arg(long)]
        nudge: Option<String>,
        /// overall max runtime in ms before failing (default: none)
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// single poll pass then exit (cron-friendly)
        #[arg(long)]
        once: bool,
        /// blocked-agent policy: continue|stop|mail (default continue)
        #[arg(long, default_value = "continue")]
        on_blocked: String,
        /// agents to manage (default all)
        #[arg(long, default_value = "all")]
        to: String,
        /// run the sync polling loop instead of the durable engine-backed workflow
        #[arg(long)]
        local: bool,
        /// bead-driven mode: source work from `br ready` each poll instead of
        /// a static queue — feeds the swarm until the plan/beads are done
        #[arg(long)]
        work: bool,
        /// bd-gnej: disable the wave-gate (wave-N label ordering) — for
        /// graphs that do not use wave labels. Default: gating ON.
        #[arg(long)]
        no_wave_gates: bool,
        /// ADR-0006 D2 (requires --work): wave-scoped dispatch — the ready
        /// set is `br ready` ∩ beads labeled `wave-N`, held (error naming
        /// the blockers) while any lower wave-M (M<N) bead is open or
        /// in_progress. Label-based, transitional until beads ADR-0001
        /// schema-18 ships the typed `wave` field.
        #[arg(long)]
        wave_label: Option<String>,
        /// send prompts without landing confirmation (escape hatch; confirm
        /// is the default). Only affects the durable workflow.
        #[arg(long)]
        no_confirm: bool,
        /// confirm timeout in ms for `__send --confirm` (default: leaf default 45s)
        #[arg(long)]
        confirm_timeout_ms: Option<u64>,
        /// run the loop as a WorkflowEngine durable function (bd-e0xk
        /// coexistence path): dispatches run as `loop-dispatch` engine
        /// workflows instead of the imperative body's send leaf
        #[arg(long)]
        engine: bool,
        /// take over the project from a live loop process instead of refusing
        #[arg(long)]
        force: bool,
        /// bd-swarm-loop-durable-panic-atlo: replay a killed run's pending
        /// invocation checkpoints. Default is FRESH — a pending invocation
        /// belongs to a dead owner, and replaying its checkpoints against a
        /// newer binary's leaf shapes storms serialization errors. Conflicts
        /// with --force (fresh takeover).
        #[arg(long, conflicts_with = "force")]
        resume: bool,
        /// ADR-0019 Spec 3.3 (operator-only, logged): allow `loop run` in a
        /// workspace that carries an extra linked worktree (deliberate human
        /// experiment). Requires the operator key; the loop still never
        /// dispatches `--work` INTO the linked tree.
        #[arg(long)]
        allow_worktree: bool,
        /// bd-p4oc strict vacuous gate: refuse 0-test VERIFY (also FLYWHEEL_VERIFY_STRICT=1)
        #[arg(long)]
        strict: bool,
    },
    /// bd-swarm-loop-durable-panic-atlo: own the once-pass cadence the
    /// operator would otherwise hand-roll (`while true; do loop-mode run
    /// --once; sleep N; done`). Each pass is a CHILD process, so a crash
    /// cannot kill the supervisor; stops after 3 consecutive failed passes
    /// with one actionable line.
    Supervise {
        project: String,
        /// bead-driven passes: forward --work to `loop-mode run`
        #[arg(long)]
        work: bool,
        /// seconds between passes (floor 5)
        #[arg(long, default_value = "60")]
        interval: u64,
        /// agents to manage (forwarded to `loop-mode run`)
        #[arg(long, default_value = "all")]
        to: String,
        /// bd-supervise-dead-stop-escalation-rweq: dead-stop window in minutes.
        /// No pane working AND no commit past this window with work remaining
        /// emits ONE loud escalation line and pages the captain once, not per
        /// pass; a recovering fleet clears it without a second.
        /// bd-pane-wedge-detect-respawn-pnyi: the same window gates the
        /// per-pane wedge probe (process alive but parked on a fatal error
        /// line) — one page per wedge, cleared when a commit lands.
        #[arg(long, default_value_t = crate::durable::DEAD_STOP_DEFAULT_MINUTES)]
        stall_minutes: u64,
        /// Nested lead. Its own log and lease, beside the root host.
        /// A lead relaunch passes the child id here.
        #[arg(long)]
        context: Option<String>,
    },
    /// Query the state of a durable loop invocation from local state
    Status { project: String },
    /// List registered loop processes (pid registry) with live/dead status
    List,
    /// Remove pid-registry entries whose process is dead (stale cleanup)
    Prune,
    /// Infrastructure diagnostics + repair (bd-eign.4): engine-store capability,
    /// stale loop rows, orphaned loop PIDs, loops.json consistency. Human
    /// report by default, --json for machine. --fix drops stale rows + prunes
    /// orphaned pid entries.
    Doctor {
        /// auto-repair: reject stale invocations, prune orphaned pids
        #[arg(long)]
        fix: bool,
        /// machine-readable JSON health report
        #[arg(long)]
        json: bool,
    },
    /// Re-drive the recorded Msg journal through tick, assert identical actions (ADR-0025 Spec 3)
    Replay { project: String },
    /// Print (msg, state-delta, actions) triple for a journal seq (ADR-0025 Spec 3)
    Why { project: String, seq: usize },
}

/// What the loop should do for one agent on one poll — the SINGLE
/// definition lives in durable.rs (bd-w4-delete-dup-sbh8); the sync
/// (`--local`/`--once`) path delegates to `durable::plan_actions`.
/// Build the instruction queue from --queue / --workflow.
/// Clamp the active cadence to the 5s floor (consistent with the workflow's
/// `INTERVAL_MIN` in `next_interval_secs`). Pure — the warning is emitted by
/// the caller when the value changes.
#[cfg(test)]
fn clamp_active_interval(v: u64) -> u64 {
    v.max(5)
}

/// Sync-loop `--work` queue refresh (parity with the durable loop's
/// bead-driven mode): resume interrupted in-progress beads first, then hand
/// out fresh ready beads. Each bead is assigned at most once per loop run
/// (the `assigned` set is per-invocation state). Pure — fully testable.
/// bd-g0ge: the window value the loop resume filter gates on — keyed on
/// the live attempt generation when an attempt row exists, else the legacy
/// bead key. Pure: the window and the attempts arrive as values, so the
/// filter and its test read the same lookup.
#[cfg(test)]
fn attempt_window_last_us<'a>(
    window: &std::collections::BTreeMap<String, i64>,
    attempts: &'a [durable::WorkAttempt],
    bead_id: &str,
    pane: &str,
) -> (Option<i64>, Option<&'a durable::WorkAttempt>) {
    let live = durable::live_attempt_for_bead(attempts, bead_id);
    let last = match live {
        Some(a) => window
            .get(&crate::state::attempt_resume_key(
                bead_id,
                pane,
                a.generation,
            ))
            .copied(),
        None => window
            .get(&crate::state::resume_key(bead_id, pane))
            .copied(),
    };
    (last, live)
}

#[cfg(test)]
fn work_prompts(
    inprog: &[durable::ReadyBead],
    ready: &[durable::ReadyBead],
    assigned: &mut Vec<String>,
    project: &str,
) -> Vec<String> {
    let mut prompts = Vec::new();
    let mut fresh: Vec<&durable::ReadyBead> = Vec::new();
    for bead in inprog {
        if !assigned.contains(&bead.id) {
            prompts.push(durable::brief(&bead.id, &bead.title, project, true));
            fresh.push(bead);
        }
    }
    for bead in ready {
        if !assigned.contains(&bead.id) {
            prompts.push(durable::brief(&bead.id, &bead.title, project, false));
            fresh.push(bead);
        }
    }
    assigned.extend(fresh.into_iter().map(|b| b.id.clone()));
    prompts
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
mod verifier_dispatch_tests {
    use super::*;

    /// Scratch workspace: git + br + one bead with a typed `verify` field.
    fn scratch_vd(dir: &std::path::Path, slug: &str, priority: &str) -> String {
        crate::br::scratch_bead(
            dir,
            "workflow:\n  strict: true\n  statuses: [open, in_progress, closed, deferred, blocked]\n  transitions:\n    open: [in_progress, closed, deferred, blocked]\n    in_progress: [open, closed, deferred]\n    deferred: [open]\n    closed: [open]\n",
            "vd",
            slug,
            priority,
            Some("cargo test -p flywheel verifier"),
            Some("prove-it-works — fixture carries runnable VERIFY fence, not a self-report"),
            None,
        )
    }

    fn status_of(dir: &std::path::Path, bead_id: &str) -> String {
        crate::br::bead_status(dir, bead_id)
    }

    /// A config whose mail backend fails FAST (daemon at port 1) so the
    /// degrade test never stalls on a real relay round-trip.
    fn no_mail_cfg() -> Config {
        Config {
            mail: Some(crate::config::MailSections {
                toron: Some(crate::config::MailServerConfig {
                    url: Some("http://127.0.0.1:1".to_string()),
                    token: None,
                }),
            }),
            ..Default::default()
        }
    }

    fn write_registry(toml: &str) {
        let cfg_dir = crate::config::config_dir();
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("models.toml"), toml).unwrap();
    }

    /// Fixture 3 (ADR-0019 Spec 4): single-family swarm + P0 bead ->
    /// blocked `verifier-family-missing`; idempotent on re-invocation;
    /// never reopened/respawned.
    #[test]
    fn single_family_p0_is_blocked_not_reopened() {
        let _g = crate::config::lock_env();
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("vd-degrade-{}", std::process::id()));
        let bead_id = scratch_vd(&dir, "vd-deg", "0");
        write_registry("[models.\"deepseek/deepseek-v4\"]\n[models.\"deepseek/deepseek-v3\"]\n");
        let out = crate::cmd_internal_verify_dispatch(
            &cfg,
            "vd-test",
            &bead_id,
            crate::cmd::internal::VerifyDispatchOpts {
                worker_model: Some("deepseek-v4".to_string()),
                candidates: Some("deepseek-v4,deepseek-v3".to_string()),
                blast: "normal".to_string(),
                ac: "checkable".to_string(),
                dir: Some(dir.clone()),
            },
        );
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(status_of(&dir, &bead_id), "blocked");
        let out2 = crate::cmd_internal_verify_dispatch(
            &cfg,
            "vd-test",
            &bead_id,
            crate::cmd::internal::VerifyDispatchOpts {
                worker_model: Some("deepseek-v4".to_string()),
                candidates: Some("deepseek-v4,deepseek-v3".to_string()),
                blast: "normal".to_string(),
                ac: "checkable".to_string(),
                dir: Some(dir.clone()),
            },
        );
        assert!(out2.is_ok(), "{out2:?}");
        assert_eq!(status_of(&dir, &bead_id), "blocked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fixture 1 (decision half): P0 with a second family -> dispatch the
    /// different-family model; the bead is NOT blocked.
    #[test]
    fn p0_with_second_family_dispatches_and_is_not_blocked() {
        let _g = crate::config::lock_env();
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("vd-dispatch-{}", std::process::id()));
        let bead_id = scratch_vd(&dir, "vd-disp", "0");
        write_registry("[models.\"deepseek/deepseek-v4\"]\n[models.\"anthropic/opus-5\"]\n");
        let out = crate::cmd_internal_verify_dispatch(
            &cfg,
            "vd-test",
            &bead_id,
            crate::cmd::internal::VerifyDispatchOpts {
                worker_model: Some("deepseek-v4".to_string()),
                candidates: Some("deepseek-v4,opus-5".to_string()),
                blast: "normal".to_string(),
                ac: "checkable".to_string(),
                dir: Some(dir.clone()),
            },
        );
        assert!(out.is_ok(), "{out:?}");
        assert_ne!(status_of(&dir, &bead_id), "blocked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cheap band (P3, normal, loop-runnable): none-required — no side
    /// effect even in a single-family swarm (the loop verifies itself).
    #[test]
    fn cheap_band_is_none_required() {
        let _g = crate::config::lock_env();
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("vd-cheap-{}", std::process::id()));
        let bead_id = scratch_vd(&dir, "vd-cheap", "3");
        write_registry("[models.\"deepseek/deepseek-v4\"]\n[models.\"deepseek/deepseek-v3\"]\n");
        let out = crate::cmd_internal_verify_dispatch(
            &cfg,
            "vd-test",
            &bead_id,
            crate::cmd::internal::VerifyDispatchOpts {
                worker_model: Some("deepseek-v4".to_string()),
                candidates: Some("deepseek-v4,deepseek-v3".to_string()),
                blast: "normal".to_string(),
                ac: "checkable".to_string(),
                dir: Some(dir.clone()),
            },
        );
        assert!(out.is_ok(), "{out:?}");
        assert_ne!(status_of(&dir, &bead_id), "blocked");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod loop_run_verify_tests {
    use super::*;

    fn scratch_lrv(dir: &std::path::Path, slug: &str, verify: &str) -> String {
        crate::br::scratch_bead(
            dir,
            "workflow:\n  transitions:\n    open:\n      - closed\n    closed:\n      - closed\n  gates:\n    open -> closed:\n      require_all:\n        - command-verified\n    closed -> closed:\n      require_all:\n        - command-verified\n",
            "lrv",
            slug,
            "3",
            Some(verify),
            None,
            None,
        )
    }

    fn commit_and_sha(dir: &std::path::Path, bead_id: &str) -> String {
        std::fs::write(dir.join("work.txt"), "work\n").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(add.status.success());
        let cm = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                &format!("feat: lrv ({bead_id})"),
            ])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(cm.status.success());
        let log = std::process::Command::new("git")
            .args([
                "log",
                "--all",
                "-1",
                &format!("--grep={bead_id}"),
                "--format=%H",
            ])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&log.stdout).trim().to_string()
    }

    fn status_of(dir: &std::path::Path, bead_id: &str) -> String {
        crate::br::bead_status(dir, bead_id)
    }

    fn no_mail_cfg() -> Config {
        Config {
            mail: Some(crate::config::MailSections {
                toron: Some(crate::config::MailServerConfig {
                    url: Some("http://127.0.0.1:1".to_string()),
                    token: None,
                }),
            }),
            ..Default::default()
        }
    }

    /// Fixture: cheap P3 loop-runnable VERIFY -> the loop executes it,
    /// records command-verified (machine receipt + gate row), the ledger is
    /// legal and the bead STAYS closed (never reopened).
    #[test]
    fn cheap_p3_loop_runnable_records_and_stays_closed() {
        let _lock = crate::verdict::lock_loop_reporter();
        crate::verdict::test_reset_loop_reporter();
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("lrv-ok-{}", std::process::id()));
        let bead_id = scratch_lrv(&dir, "lrv-ok", "test -f work.txt");
        let sha = commit_and_sha(&dir, &bead_id);
        crate::br::run_br(
            Some(&dir),
            &[
                "close",
                bead_id.as_str(),
                "--reason",
                "worker done",
                "--commit-sha",
                sha.as_str(),
            ],
        )
        .unwrap();
        let o = crate::cmd::internal::loop_run_verify_exec(
            &cfg,
            "lrv-test",
            &bead_id,
            &sha,
            Some(60),
            Some(&dir),
        )
        .unwrap();
        assert_eq!(o.band, "loop-runnable");
        assert_eq!(o.exit, 0, "verify output: {}", o.output_tail);
        assert!(!o.skipped);
        crate::verdict::arm_loop_reporter();
        let receipt =
            crate::verdict::write_command_verified_receipt(&o.dir, &bead_id, &sha, &o.output_tail)
                .unwrap();
        assert_eq!(
            receipt,
            dir.join(format!(
                ".flywheel/receipts/{bead_id}-{sha}.command-verified.txt"
            ))
        );
        let v = crate::verdict::Verdict {
            kind: crate::verdict::VerdictKind::CommandVerified,
            bead_id: bead_id.clone(),
            sha: sha.clone(),
            at: chrono::Utc::now(),
            actor: crate::verdict::AgentName::new("flywheel-toron-loop1"),
            receipt_path: receipt,
            verify_cmd: o.verify,
        };
        crate::verdict::report_pass(&o.dir, &v).unwrap();
        let bead = crate::verdict::bead_from_br_show(&dir, &bead_id).unwrap();
        assert!(
            crate::verdict::ledger_check(&o.dir, &bead, crate::verdict::AcShape::Checkable, &sha)
                .unwrap()
        );
        assert_eq!(status_of(&dir, &bead_id), "closed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// VERIFY failing -> failed-head marker written; the same head is NEVER
    /// re-verified (second invocation skips).
    #[test]
    fn failing_verify_marks_head_and_never_rereuns_same_head() {
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("lrv-fail-{}", std::process::id()));
        let bead_id = scratch_lrv(&dir, "lrv-fail", "test -f does-not-exist.txt");
        let sha = commit_and_sha(&dir, &bead_id);
        let o1 = crate::cmd::internal::loop_run_verify_exec(
            &cfg,
            "lrv-test",
            &bead_id,
            &sha,
            Some(60),
            Some(&dir),
        )
        .unwrap();
        assert_eq!(o1.band, "loop-runnable");
        assert_ne!(o1.exit, 0, "the fixture VERIFY must fail");
        let marker = dir.join(format!(".flywheel/loop-run-verify-failed/{bead_id}-{sha}"));
        assert!(marker.exists(), "failed-head marker must be written");
        let o2 = crate::cmd::internal::loop_run_verify_exec(
            &cfg,
            "lrv-test",
            &bead_id,
            &sha,
            Some(60),
            Some(&dir),
        )
        .unwrap();
        assert!(
            o2.skipped,
            "never re-verify the same broken head — second run must skip"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whole-workspace / heavy VERIFY is NOT the loop-runnable band.
    #[test]
    fn heavy_verify_is_non_runnable_band() {
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("lrv-heavy-{}", std::process::id()));
        let bead_id = scratch_lrv(&dir, "lrv-heavy", "cargo test --workspace");
        let sha = commit_and_sha(&dir, &bead_id);
        let o = crate::cmd::internal::loop_run_verify_exec(
            &cfg,
            "lrv-test",
            &bead_id,
            &sha,
            Some(60),
            Some(&dir),
        )
        .unwrap();
        assert_eq!(o.band, "non-runnable");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use crate::cmd::internal::{
        first_routable, planner_recipient_chain, registered_orchestrator, reopen_unverified_close,
        send_done_mail, send_not_ready_mail, send_unverified_close_mail,
    };
    use crate::cmd::send::bare_short_group_matches;
    use crate::cmd::status::progress_waves;
    use crate::{
        WantToken, agent_name_n, agent_ordinal, doorbell_uncovered_panes, free_spawn_ordinal,
        kill_key, name_matches_want, resolve_send_targets_dyn, slugify_label, spawn_agent_profile,
        watch_name_for_pane,
    };

    /// bd-8hv0: the sweep must re-subscribe only panes the push
    /// spine does not already cover; empty ids never count as new.
    #[test]
    fn doorbell_delta_covers_only_uncovered_panes() {
        let covered: HashSet<String> = ["w1:p1".to_string()].into_iter().collect();
        let ids = vec![
            "w1:p1".to_string(),
            String::new(),
            "w2:p9".to_string(),
            "w3:p1".to_string(),
        ];
        assert_eq!(
            doorbell_uncovered_panes(&covered, &ids),
            vec!["w2:p9".to_string(), "w3:p1".to_string()]
        );
        let all: HashSet<String> = ids.into_iter().filter(|p| !p.is_empty()).collect();
        assert_eq!(
            doorbell_uncovered_panes(&all, &["w1:p1".to_string()]),
            [] as [String; 0]
        );
    }

    fn st(name: &str, s: AgentStatus) -> durable::AgentState {
        durable::AgentState {
            name: name.to_string(),
            status: format!("{s:?}").to_ascii_lowercase(),
            kind: String::new(),
            pane_id: String::new(),
            mail_identity: None,
            nudge_streak: 0,
        }
    }

    #[test]
    fn active_interval_clamps_to_floor() {
        assert_eq!(clamp_active_interval(3), 5);
        assert_eq!(clamp_active_interval(0), 5);
        assert_eq!(clamp_active_interval(5), 5);
        assert_eq!(clamp_active_interval(15), 15);
        assert_eq!(clamp_active_interval(30), 30);
    }

    /// bd-g0ge: the loop resume filter keys the window on the live
    /// generation — a stale bead-level watermark must not re-arm a
    /// completed attempt, and a fresh generation window must pass.
    #[test]
    fn resume_attempt_window_keys_live_generation() {
        use std::collections::BTreeMap;
        let now_us = 1_000_000_000_000_000i64;
        let mut window: BTreeMap<String, i64> = BTreeMap::new();
        window.insert(state::resume_key("bd-x", "PaneA"), now_us - 60 * 1_000_000);
        let attempts = vec![durable::WorkAttempt {
            bead_id: "bd-x".into(),
            pane: "PaneA".into(),
            generation: 3,
            status: durable::WorkAttemptStatus::Running,
            corr: None,
            effect_key: None,
            updated_us: now_us,
        }];
        let live = durable::live_attempt_for_bead(&attempts, "bd-x");
        assert!(live.is_some_and(|a| a.generation == 3));
        let (last, live) = attempt_window_last_us(&window, &attempts, "bd-x", "PaneA");
        assert_eq!(last, None);
        assert!(durable::should_resume_attempt(
            last,
            now_us,
            durable::RESUME_WINDOW_DEFAULT_SECS,
            false,
            live,
        ));
        window.insert(state::resume_key("bd-y", "PaneA"), now_us);
        let done = vec![durable::WorkAttempt {
            bead_id: "bd-y".into(),
            pane: "PaneA".into(),
            generation: 1,
            status: durable::WorkAttemptStatus::Completed,
            corr: None,
            effect_key: None,
            updated_us: now_us,
        }];
        let (last_done, live_done) = attempt_window_last_us(&window, &done, "bd-y", "PaneA");
        assert!(!durable::should_resume_attempt(
            last_done,
            now_us,
            durable::RESUME_WINDOW_DEFAULT_SECS,
            false,
            live_done,
        ));
    }

    #[test]
    fn work_prompts_resumes_inprogress_before_fresh() {
        let inprog = vec![
            durable::ReadyBead {
                id: "bd-r1".into(),
                title: "resume me".into(),
                assignee: None,
                verify: None,
                labels: Vec::new(),
                priority: 3,
                principles: String::new(),
                updated_at: None,
                ..Default::default()
            },
            durable::ReadyBead {
                id: "bd-r2".into(),
                title: "resume me too".into(),
                assignee: None,
                verify: None,
                labels: Vec::new(),
                priority: 3,
                principles: String::new(),
                updated_at: None,
                ..Default::default()
            },
        ];
        let ready = vec![durable::ReadyBead {
            id: "bd-f1".into(),
            title: "fresh".into(),
            assignee: None,
            verify: None,
            labels: Vec::new(),
            priority: 3,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        }];
        let mut assigned = Vec::new();
        let prompts = work_prompts(&inprog, &ready, &mut assigned, "flywheel");
        assert_eq!(prompts.len(), 3);
        assert!(prompts[0].contains("GOAL Resume bd-r1"));
        assert!(prompts[1].contains("GOAL Resume bd-r2"));
        assert!(prompts[2].contains("GOAL Execute bd-f1"));
        assert_eq!(assigned, vec!["bd-r1", "bd-r2", "bd-f1"]);
        let again = work_prompts(&inprog, &ready, &mut assigned, "flywheel");
        assert_eq!(again, [] as [String; 0]);
    }

    #[test]
    fn work_prompts_skips_already_assigned() {
        let inprog = vec![durable::ReadyBead {
            id: "bd-r1".into(),
            title: "x".into(),
            assignee: None,
            verify: None,
            labels: Vec::new(),
            priority: 3,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        }];
        let ready = vec![durable::ReadyBead {
            id: "bd-r1".into(),
            title: "x".into(),
            assignee: None,
            verify: None,
            labels: Vec::new(),
            priority: 3,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        }];
        let mut assigned = vec!["bd-r1".to_string()];
        let prompts = work_prompts(&inprog, &ready, &mut assigned, "flywheel");
        assert_eq!(prompts, [] as [String; 0]);
    }

    #[test]
    fn work_prompts_builds_self_contained_prompts() {
        let ready = vec![durable::ReadyBead {
            id: "bd-42".into(),
            title: "fix auth".into(),
            assignee: None,
            verify: None,
            labels: Vec::new(),
            priority: 3,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        }];
        let mut assigned = Vec::new();
        let prompts = work_prompts(&[], &ready, &mut assigned, "flywheel");
        assert_eq!(prompts.len(), 1);
        let p = &prompts[0];
        assert!(p.contains("bd-42"));
        assert!(p.contains("GOAL Execute bd-42: fix auth."));
        assert!(p.contains("flywheel/.flywheel/standing-orders.md"));
        assert!(!p.contains("br update"));
    }

    /// bd-swarm-cockpit-pczs: `progress_waves` groups by `wave-N`, counts an
    /// open bead `ready` only when `br ready` listed it, and names the LOWEST
    /// wave holding ready/in-progress work as dispatching. Unlabeled beads
    /// never invent a wave; an unknown status reads unknown, never a default.
    #[test]
    fn progress_waves_groups_and_names_the_dispatching_wave() {
        use std::collections::HashSet;
        let rows = json!([
            {"id": "bd-a", "status": "closed", "labels": ["wave-1"]},
            {"id": "bd-b", "status": "in_progress", "labels": ["wave-1"]},
            {"id": "bd-c", "status": "open", "labels": ["wave-1"]},
            {"id": "bd-d", "status": "open", "labels": ["wave-2"]},
            {"id": "bd-e", "status": "open", "labels": ["wave-2"]},
            {"id": "bd-f", "status": "open", "labels": ["wave-2"]},
            {"id": "bd-g", "status": "open"},
            {"id": "bd-h", "status": "open", "labels": ["wave-2"]},
            {"id": "bd-i", "status": "mystery", "labels": ["wave-2"]},
            {"id": "bd-j", "status": "open", "wave": 2},
        ]);
        let ready: HashSet<String> = ["bd-h".to_string(), "bd-j".to_string()]
            .into_iter()
            .collect();
        let out = progress_waves(rows.as_array().unwrap(), &ready);
        assert_eq!(
            out["dispatching_wave"],
            json!(1),
            "wave 1 holds in-progress work"
        );
        let waves = out["waves"].as_array().unwrap();
        assert_eq!(waves.len(), 2, "two labeled waves");
        assert_eq!(waves[0]["wave"], json!(1));
        assert_eq!(waves[0]["closed"], json!(1));
        assert_eq!(waves[0]["in_progress"], json!(1));
        assert_eq!(
            waves[0]["ready"],
            json!(0),
            "bd-c is open but not in br ready"
        );
        assert_eq!(waves[0]["blocked"], json!(1));
        assert_eq!(
            waves[1]["ready"],
            json!(2),
            "bd-h by label, bd-j by the typed field — both dispatchable"
        );
        assert_eq!(waves[1]["blocked"], json!(3));
        assert_eq!(waves[1]["unknown"], json!(1), "no default status");
        assert_eq!(out["unlabeled"]["blocked"], json!(1));
        assert_eq!(out["unlabeled"]["total"], json!(1));

        let rows2 = json!([
            {"id": "bd-r1", "status": "open", "labels": ["wave-1"]},
            {"id": "bd-r2", "status": "in_progress", "labels": ["wave-2"]},
        ]);
        let ready2: HashSet<String> = ["bd-r1".to_string()].into_iter().collect();
        let out2 = progress_waves(rows2.as_array().unwrap(), &ready2);
        assert_eq!(out2["dispatching_wave"], json!(1));

        let rows3 = json!([{"id": "bd-z", "status": "closed", "labels": ["wave-1"]}]);
        let out3 = progress_waves(rows3.as_array().unwrap(), &HashSet::<String>::new());
        assert_eq!(out3["dispatching_wave"], Value::Null);
    }

    /// ADR-0019 acceptance: a bead without a fence must be refused at
    /// dispatch AND the planner must be mailed on the bead's thread. The
    /// not-ready mail lands in the planner's inbox with thread = bead id.
    #[test]
    fn verify_fence_not_ready_bead_mails_planner_on_bead_thread() {
        use crate::backend::MailSurface as _;
        let mail = crate::mail::Mail::from_config(None)
            .expect("mail ok")
            .expect("mail Some");
        let backend = crate::backend::toron::ToronMailBackend::new(mail);
        let project = "verify-fence-mail";
        backend::run_mail(backend.ensure_project(project)).expect("ensure");
        let sender = backend.agent_name().to_string();
        let sender_profile = spawn_agent_profile(&sender, "opencode", project);
        crate::mail::fixture_register(project, &sender_profile).expect("register sender");
        let profile = spawn_agent_profile("PlannerOne", "opencode", project);
        crate::mail::fixture_register(project, &profile).expect("register");
        let receipt = send_not_ready_mail(
            &backend,
            project,
            "bd-fence-missing",
            "some bead",
            "PlannerOne",
            None,
        )
        .expect("not-ready mail sends");
        assert!(receipt.count >= 1, "delivered: {receipt:?}");
        let items = backend::run_mail(backend.inbox(&backend::InboxQuery {
            project: project.into(),
            agent: "PlannerOne".into(),
            unread_only: false,
            limit: Some(10),
            include_bodies: false,
            urgent_only: false,
            since_ts: None,
            thread_id: None,
        }))
        .expect("inbox");
        let m = items
            .iter()
            .find(|m| m.subject.contains("NOT ready"))
            .expect("planner received the not-ready mail");
        assert_eq!(
            m.thread_id
                .as_ref()
                .map(toron_core::mailbox::ids::MessageId::as_str),
            Some("bd-fence-missing"),
            "mail rides the bead's thread (ADR-0019 Spec 0)"
        );
    }

    /// Programmatic done-mail (2026-08-18): the loop's close-report mail
    /// rides the bead's thread, subject `[<id>] done`, body carries the
    /// close-reason REPORT — the captain's copy that does not depend on the
    /// agent remembering the courtesy.
    #[test]
    fn done_mail_carries_close_report_on_bead_thread() {
        use crate::backend::MailSurface as _;
        let mail = crate::mail::Mail::from_config(None)
            .expect("mail ok")
            .expect("mail Some");
        let backend = crate::backend::toron::ToronMailBackend::new(mail);
        let project = "done-mail-test";
        backend::run_mail(backend.ensure_project(project)).expect("ensure");
        let sender = backend.agent_name().to_string();
        crate::mail::fixture_register(project, &spawn_agent_profile(&sender, "opencode", project))
            .expect("register sender");
        let captain = "CaptainPine";
        crate::mail::fixture_register(project, &spawn_agent_profile(captain, "flywheel", project))
            .expect("register captain");
        assert_eq!(
            registered_orchestrator(&backend, project).as_deref(),
            Some(captain),
            "orchestrator = the registered flywheel-program agent"
        );
        let receipt = send_done_mail(
            &backend,
            project,
            "bd-done-probe",
            "some bead",
            "status: closed; sha: abc123; verdict: command-verified; verify: cargo test -p x",
            captain,
        )
        .expect("done mail sends");
        assert!(receipt.count >= 1, "delivered: {receipt:?}");
        let items = backend::run_mail(backend.inbox(&backend::InboxQuery {
            project: project.into(),
            agent: captain.into(),
            unread_only: false,
            limit: Some(10),
            include_bodies: true,
            urgent_only: false,
            since_ts: None,
            thread_id: None,
        }))
        .expect("inbox");
        let m = items
            .iter()
            .find(|m| m.subject == "[bd-done-probe] done")
            .expect("captain received the done mail");
        assert_eq!(
            m.thread_id
                .as_ref()
                .map(toron_core::mailbox::ids::MessageId::as_str),
            Some("bd-done-probe"),
            "done mail rides the bead's thread"
        );
        assert!(
            m.body_md.contains("cargo test -p x"),
            "body carries the close-reason REPORT: {}",
            m.body_md
        );
    }

    /// ADR-0019 (bd-w1-reopen-f6tw): the unverified-close mail rides the
    /// bead's thread, subject `[<id>] unverified close reopened`, and NAMES
    /// what is missing (here: the commit).
    #[test]
    fn sha_rule_missing_commit_mail_names_the_missing_commit() {
        use crate::backend::MailSurface as _;
        let mail = crate::mail::Mail::from_config(None)
            .expect("mail ok")
            .expect("mail Some");
        let backend = crate::backend::toron::ToronMailBackend::new(mail);
        let project = "sha-rule-mail";
        backend::run_mail(backend.ensure_project(project)).expect("ensure");
        let sender = backend.agent_name().to_string();
        let sender_profile = spawn_agent_profile(&sender, "opencode", project);
        crate::mail::fixture_register(project, &sender_profile).expect("register sender");
        let profile = spawn_agent_profile("PlannerOne", "opencode", project);
        crate::mail::fixture_register(project, &profile).expect("register");
        send_unverified_close_mail(&backend, project, "bd-sha-missing", "", "PlannerOne")
            .expect("unverified-close mail sends");
        let items = backend::run_mail(backend.inbox(&backend::InboxQuery {
            project: project.into(),
            agent: "PlannerOne".into(),
            unread_only: false,
            limit: Some(10),
            include_bodies: false,
            urgent_only: false,
            since_ts: None,
            thread_id: None,
        }))
        .expect("inbox");
        let m = items
            .iter()
            .find(|m| m.subject.contains("unverified close reopened"))
            .expect("planner received the unverified-close mail");
        assert_eq!(
            m.thread_id
                .as_ref()
                .map(toron_core::mailbox::ids::MessageId::as_str),
            Some("bd-sha-missing"),
            "mail rides the bead's thread (ADR-0019 Spec 0)"
        );
        assert!(
            m.body_md.contains("bd-sha-missing"),
            "mail names the bead whose commit is missing"
        );
    }

    /// Scratch workspace (git + br + one bead with a typed `verify` field +
    /// a commit whose message carries the bead id + gate policy).
    /// bd-3566 (beads_rust-wave5-flywheel-parser-strip-dkpvi): the fixture
    /// sets the TYPED verify field (`br create --verify`) — a description
    /// fence no longer makes a bead ready under the schema-v18 predicate,
    /// and after bypass-close + reopen the fence-import never runs, so the
    /// reopened bead would be missing from `br ready` (the env-dependency
    /// this test used to trip on).
    fn scratch_workspace(tag: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("{tag}-{}", std::process::id()));
        let bead_id = crate::br::scratch_bead(
            &dir,
            "workflow:\n  transitions:\n    open:\n      - closed\n  gates:\n    open -> closed:\n      require_all:\n        - command-verified\n",
            "reopen fixture",
            "reopen-fix-1",
            "3",
            Some("cargo test -p flywheel reopen::"),
            None,
            Some("reopen fixture"),
        );
        (dir, bead_id)
    }

    /// THE RED-THEN-GREEN OF THE PROGRAM: a worker closes with no gate row
    /// -> close-check says MISSING -> `reopen_unverified_close` reopens ->
    /// the bead is back in `br ready`. Repeated ticks (same bead, same sha)
    /// mail exactly ONCE (marker dedup) — the reopen storm guard.
    #[test]
    fn reopen_flow_reopens_unverified_close_and_mails_once() {
        use crate::backend::MailSurface as _;
        let (dir, bead_id) = scratch_workspace("reopen-flow");
        let dir_str = dir.to_string_lossy().to_string();
        crate::br::run_br_with_env(
            Some(&dir),
            &[
                "close",
                bead_id.as_str(),
                "--reason",
                "worker says done",
                "--bypass-policy",
                "--bypass-reason",
                "test fixture: illegal no-gate-row close, loop floor must catch it",
            ],
            &[("BR_OPERATOR", "1")],
        )
        .expect("br close bypass");
        match verdict::close_check(&dir, &bead_id).unwrap() {
            verdict::CloseCheck::Missing {
                sha,
                receipt_eligible,
            } => {
                assert!(!receipt_eligible, "loop-runnable -> no receipt grace");
                let mail = crate::mail::Mail::from_config(None)
                    .expect("mail ok")
                    .expect("mail Some");
                let backend = crate::backend::toron::ToronMailBackend::new(mail);
                let project = dir_str.clone();
                backend::run_mail(backend.ensure_project(&project)).expect("ensure");
                let sender = backend.agent_name().to_string();
                let sender_profile = spawn_agent_profile(&sender, "opencode", &project);
                crate::mail::fixture_register(&project, &sender_profile).expect("register sender");
                let planner = planner_recipient_chain(&dir_str, &bead_id, "FallbackIdentity")
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let planner_profile = spawn_agent_profile(&planner, "opencode", &project);
                crate::mail::fixture_register(&project, &planner_profile)
                    .expect("register planner");
                let v = reopen_unverified_close(&dir_str, &bead_id, &sha, &backend, &planner)
                    .expect("reopen tick 1");
                assert_eq!(v["reopened"].as_bool(), Some(true), "reopened: {v}");
                let ready = crate::br::Bead::ready(Some(&dir)).expect("br ready");
                assert!(
                    ready.iter().any(|b| b.id == bead_id),
                    "reopened bead is back in br ready: {ready:?}"
                );
                let v2 = reopen_unverified_close(&dir_str, &bead_id, &sha, &backend, &planner)
                    .expect("reopen tick 2");
                assert_eq!(v2["already_processed"].as_bool(), Some(true), "dedup: {v2}");
                let items = backend::run_mail(backend.inbox(&backend::InboxQuery {
                    project: project.clone(),
                    agent: planner.clone(),
                    unread_only: false,
                    limit: Some(10),
                    include_bodies: false,
                    urgent_only: false,
                    since_ts: None,
                    thread_id: None,
                }))
                .expect("inbox");
                let mails: Vec<_> = items
                    .iter()
                    .filter(|m| m.subject.contains("unverified close reopened"))
                    .collect();
                assert_eq!(
                    mails.len(),
                    1,
                    "exactly one mail per (bead, sha): {items:?}"
                );
                assert_eq!(
                    mails[0]
                        .thread_id
                        .as_ref()
                        .map(toron_core::mailbox::ids::MessageId::as_str),
                    Some(bead_id.as_str()),
                    "on the bead's thread"
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The planner to notify is the bead's creator (`br show --json` →
    /// `created_by`), falling back to the configured identity when the
    /// creator cannot be resolved.
    #[test]
    fn verify_fence_resolve_planner_uses_bead_creator() {
        let dir = std::env::temp_dir().join(format!("verify-fence-planner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::br::run_br(Some(&dir), &["init"]).expect("br init");
        crate::br::Bead::create(
            Some(&dir),
            &[
                "create",
                "--title",
                "fixture",
                "--description",
                "body",
                "--verify",
                "br list --json",
                "--principle",
                "prove-it-works — fixture carries runnable VERIFY fence, not a self-report",
            ],
        )
        .expect("br create");
        let list = crate::br::run_br(Some(&dir), &["list", "--json"]).expect("br list");
        let v: Value = serde_json::from_str(&list).expect("br list json");
        let id = v["issues"][0]["id"].as_str().expect("bead id").to_string();
        let dir_str = dir.to_string_lossy().to_string();
        let planner = planner_recipient_chain(&dir_str, &id, "FallbackIdentity")
            .into_iter()
            .next()
            .unwrap_or_default();
        assert!(
            !planner.is_empty() && planner != "FallbackIdentity",
            "creator resolved, got: {planner}"
        );
        assert_eq!(
            planner_recipient_chain(&dir_str, "bd-does-not-exist", "FallbackIdentity"),
            vec!["FallbackIdentity".to_string()],
            "unresolvable bead -> the configured operator is the only candidate"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_short_group_matches_detects_crew() {
        let names = vec![
            "flywheel-proj-oc".to_string(),
            "flywheel-proj-oc2".to_string(),
            "flywheel-proj-oc3".to_string(),
        ];
        let group = bare_short_group_matches("proj", "oc", &names).expect("oc broadens");
        assert_eq!(group.len(), 3);
        assert!(group.contains(&"flywheel-proj-oc".to_string()));
        assert!(group.contains(&"flywheel-proj-oc3".to_string()));
        assert!(bare_short_group_matches("proj", "oc2", &names).is_none());
        assert!(bare_short_group_matches("proj", "flywheel-proj-oc", &names).is_none());
        assert!(bare_short_group_matches("proj", "all", &names).is_none());
    }

    #[test]
    fn bare_short_group_matches_single_agent_is_fine() {
        let names = vec!["flywheel-proj-oc".to_string()];
        assert!(bare_short_group_matches("proj", "oc", &names).is_none());
    }

    /// bd-rex-default-plane-xn1t: the default send path on a rex plane
    /// resolves `--to <pin>` through the shared exact rule and prompts once
    /// on the pin's block — with a dead hc socket anywhere near it.
    #[cfg(feature = "backend-superlogical")]
    const REX_SEND_FAKE_REX: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> @@LOG@@
cmd=""
for arg in "$@"; do
  case "$arg" in
    ls|session|block|capture|api|run|split|new|do) cmd=$arg; break ;;
  esac
done
sub=""
for arg in "$@"; do
  case "$arg" in
    program_status|send-key|write) sub=$arg ;;
    *list_blocks*) sub=list_blocks ;;
  esac
done
case "$cmd:$sub" in
  ls:)
    printf '%s\n' '{"sessions":[{"session_id":"session:fake-9","label":"flywheel-probe-send-rex-default","pane_count":1,"tab_count":1}]}'
    ;;
  api:list_blocks)
    printf '%s\n' '{"blocks":[{"block_id":"block:fake-1","window_id":"window:fake-1","label":"NeatHeron","creator_name":"com.superlogical.terminal"}]}'
    ;;
  block:program_status)
    printf '%s\n' '{"records":[{"state":"idle"}]}'
    ;;
  block:write)
    printf '%s\n' '{}'
    ;;
  block:send-key)
    printf '%s\n' 'keyed'
    ;;
  *)
    echo "unexpected $*" >&2
    exit 1
    ;;
esac
"#;

    /// The pin resolves and exactly one prompt lands on the pin's block.
    /// No registry row exists, so this drives the shared exact-match rule;
    /// a dead hc socket is present in the environment shape (no socket
    /// is ever touched — the log must stay hc-free).
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn rex_send_resolves_pin_and_prompts_once_on_the_rex_target() {
        let guard = crate::backend::rex_test_guard::RexBinGuard::install(REX_SEND_FAKE_REX);
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "superlogical") };
        let cfg = crate::config::Config::default();
        let out = crate::cmd::send::cmd_send_rex(
            &cfg,
            "probe-send-rex-default",
            "NeatHeron",
            "hello probe",
            None,
            false,
            None,
            false,
            true,
            None,
        );
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        out.expect("rex send of one prompt");
        let log = guard.log_text();
        assert_eq!(
            log.lines().filter(|l| l.contains("write")).count(),
            1,
            "exactly one prompt write:\n{log}"
        );
        assert_eq!(
            log.lines().filter(|l| l.contains("send-key")).count(),
            1,
            "one submit after the write:\n{log}"
        );
        assert!(!log.contains("hc"), "no hc contact:\n{log}");
    }

    /// Registry ground truth: a lane name resolves to its pane even though
    /// no block carries the lane label (spawn renamed the block to the pin).
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn rex_send_registry_lane_name_resolves_to_pane_ground_truth() {
        fn send_agent(name: Option<&str>, pane: &str) -> AgentInfo {
            AgentInfo {
                agent: None,
                agent_status: AgentStatus::Idle,
                cwd: None,
                display_agent: None,
                focused: false,
                foreground_cwd: None,
                interactive_ready: false,
                launch_pending: false,
                name: name.map(str::to_string),
                pane_id: pane.to_string(),
                revision: 0,
                state_change_seq: 0,
                completion_seq: None,
                state_labels: serde_json::Map::new(),
                tab_id: "t1".into(),
                terminal_id: "term".into(),
                terminal_title: None,
                workspace_id: "session:fake-9".into(),
            }
        }
        let agents = vec![send_agent(Some("NeatHeron"), "block:fake-1")];
        let registry = vec![state::SpawnEntry::with_mail_identity(
            "block:fake-1",
            "probe-send-rex-default-coder",
            "NeatHeron",
        )];
        let matched =
            resolve_send_targets_dyn(&agents, &registry, "probe-send-rex-default", "NeatHeron")
                .expect("pin resolves");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].pane_id, "block:fake-1");
        let lane = resolve_send_targets_dyn(
            &agents,
            &registry,
            "probe-send-rex-default",
            "probe-send-rex-default-coder",
        )
        .expect("lane resolves through the registry");
        assert_eq!(lane.len(), 1);
        assert_eq!(lane[0].pane_id, "block:fake-1");
    }

    /// Ambiguity is typed and prompts nothing: two blocks under one display
    /// name refuse with `matches N panes; not ringing` and the fake log
    /// shows no write.
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn rex_send_ambiguity_is_typed_and_prompts_nothing() {
        let script = REX_SEND_FAKE_REX.replace(
            r#"{"blocks":[{"block_id":"block:fake-1","window_id":"window:fake-1","label":"NeatHeron","creator_name":"com.superlogical.terminal"}]}"#,
            r#"{"blocks":[{"block_id":"block:fake-1","window_id":"window:fake-1","label":"NeatHeron"},{"block_id":"block:fake-2","window_id":"window:fake-1","label":"NeatHeron"}]}"#,
        );
        let guard = crate::backend::rex_test_guard::RexBinGuard::install(&script);
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "superlogical") };
        let cfg = crate::config::Config::default();
        let err = crate::cmd::send::cmd_send_rex(
            &cfg,
            "probe-send-rex-default",
            "NeatHeron",
            "hello probe",
            None,
            false,
            None,
            false,
            true,
            None,
        )
        .expect_err("two blocks under one name must refuse");
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("matches multiple panes; not ringing"),
            "typed ambiguity, not a miss: {msg}"
        );
        let log = guard.log_text();
        assert!(
            !log.lines().any(|l| l.contains("write")),
            "ambiguity prompts nothing:\n{log}"
        );
    }

    /// Fence: the non-hc send arm is selected by the RESOLVED plane and
    /// builds no hc client. Both halves are asserted so neither direction is
    /// vacuous: the route must exist AND the hc constructor must be gone.
    #[test]
    fn rex_send_arm_constructs_no_plane_client() {
        let src = include_str!("send.rs");
        let send = src
            .split_once("\npub(crate) fn cmd_send(")
            .expect("cmd_send exists")
            .1
            .split_once("\npub(crate) fn mint_task_id()")
            .expect("cmd_send ends before mint_task_id")
            .0;
        assert!(
            send.contains("cmd_send_rex("),
            "cmd_send must route to the rex arm"
        );
        assert!(
            send.contains("resolve_plane(cfg).plane"),
            "the route is the RESOLVED plane, never REX_SESSION or the configured name"
        );
        let rex = src
            .split_once("\npub(crate) fn cmd_send_rex(")
            .expect("cmd_send_rex exists")
            .1
            .split_once("\npub(crate) fn cmd_send(")
            .expect("cmd_send_rex ends at cmd_send")
            .0;
        assert!(
            rex.contains("multiplexer_backend"),
            "the rex arm resolves through the selected backend"
        );
        assert!(
            rex.contains("backend.prompt("),
            "the rex arm prompts through the backend trait"
        );
        assert!(
            !rex.contains("Client::connect"),
            "no hc client on the rex arm"
        );
        assert!(
            !rex.contains("hc::Client"),
            "no hc client type on the rex arm"
        );
    }

    #[test]
    fn kill_key_detects_only_kill_keys() {
        assert_eq!(kill_key(&["Escape"]), None);
        assert_eq!(kill_key(&["ctrl+c"]), Some("ctrl+c"));
        assert_eq!(kill_key(&["ctrl-c"]), Some("ctrl-c"));
        assert_eq!(kill_key(&["^c"]), Some("^c"));
        assert_eq!(kill_key(&["Escape", "ctrl+c"]), Some("ctrl+c"));
        assert_eq!(kill_key(&[]), None);
    }

    #[test]
    fn slugify_label_normalizes() {
        let (s, changed) = slugify_label("ADR-0008-debate");
        assert_eq!(s, "adr-0008-debate");
        assert!(changed);
        let (s, _) = slugify_label("My Cool  Label!");
        assert_eq!(s, "my-cool-label");
        let (s, _) = slugify_label("--leading-and-trailing--");
        assert_eq!(s, "leading-and-trailing");
        let (s, changed) = slugify_label("flywheel-toron");
        assert_eq!(s, "flywheel-toron");
        assert!(!changed, "already a slug");
        let (s, _) = slugify_label("!!!");
        assert_eq!(s, "", "no alphanumerics -> empty (caller rejects)");
    }

    #[test]
    fn bare_short_group_matches_comma_list() {
        let names = vec![
            "flywheel-proj-oc".to_string(),
            "flywheel-proj-oc2".to_string(),
            "flywheel-proj-pi".to_string(),
        ];
        let group = bare_short_group_matches("proj", "oc,pi", &names).unwrap();
        assert_eq!(group.len(), 3);
        assert!(group.contains(&"flywheel-proj-pi".to_string()));
    }

    #[test]
    fn done_agent_pops_queue_like_idle() {
        let states = vec![st("a", AgentStatus::Done)];
        let q = vec!["p1".into(), "p6".into()];
        let actions = durable::plan_actions(
            &crate::durable::LoopModel {
                queue: q.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            durable::LoopAction::Send { name, text } => {
                assert_eq!(name, "a");
                assert_eq!(text, "p1");
            }
            other => panic!("expected Send for done agent, got {other:?}"),
        }
        let states = vec![st("a", AgentStatus::Done)];
        let q2: Vec<String> = vec![];
        let actions = durable::plan_actions(
            &crate::durable::LoopModel {
                queue: q2.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
            },
            &states,
            Some("continue"),
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        assert!(
            matches!(actions[0], durable::LoopAction::Nudge { .. }),
            "{actions:?}"
        );
    }

    #[test]
    fn sync_loop_substitutes_agent_name_at_dispatch() {
        let states = vec![st("flywheel-proj-oc", AgentStatus::Idle)];
        let q = vec!["write to .flywheel/agent-state/{AGENT_NAME}.json".into()];
        let actions = durable::plan_actions(
            &crate::durable::LoopModel {
                queue: q.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            durable::LoopAction::Send { text, .. } => {
                assert!(text.contains(".flywheel/agent-state/flywheel-proj-oc.json"));
                assert!(!text.contains("{AGENT_NAME}"));
            }
            other => panic!("expected Send, got {other:?}"),
        }
        let states = vec![st("a", AgentStatus::Idle)];
        let q = vec!["t1".into()];
        let actions = durable::plan_actions(
            &crate::durable::LoopModel {
                queue: q.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            durable::LoopAction::Send { text, .. } => assert_eq!(text, "t1"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn name_matching_handles_numbered_agents() {
        let base = "flywheel-proj-";
        let want_oc = WantToken {
            name: format!("{base}oc"),
            broaden: true,
            short: None,
            ordinal: 1,
            pane: None,
        };
        assert!(name_matches_want(&format!("{base}oc"), &want_oc));
        assert!(name_matches_want(&format!("{base}oc2"), &want_oc));
        assert!(name_matches_want(&format!("{base}oc10"), &want_oc));
        assert!(!name_matches_want(&format!("{base}ocr"), &want_oc));
        assert!(!name_matches_want(&format!("{base}ocx"), &want_oc));
        let want_exact = WantToken {
            name: format!("{base}oc"),
            broaden: false,
            short: None,
            ordinal: 1,
            pane: None,
        };
        assert!(name_matches_want(&format!("{base}oc"), &want_exact));
        assert!(!name_matches_want(&format!("{base}oc2"), &want_exact));
        let want_oc2 = WantToken {
            name: format!("{base}oc2"),
            broaden: false,
            short: None,
            ordinal: 1,
            pane: None,
        };
        assert!(name_matches_want(&format!("{base}oc2"), &want_oc2));
        assert!(!name_matches_want(&format!("{base}oc20"), &want_oc2));
    }

    #[test]
    fn agent_naming_scheme() {
        assert_eq!(agent_name_n("proj", "oc", 1), "flywheel-proj-oc");
        assert_eq!(agent_name_n("proj", "oc", 2), "flywheel-proj-oc2");
        assert_eq!(agent_name_n("a/b/proj", "pi", 3), "flywheel-proj-pi3");
        assert_eq!(agent_ordinal("flywheel-proj-oc", "proj", "oc"), Some(1));
        assert_eq!(agent_ordinal("flywheel-proj-oc2", "proj", "oc"), Some(2));
        assert_eq!(agent_ordinal("flywheel-proj-ocr", "proj", "oc"), None);
        assert_eq!(agent_ordinal("flywheel-proj-gk", "proj", "oc"), None);
    }

    #[test]
    fn spawn_retry_bumps_ordinal_past_taken_names() {
        use std::collections::HashSet;
        let taken: HashSet<String> = ["flywheel-flywheel-pi", "flywheel-flywheel-pi2"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(free_spawn_ordinal("flywheel", "pi", 1, &taken), 3);
        assert_eq!(free_spawn_ordinal("flywheel", "pi", 2, &taken), 3);
        assert_eq!(free_spawn_ordinal("flywheel", "pi", 1, &HashSet::new()), 1);
        assert_eq!(free_spawn_ordinal("flywheel", "oc", 1, &taken), 1);
    }

    #[test]
    fn watch_matches_event_by_pane_id_not_agent_kind() {
        let targets = vec![
            ("w1:p1".to_string(), "flywheel-x-oc".to_string()),
            ("w1:p2".to_string(), "flywheel-x-pi".to_string()),
        ];
        assert_eq!(
            watch_name_for_pane(&targets, "w1:p1"),
            Some("flywheel-x-oc")
        );
        assert_eq!(
            watch_name_for_pane(&targets, "w1:p2"),
            Some("flywheel-x-pi")
        );
        assert_eq!(watch_name_for_pane(&targets, "w9:p9"), None);
        assert_eq!(watch_name_for_pane(&targets, "opencode"), None);
    }

    #[test]
    fn watch_matches_nameless_agents_by_pane() {
        let targets = vec![("w1:p2".to_string(), "w1:p2".to_string())];
        assert_eq!(watch_name_for_pane(&targets, "w1:p2"), Some("w1:p2"));
    }

    /// Pure: an OS username is skipped and the first REGISTERED candidate
    /// wins. `br` writes `created_by` from `$USER`, so the first link is
    /// almost always a name no mailbox knows.
    #[test]
    fn first_routable_skips_the_username_and_takes_the_registered_pin() {
        let routable = |n: &str| n == "BoldKite" || n == "captain";
        let chain = vec![
            "bangedorrunt".to_string(),
            "Flywheel".to_string(),
            "captain".to_string(),
        ];
        assert_eq!(
            first_routable(&chain, |c| routable(c)).as_deref(),
            Some("captain"),
            "the unroutable username and the unset operator fall through to the captain fold"
        );
        assert_eq!(
            first_routable(&chain, |_| false),
            None,
            "nothing routable -> None, never a silent pick"
        );
    }

    /// Pure: empty and whitespace-only candidates are never tried (an
    /// unset `agent_mail` yields "", which would otherwise "resolve" as
    /// the empty recipient).
    #[test]
    fn first_routable_ignores_blank_candidates() {
        let mut tried = Vec::new();
        let chain = vec!["   ".to_string(), String::new(), "BoldKite".to_string()];
        let got = first_routable(&chain, |c| {
            tried.push(c.to_string());
            true
        });
        assert_eq!(got.as_deref(), Some("BoldKite"));
        assert_eq!(
            tried,
            vec!["BoldKite"],
            "blanks are filtered before any whois"
        );
    }

    /// Pure: order is preserved — the chain never prefers a later rung
    /// over an earlier routable one (created_by beats the operator pin).
    #[test]
    fn first_routable_respects_chain_order() {
        let chain = vec!["BoldKite".to_string(), "captain".to_string()];
        assert_eq!(
            first_routable(&chain, |_| true).as_deref(),
            Some("BoldKite"),
            "first routable wins; the captain fold is the last rung, not the first"
        );
    }

    /// Pure: the loop's status word comes from the leaf's own `ok` flag.
    /// `planner mailed` over `ok:false` is how beads were silently never
    /// dispatched with no reason delivered.
    #[test]
    fn not_ready_mail_status_words_the_leaf_ok_flag() {
        use serde_json::json;
        assert_eq!(
            crate::durable::not_ready_mail_status(&json!({"ok": true, "notified": "BoldKite"})),
            "planner mailed"
        );
        assert_eq!(
            crate::durable::not_ready_mail_status(
                &json!({"ok": false, "notified": "", "error": "unknown recipient"})
            ),
            "planner NOT mailed",
            "a failed send is reported as a failure"
        );
        assert_eq!(
            crate::durable::not_ready_mail_status(&json!({"raw": "garbage"})),
            "planner NOT mailed"
        );
    }
}

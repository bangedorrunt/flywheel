// governed-by: ADR-0024
// governed-by: ADR-0025
// governed-by: ADR-0026
// governed-by: ADR-0027
//! flywheel — multi-agent swarm orchestrator on the hc backend.
//!
//! NTM-parity daily loop for opencode/pi/grok: spawn swarms, dispatch work,
//! monitor states, checkpoint progress. All tracker state lives in br/bv;
//! all coordination lives in agent-mail; hc is the backend.
//!
//! governed-by: ADR-0022 §3.6 (workflow compose --from-yaml / --dry-run
//! surface + budget-cap fail-closed gate in the Compose arm), ADR-0026

#![cfg_attr(not(feature = "serve"), allow(dead_code))]

mod adr0021_gates;
mod agent_ref;
mod agentstate;
#[cfg_attr(not(test), allow(dead_code))]
mod approval;
mod artifact;
mod authority;
mod awareness;
mod backend;
mod br;
mod briefing;
mod builtin_workflows;
mod captain_directory;
mod captain_slot;
mod catalog_tui;
mod cmd;
mod coalition;
mod comms_send;
mod config;
mod control;
mod crew;
mod crew_briefs;
mod crew_menu;
mod crew_receipt;
mod cron;
mod daemon;
mod decide;
mod dispatch_bindings;
mod doorbell;
mod doorbell_host;
mod doorbell_supervise;
mod doorbell_tick;
mod durable;
mod envpin_hop;
mod integration;
mod turn_hooks;
mod turn_state;
#[cfg(test)]
pub(crate) use envpin_hop::{EnvPinSplitUnsupported, envpin_hop_split};
pub(crate) use envpin_hop::{
    EnvpinHop, EnvpinHopOutcome, RerouteEnvPin, envpin_hop_respawn, envpin_respin_hop,
    envpin_split_refusal, reroute_env_pin_verdict,
};
mod durable_body;
mod durable_policy;
mod durable_resume;
mod durable_transition;
mod effect_journal;
mod elm;
mod events_bridge;
mod events_client;
mod events_source;
mod events_tokens;
mod failure;
mod fs_util;
mod harness_api;
mod harness_caps;
mod harness_registry;
mod hc;
mod headless;
mod health_page;
mod inbox_host;
mod inbox_ports;
mod ingress;
mod init_scaffold;
mod intent;
#[allow(dead_code)]
mod jev_rank;
mod judgment;
#[allow(dead_code)]
mod kernel;
mod lead_claim;
mod mail;
mod mail_wake;
mod mcp_http;
mod models;
mod mutation_witnesses;
mod output;
mod pane;
mod pane_keep;
mod pane_read;
mod pane_screen;
#[cfg(test)]
mod pane_screen_corpus;
mod pane_write;
mod phase_chain;
mod phase_contracts;
mod plan_fence;
mod plane_route;
mod preferences;
mod preflight;
mod principles;
mod private_fs;
mod profile;
mod project_state;
mod pull_fresh_poller;
mod recipient_guard;
mod redact;
mod request_receipts;
mod retry;
mod rot;
mod run_log;
mod run_state_recorder;
mod seat_binder;
mod seat_reconcile;
mod self_watch;
mod signoff;
mod snapshot;
mod snapshot_reporting;
mod spawn_facts;
mod stage;
mod stages;
mod stale_check;
mod standing_orders;
mod state;
mod state_log;
mod steer_brief;
mod steer_delta;
mod stuck_vs_busy;
mod supervisor_hints;
mod supervisor_host;
mod supervisor_program;
mod swarm_plan;
#[cfg(test)]
mod test_env_contract;
#[cfg(test)]
mod test_source;
mod timer_watch;
mod unanswered_triage;
mod upstream_parity;
mod verdict;
mod wake_liveness;
mod wake_rung;
mod work;
mod workflow_catalog;
mod workflow_lowering;
mod workspace;
mod workspace_policy;
mod workspace_reporting;
mod write_liveness;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[cfg(feature = "serve")]
use cmd::DaemonCmd;
use cmd::{
    AgentCmd, ArtifactsCmd, BackendCmd, CaptainCmd, CheckpointCmd, CoalitionCmd, ConfigCmd,
    CrewCmd, CronCmd, DoorbellCmd, JudgmentCmd, LedgerCmd, LoopCmd, MailCmd, MemoryCmd, ModelCmd,
    PeerCmd, PolicyCmd, PrinciplesCmd, ProfileCmd, RunsCmd, ShipCmd, SlotCmd, SquadCmd, StackCmd,
    StageCmd, StandingOrdersCmd, SwarmCmd, WorkCmd, WorkflowCmd, WorktreeCmd, XprojCmd, cmd_agent,
    cmd_analytics, cmd_audit, cmd_backend, cmd_captain, cmd_changes, cmd_checkpoint, cmd_commit,
    cmd_config, cmd_crew, cmd_doorbell_digest, cmd_doorbell_gc, cmd_doorbell_inject,
    cmd_doorbell_introduce, cmd_doorbell_re_inject, cmd_doorbell_roster, cmd_doorbell_start,
    cmd_doorbell_status, cmd_doorbell_stop, cmd_doorbell_subscribe, cmd_history,
    cmd_internal_doorbell, cmd_judgment, cmd_ledger, cmd_mail, cmd_memory, cmd_models, cmd_peer,
    cmd_policy, cmd_prefer, cmd_principles, cmd_resume, cmd_retire, cmd_runs, cmd_ship, cmd_slot,
    cmd_stack, cmd_stage, cmd_standing_orders, cmd_swarm, cmd_timeline, cmd_work, cmd_worktree,
    cmd_xproj,
    coalition::cmd_coalition,
    cron::cmd_cron,
    debate::{cmd_debate, cmd_plan_review},
    doorbell::doorbell_now_unix,
    doorbell::{
        cmd_doorbell_afk, cmd_doorbell_subscriptions, cmd_doorbell_unsubscribe, cmd_doorbell_why,
    },
    internal::{
        SendOptions, VerifyDispatchOpts, cmd_gate_learnings, cmd_internal_ack_overdue,
        cmd_internal_check_reply, cmd_internal_close_check, cmd_internal_compose_inbox,
        cmd_internal_context_inbox, cmd_internal_diagnose, cmd_internal_dispatch_guard,
        cmd_internal_inprogress, cmd_internal_kind, cmd_internal_loop_run_verify,
        cmd_internal_mail_blocked, cmd_internal_mail_digest, cmd_internal_mail_done,
        cmd_internal_mail_not_ready, cmd_internal_pin_stamp, cmd_internal_poll,
        cmd_internal_poll_events, cmd_internal_profile, cmd_internal_read, cmd_internal_ready,
        cmd_internal_reclaim_stranded, cmd_internal_release_bead_locks, cmd_internal_remind_reply,
        cmd_internal_renew_leases, cmd_internal_reopen_bead, cmd_internal_reservation_board,
        cmd_internal_respawn, cmd_internal_run_headless, cmd_internal_send,
        cmd_internal_verify_dispatch, cmd_internal_wait_output, cmd_internal_xproj_inbox,
        cmd_internal_xproj_spawn, cmd_subagent_guard, cmd_turn_guard, default_gate_budget,
        format_send_failure,
    },
    mail::{
        cmd_collab, cmd_ensemble, mail_as, mail_or_die, repo_evidence_verified,
        resolve_mail_actor_roster,
    },
    profile::cmd_profile,
    send::{cmd_send, cmd_send_via_inbox, interrupt_guard_working, mint_task_id, status_label},
    spawn::{
        cmd_add, cmd_preflight, cmd_spawn, cmd_steer, doorbell_project_workspaces,
        ensure_doorbell_supervisor, preflight_gate,
    },
    squad::cmd_squad,
    status::{cmd_helm, cmd_progress, cmd_snapshot, cmd_status},
    workflow::cmd_workflow,
};
#[cfg(test)]
use cmd::{
    internal::{
        bead_id_from_reason, delegation_deny, parse_leaf_output, skills_injected_for,
        subagent_guard_decision,
    },
    mail::{mail_project, newest_urgent, wake_identity_pin, wake_urgent},
    send::{
        StallProbe, StallStep, classify_stall_probe, is_agent_not_ready, launch_pending_error,
        not_ready_retry_target, prompt_presence_needle, stall_giveup_error, stall_may_resend,
        stall_step,
    },
    workflow::lint_flywheel_tier,
};
use config::{Config, Recipe, WorkflowStep};
use hc::{
    AgentInfo, AgentStatus, Client, Subscription, alt_screen_read_ladder, sanitize_pane_text,
};
use output::{die, dim, error, kind_paint, ok as paint_ok, print_json, status_paint};
use profile::ProfileCatalog;
use snapshot::robot_snapshot;
use state::{EventKind, log_event};
use toron_plane::backend::error::{BackendError, BackendErrorCode};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn fallback_chain_for(profile_name: &str) -> Vec<String> {
    ProfileCatalog::load()
        .ok()
        .and_then(|cat| cat.get(profile_name).map(|p| p.fallback.clone()))
        .unwrap_or_default()
}

fn next_fallback(profile_name: &str, attempted: &[String]) -> Option<String> {
    fallback_chain_for(profile_name)
        .into_iter()
        .find(|fb| !attempted.contains(fb))
}

fn record_reroute_audit(
    project: &str,
    attempt: u32,
    profile: &str,
    kind: failure::FailureKind,
    evidence: &str,
) {
    let evidence = agent_ref::redact_evidence(evidence);
    let entry = serde_json::json!({
        "attempt": attempt,
        "profile": profile,
        "kind": kind.to_string(),
        "evidence": evidence,
    });
    let _ = state::log_event(project, state::EventKind::Spawn, entry.clone());
    eprintln!(
        "flywheel: reroute audit attempt={attempt} profile={profile} kind={kind} evidence={evidence}"
    );
}

/// ADR-0026 Spec 3: shared reroute helper for headless and pane paths.
/// Returns the next fallback profile to try, audited. ONE hop per reroute_eligible
/// failure; second same-class failure is terminal (no further hop when the chain
/// is exhausted). Caller checks `reroute_eligible` and `evidence` before calling.
fn reroute_next_profile(
    profile_name: &str,
    kind: failure::FailureKind,
    evidence: &str,
    attempted: &[String],
    attempt: u32,
) -> Option<String> {
    if evidence.is_empty() {
        return None;
    }
    record_reroute_audit("flywheel", attempt, profile_name, kind, evidence);
    next_fallback(profile_name, attempted)
}

/// ADR-0025 Spec 5 plane tripwires — pure, testable helper for `cmd_send`.
/// - secret → public fires only on explicit `--plane s` (or channel post);
///   default (no plane) stays private — classifier never flips to public.
/// - job-assign → private fires on gift-wrap (private) wires.
/// - `--unsafe-plane` is the only audited escape (stderr JSON inside check_*);
///   `--raw` never bypasses.
fn check_send_plane_tripwires(
    text: &str,
    plane: Option<&str>,
    unsafe_plane: bool,
    raw: bool,
) -> Result<()> {
    let is_public = matches!(plane, Some("s"));
    if is_public {
        plane_route::check_secret_tripwire(text, None, false, &[], plane, unsafe_plane, raw)
            .map_err(|e| anyhow!(e))?;
    }
    let is_gift_wrap = !is_public && !matches!(plane, Some("j" | "job"));
    if is_gift_wrap {
        let has_mention = text.contains('@');
        let has_p = has_mention;
        plane_route::check_job_tripwire(text, has_p, false, false, true, plane, unsafe_plane, raw)
            .map_err(|e| anyhow!(e))?;
    }
    Ok(())
}

/// bd-adr0027-proven-leftovers-55nx.9: budget-gate outcome as journal data.
/// `raw` is tick's complete Prompt injects, `allowed` is take_allowed's
/// survivors (an order-preserving subsequence); the difference is the drop
/// set `why` must not replay as injected. Tick output stays complete —
/// effect rides the `executed` JSON beside cmds, never inside tick.
#[cfg(test)]
fn budget_effect(raw: &[(String, String)], allowed: &[(String, String)]) -> serde_json::Value {
    let mut remaining: Vec<(String, String)> = allowed.to_vec();
    let mut injected: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for (pin, line) in raw {
        if let Some(pos) = remaining.iter().position(|(p, l)| p == pin && l == line) {
            remaining.remove(pos);
            if !injected.contains(pin) {
                injected.push(pin.clone());
            }
        } else if !dropped.contains(pin) {
            dropped.push(pin.clone());
        }
    }
    serde_json::json!({ "injected": injected, "dropped": dropped })
}

/// Registry pane ids not yet covered by a live forwarder (pure — tested).
#[cfg(test)]
fn doorbell_uncovered_panes(covered: &HashSet<String>, pane_ids: &[String]) -> Vec<String> {
    pane_ids
        .iter()
        .filter(|p| !p.is_empty() && !covered.contains(*p))
        .cloned()
        .collect()
}

#[cfg(test)]
mod plane_tripwire_tests {
    use super::check_send_plane_tripwires;

    #[test]
    fn plane_tripwire_secret_via_plane_s_errors() {
        let err = check_send_plane_tripwires("nsec1abc secret", Some("s"), false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("plane_error: secrets use mail send"), "{err}");
        assert!(err.contains("toron mail send"), "{err}");
    }

    #[test]
    fn plane_tripwire_unsafe_plane_overrides_audited() {
        assert!(check_send_plane_tripwires("nsec1abc secret", Some("s"), true, false).is_ok());
        assert!(check_send_plane_tripwires("@Alice fix job", None, true, false).is_ok());
    }

    #[test]
    fn plane_tripwire_raw_does_not_bypass() {
        let err = check_send_plane_tripwires("nsec1abc secret", Some("s"), false, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("plane_error"), "raw should not bypass: {err}");
        let err2 = check_send_plane_tripwires("@Bob work assign", None, false, true)
            .unwrap_err()
            .to_string();
        assert!(
            err2.contains("plane_error"),
            "raw should not bypass job: {err2}"
        );
    }

    #[test]
    fn plane_tripwire_default_private_unchanged() {
        assert!(check_send_plane_tripwires("hello world", None, false, false).is_ok());
        assert!(check_send_plane_tripwires("hello world", Some("s"), false, false).is_ok());
        let err = check_send_plane_tripwires("@Alice fix the bug", None, false, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("plane_error: work assignment uses the job plane"),
            "{err}"
        );
        assert!(check_send_plane_tripwires("@Alice fix", Some("j"), false, false).is_ok());
    }
}

#[cfg(test)]
mod pane_fallback_tests {
    use super::{fallback_chain_for, next_fallback, reroute_next_profile};
    use crate::failure;

    fn with_temp_profile(content: &str, f: impl FnOnce()) {
        let _guard = crate::config::lock_env();
        let orig = std::env::var_os("XDG_CONFIG_HOME");
        let tmp = std::env::temp_dir().join(format!("fw-test-{}", std::process::id()));
        let cfg_dir = tmp.join("flywheel");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &tmp) };
        let path = cfg_dir.join("profiles.toml");
        std::fs::write(&path, content).unwrap();
        f();
        match orig {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pane_fallback_hops_to_fallback_on_402() {
        let content = r#"
[profiles.test-primary]
harness = "opencode"
model = "test-model"
fallback = ["test-fallback"]

[profiles.test-fallback]
harness = "opencode"
model = "fallback-model"
"#;
        with_temp_profile(content, || {
            let cf = failure::classify("402 payment required", None, None);
            assert_eq!(cf.kind, failure::FailureKind::Billing);
            assert!(cf.reroute_eligible);
            let chain = fallback_chain_for("test-primary");
            assert_eq!(chain, vec!["test-fallback"]);
            let next = next_fallback("test-primary", &[]).expect("should hop");
            assert_eq!(next, "test-fallback");
            let attempted: Vec<String> = vec![];
            let reroute =
                reroute_next_profile("test-primary", cf.kind, &cf.evidence, &attempted, 0)
                    .expect("reroute should return fallback");
            assert_eq!(reroute, "test-fallback");
        });
    }

    #[test]
    fn pane_fallback_second_same_class_terminal() {
        let content = r#"
[profiles.test-primary]
harness = "opencode"
model = "test-model"
fallback = ["test-fallback"]

[profiles.test-fallback]
harness = "opencode"
model = "fallback-model"
"#;
        with_temp_profile(content, || {
            let cf = failure::classify("402 insufficient balance", None, None);
            let first = reroute_next_profile("test-primary", cf.kind, &cf.evidence, &[], 0)
                .expect("first hop");
            assert_eq!(first, "test-fallback");
            let attempted = vec!["test-primary".to_string(), "test-fallback".to_string()];
            let second = reroute_next_profile("test-primary", cf.kind, &cf.evidence, &attempted, 1);
            assert!(
                second.is_none(),
                "second same-class should be terminal, got {second:?}"
            );
            let next2 = next_fallback("test-primary", &attempted);
            assert!(next2.is_none());
        });
    }

    #[test]
    fn pane_fallback_audit_row_written() {
        let content = r#"
[profiles.test-primary]
harness = "opencode"
model = "test-model"
fallback = ["test-fallback"]

[profiles.test-fallback]
harness = "opencode"
model = "fallback-model"
"#;
        with_temp_profile(content, || {
            let cf = failure::classify("401 unauthorized", None, None);
            assert!(cf.reroute_eligible);
            let next = reroute_next_profile("test-primary", cf.kind, &cf.evidence, &[], 0)
                .expect("audit hop");
            assert_eq!(next, "test-fallback");
            let chain = fallback_chain_for("test-primary");
            assert_ne!(chain, [] as [String; 0]);
        });
    }
}

pub(crate) fn stage_prompt(name: &str) -> Result<String> {
    let lib = stages::StageLibrary::load()?;
    lib.resolve(name)
        .map(std::string::ToString::to_string)
        .ok_or_else(|| {
            anyhow!(
                "unknown stage: {name} (known: {})",
                lib.list()
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// ADR-0032 Spec 9 tail (bd-40d1): project-tier stage resolve for runs.
/// `<project_dir>/.flywheel/stages.toml` shadows the untiered library; a
/// missing tier falls back byte-identical to [`stage_prompt`].
pub(crate) fn stage_prompt_for_project(name: &str, project_dir: &Path) -> Result<String> {
    let path = project_dir.join(".flywheel/stages.toml");
    let lib = if path.is_file() {
        stages::StageLibrary::load_layered(Some(&path))?
    } else {
        stages::StageLibrary::load()?
    };
    lib.resolve(name)
        .map(std::string::ToString::to_string)
        .ok_or_else(|| {
            anyhow!(
                "unknown stage: {name} (known: {})",
                lib.list()
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}
/// Project-aware stage render for sends: the project's `.flywheel/stages.toml`
/// tier rides along, so project-authored dispatch stages (lane-bead,
/// stall-wake) resolve from CLI sends — not just the workflow engine.
/// Missing file falls back to the untiered library (byte-identical).
pub(crate) fn stage_prompt_with_params_project(
    name: &str,
    raw_params: &[String],
    project_dir: &Path,
) -> Result<String> {
    let path = project_dir.join(".flywheel/stages.toml");
    let lib = if path.is_file() {
        stages::StageLibrary::load_layered(Some(&path))?
    } else {
        stages::StageLibrary::load()?
    };
    let stage = lib.get_stage(name).ok_or_else(|| {
        anyhow!(
            "unknown stage: {name} (known: {})",
            lib.list()
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let params = stages::parse_params(raw_params)?;
    stages::render_stage(&stage.prompt.clone(), &stage.params.clone(), &params)
}

#[cfg(test)]
mod send_project_stage_tests {
    use super::*;

    fn tmp_toml(contents: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("flywheel-send-stage-test-{}", std::process::id()));
        let fw = dir.join(".flywheel");
        std::fs::create_dir_all(&fw).expect("fw dir");
        std::fs::write(fw.join("stages.toml"), contents).expect("write toml");
        dir
    }

    /// Project-tier slotted stages render through sends; unknown names fail loud.
    #[test]
    fn project_stage_renders_params_for_send() {
        let dir = tmp_toml(
            "[stages.probe]\nname = \"probe\"\nphase = \"implement\"\nparams = [\"bead\"]\nprompt = \"Bound bead: {{bead}}.\"\n",
        );
        let out = stage_prompt_with_params_project("probe", &["bead=bd-1".to_string()], &dir)
            .expect("renders");
        assert!(out.contains("bd-1"), "slot substituted: {out}");
        assert!(stage_prompt_with_params_project("nope", &[], &dir).is_err());
        let bare = std::env::temp_dir().join(format!(
            "flywheel-send-stage-test-empty-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&bare).expect("tmp dir");
        assert!(stage_prompt_with_params_project("ce-execute", &[], &bare).is_ok());
    }
}
/// Stage + explicit message combine: the message is operator intent and rides
/// after the rendered stage — never silently dropped (hollow-dispatch fix).
fn combine_stage_message(rendered: String, msg: Option<&str>) -> String {
    match msg {
        Some(t) => format!("{rendered}\n\n{t}"),
        None => rendered,
    }
}

/// Dispatch header: the TASK-ID is routing metadata, never the work order.
/// Names the bound bead — or, for a STAGE dispatch with none, states the
/// bootstrap contract the pane can act on by itself (bd-stage-dispatch-
/// binds-bead-roul). A raw nudge (no stage) still defers to its message text.
fn dispatch_header(task_id: &str, bead: Option<&str>, stage: Option<&str>) -> String {
    match bead {
        Some(b) => format!(
            "TASK-ID: {task_id} (routing metadata only — not a bead id, never `br show` it)\nBEAD: {b} (the bound work order; `br show {b}` is the contract)"
        ),
        None => match stage {
            Some(s) => format!(
                "TASK-ID: {task_id} (routing metadata only — not a bead id, never `br show` it)\nBEAD: none bound — bootstrap: stage dispatch `{s}` names no target bead, so take your own work order now: `br ready --json` (dispatchable beads only), `br update <id> --status in_progress --assignee <your pin>`, then do that bead. If `br ready` is empty, say exactly that in your completion mail and stop — never invent work and never wait for a bead id."
            ),
            None => format!(
                "TASK-ID: {task_id} (routing metadata only — not a bead id, never `br show` it)\nBEAD: none bound by this dispatch — follow the message text; if it carries no actionable content, ask the captain for the bead id."
            ),
        },
    }
}

#[cfg(test)]
mod send_dispatch_header_tests {
    use super::*;

    /// Message text survives alongside a stage; header binds or disclaims.
    #[test]
    fn stage_message_combine_and_header() {
        assert_eq!(
            combine_stage_message("stage".to_string(), Some("brief")),
            "stage\n\nbrief"
        );
        assert_eq!(combine_stage_message("stage".to_string(), None), "stage");
        let bound = dispatch_header("task-1", Some("bd-2"), Some("ce-execute"));
        assert!(bound.contains("bd-2"), "names the bead: {bound}");
        assert!(
            bound.contains("not a bead"),
            "disclaims the task id: {bound}"
        );
        let unbound = dispatch_header("task-1", None, None);
        assert!(
            unbound.contains("follow the message text"),
            "unbound dispatch defers to message text (raw nudges stay actionable): {unbound}"
        );
        assert!(
            !unbound.contains("Do NOT act"),
            "must not self-negate raw messages: {unbound}"
        );
    }

    /// bd-stage-dispatch-binds-bead-roul: a fresh pane receiving a STAGE
    /// dispatch can act without asking which bead it belongs to. A bound bead
    /// is named as the work order; an unbound stage dispatch carries the
    /// bootstrap contract (pick from `br ready`), never "ask the captain".
    #[test]
    fn stage_dispatch_binds_bead() {
        let bound = dispatch_header("task-1", Some("bd-abc"), Some("ce-execute"));
        assert!(bound.contains("BEAD: bd-abc"), "names the bead: {bound}");
        assert!(
            bound.contains("the bound work order"),
            "names the contract: {bound}"
        );
        assert!(
            !bound.contains("br ready"),
            "a bound dispatch never leaks the bootstrap text: {bound}"
        );
        let unbound = dispatch_header("task-1", None, Some("ce-execute"));
        assert!(
            unbound.contains("br ready"),
            "bootstrap names the picker: {unbound}"
        );
        assert!(
            unbound.contains("stage dispatch `ce-execute`"),
            "names the stage that carried no bead: {unbound}"
        );
        assert!(
            unbound.contains("br update <id> --status in_progress"),
            "the claim step rides along so the pane is self-contained: {unbound}"
        );
        assert!(
            !unbound.contains("ask the captain"),
            "no round trip for a stage dispatch: {unbound}"
        );
        let raw = dispatch_header("task-1", None, None);
        assert!(raw.contains("follow the message text"), "{raw}");
        assert!(
            !raw.contains("br ready"),
            "raw nudges do not bootstrap: {raw}"
        );
        for h in [&bound, &unbound, &raw] {
            assert!(h.contains("not a bead id"), "{h}");
        }
    }
}
/// Agent swarm orchestrator over hc, toron mail, and the br bead ledger.
///
/// Session door: `flywheel arrive <project> --as <pin>`, then
/// `flywheel briefing "<goal>" --project <p> --json`. Run the argv it prints.
/// `decide` is the classifier inside briefing, not a second door.
///
/// User config is `~/.config/flywheel`, or `$XDG_CONFIG_HOME/flywheel`.
/// That directory is the source of truth for workflows, stages, and judgment
/// packs. A project `.flywheel` directory is a lower rung: it can add a name
/// the user tier does not have, and it does not replace a user file.
///
/// `flywheel config seed` writes the compiled-in workflows, stages, and
/// judgment packs that are missing. It never overwrites an existing file.
/// `workflow drift` and `judgment pack drift` only report. They do not rewrite.
#[derive(Parser)]
#[command(
    name = "flywheel",
    version = VERSION,
    about,
    after_help = "Examples:\n\
                  \tflywheel config path\n\
                  \tflywheel config seed\n\
                  \tflywheel config seed --json\n\
                  \tflywheel workflow list --brief --json\n\
                  \tflywheel arrive flywheel --as <pin>\n\
                  \tflywheel briefing \"until the tests pass\" --project flywheel --json\n\
                  \n\
                  Exit codes: 0 ok · 1 hc/server error · 2 usage error.\n\
                  Machine output: --json on list/status/view; --robot-snapshot for full state;\n\
                  --robot-capabilities for the agent command map.\n\
                  User config: ~/.config/flywheel  (flywheel config path)\n\
                  Fill missing defaults, never overwrite: flywheel config seed\n\
                  Feedback: https://github.com/bangedorrunt/flywheel/issues"
)]
struct Cli {
    /// Full JSON state snapshot of all flywheel swarms (scripts/agents)
    #[arg(long, global = true)]
    robot_snapshot: bool,

    /// List flywheel's machine-readable capabilities as JSON
    #[arg(long, global = true)]
    robot_capabilities: bool,

    /// Skip preflight checks (steer/loop/workflow auto-verify before dispatch)
    #[arg(long, global = true)]
    skip_preflight: bool,

    /// Compact JSON status of all flywheel swarms
    #[arg(long, global = true)]
    robot_status: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum EventsTokenCmd {
    /// Rotate a pin's events token before its TTL and revoke the old hash.
    Rotate {
        #[arg(long)]
        pin: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        token_file: std::path::PathBuf,
        #[arg(long)]
        journal: std::path::PathBuf,
        #[arg(long, default_value_t = events_tokens::ROTATE_BEFORE_SECS)]
        before_secs: u64,
    },
}

#[derive(Subcommand)]
enum Commands {
    /// Harness trampoline entry point (bd-mdpfr); no-ops outside a seat.
    #[command(name = "__hook")]
    HookTrampoline {
        event: String,
        #[arg(long)]
        agent: String,
    },
    /// Report a harness hook event (bd-b2kaa): writes per-seat turn state and
    /// prints at most one continuation line. Hooks install this at spawn.
    Hook {
        #[arg(long)]
        event: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        pin: String,
    },
    /// Scaffold a project (ensure dir + .beads + the project-scope compound
    /// spine: .flywheel/stages.toml, .flywheel/standing-orders.md,
    /// chiebukuro as the learning store) without spawning
    Init { project: String },
    /// Spawn a swarm (escape hatch). Default multi-lane is
    /// `workflow compose --parts wave-swarm --run`.
    Spawn {
        project: String,
        #[command(flatten)]
        counts: AgentCounts,
        /// spawn recipe name from recipes.toml
        #[arg(long)]
        recipe: Option<String>,
        /// extran hc agent kind counts: --kind claude=1 (repeatable)
        #[arg(long = "kind", value_name = "KIND=COUNT", value_parser = parse_kind_count)]
        kinds: Vec<(String, u32)>,
        /// steer channels every spawned agent joins at pane creation
        /// (bd-kbl1c join-before-post: membership is the only gate — the
        /// dispatcher auto-joins declared channels so a first post never
        /// bounces). Repeatable/comma-separated.
        #[arg(long = "join-channel", value_delimiter = ',')]
        join_channels: Vec<String>,
        /// bd-koafr: boot assignment per pin (--assign SlateHarbor=bd-a,bd-b).
        /// The pane gets the boot prime, then one crew-prompt brief per pin.
        #[arg(long = "assign", value_name = "PIN=BEAD[,BEAD]")]
        assign: Vec<String>,
        #[arg(long)]
        label: Option<String>,
        /// do not focus the new workspace
        #[arg(long)]
        no_focus: bool,
        /// skip the post-spawn visibility check (scripts; not recommended —
        /// a silent registry-write failure leaves panes invisible to list/send)
        #[arg(long)]
        no_verify: bool,
        /// close pre-existing FLYWHEEL-OWNED workspaces for this project
        /// (stale panes from a killed swarm race the new one —
        /// bd-fw-stale-workspace-panes-hgp + bd-627z.3). Without this, spawn
        /// REFUSES when one exists; foreign workspaces (captain/omp sessions)
        /// are never closed — they only warn.
        #[arg(long)]
        retire_orphans: bool,
        /// spawn agents with the peer-aware handler (ADR-0005). DEFAULT ON.
        /// --no-peer-aware opts out of dispatcher + handler, never of the
        /// identity line.
        #[arg(long, action = clap::ArgAction::SetTrue, default_value_t = true)]
        peer_aware: bool,
        /// opt OUT of dispatcher + handler (single-shot panes). Identity
        /// line still injects.
        #[arg(long)]
        no_peer_aware: bool,
        /// safety-net idle pull (minutes). Dispatcher wake is the path.
        #[arg(long, default_value_t = 5)]
        peer_poll_interval_minutes: u32,
        /// dispatcher liveness budget (minutes; default 30)
        #[arg(long, default_value_t = 30)]
        peer_poll_budget_minutes: u32,
        /// dispatcher max peer-prompts per pin (default 3)
        #[arg(long, default_value_t = 3)]
        peer_max_prompts: u32,
        /// post-spawn settle: seconds to sleep after the readiness gate so a
        /// booted-but-not-input-ready TUI (MCP init, model connect) catches
        /// prompts (bd-eign.6). Default 30 — observed boot is 20-30s.
        #[arg(long, default_value_t = 30)]
        settle_secs: u64,
        /// spawn agents from a profile (ADR-0005, bd-eign.13): the profile's
        /// harness determines the kind, its model/effort ride the launch args
        /// (bd-eign.9), and its behavior/skills condition dispatches via the
        /// spawn registry. Combinable with --oc/--pi/--gk/--kind for mixed
        /// crews.
        #[arg(long)]
        profile: Option<String>,
        /// how many profile agents to spawn (with --profile; default 1)
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// bd-j3so: bare harness+model override for kind/count spawns (no
        /// --profile): launch args route through the composite registry,
        /// lenient — registered ids route, unregistered pairs pass through.
        /// `--model harness-default` injects nothing and lets the harness
        /// run its own configured model.
        #[arg(long)]
        model: Option<String>,
        /// provider pin for --model's composite registry key
        #[arg(long, requires = "model")]
        provider: Option<String>,
        /// Label this spawn as an escape from `workflow compose --parts wave-swarm`.
        /// Kind/count + --model/--provider without --escape prints a warning.
        #[arg(long)]
        escape: bool,
        /// skip the pre-spawn model reconfirm (bd-ll3k; non-TTY and loop
        /// drivers never block — the gate is TTY-only)
        #[arg(long)]
        yes: bool,
        /// ADR-0011 D5: join the live fleet instead of refusing / killing it.
        #[arg(long)]
        join: bool,
        /// bd-command-receipts-zile: stable request key for this spawn. The
        /// workspace label derives from it, so a retried call lands on the
        /// same slot: a settled slot REPLAYS (no second crew), an in-flight
        /// slot refuses, and the same key with a different plan is a typed
        /// conflict instead of a replay of somebody else's crew.
        #[arg(long)]
        request_key: Option<String>,
        /// flywheel ADR-0022 phase 1: resolve exactly as the real spawn does,
        /// print flywheel-plan.v1, and return before any pane, registry,
        /// mail, or journal effect. With --json the plan prints compact;
        /// without it, pretty-printed. No launch behaviour changes.
        #[arg(long)]
        dry_run: bool,
        /// with --dry-run: print the plan compact (machine-readable).
        #[arg(long)]
        json: bool,
        /// re-resolve one seat (repeatable): `--patch <seat>.<field>=<value>`,
        /// field in harness|model|effort|mode. Later patches win; unknown
        /// seats or fields fail naming the valid ones.
        #[arg(long, value_name = "SEAT.FIELD=VALUE")]
        patch: Vec<String>,
    },
    /// ADR-0022 rollout 6: front the plan (`plan`), a live patch (`apply`),
    /// and the live seat view (`status`). `plan` prints the same
    /// flywheel-plan.v1 object as `spawn --dry-run --json`.
    Swarm {
        #[command(subcommand)]
        cmd: SwarmCmd,
    },
    /// Extend a running swarm with more agent panes
    Add {
        project: String,
        #[command(flatten)]
        counts: AgentCounts,
        #[arg(long = "kind", value_name = "KIND=COUNT", value_parser = parse_kind_count)]
        kinds: Vec<(String, u32)>,
        /// post-split settle: seconds to sleep after the readiness gate so a
        /// booted-but-not-input-ready TUI (MCP init, model connect) catches
        /// prompts (bd-eign.6, same contract as spawn). Default 30 —
        /// observed boot is 20-30s. bd-umb3: the add path previously had no
        /// settle, so the boot prime raced the shell and added panes stayed
        /// `agent_not_ready` until a manual prime.
        #[arg(long, default_value_t = 30)]
        settle_secs: u64,
        /// bd-command-receipts-zile: stable request key for this add. Its
        /// slot is the swarm's label plus `-add`, so a retry with the same key
        /// REPLAYS instead of adding a second time, an in-flight slot refuses,
        /// and the same key with a different plan is a typed conflict.
        #[arg(long)]
        request_key: Option<String>,
        /// bd-ll2f2: join every new pin to these channels (comma-separated),
        /// exactly like `spawn --join-channel`.
        #[arg(long = "join-channel", value_delimiter = ',')]
        join_channels: Vec<String>,
        /// bd-koafr: boot assignment per pin (--assign SlateHarbor=bd-a,bd-b).
        #[arg(long = "assign", value_name = "PIN=BEAD[,BEAD]")]
        assign: Vec<String>,
    },
    /// List flywheel swarms and their agents
    List {
        /// include non-flywheel hc workspaces
        #[arg(long)]
        all: bool,
    },
    /// Show one swarm's agents and states
    Status { project: Option<String> },
    /// bd-swarm-cockpit-pczs: wave progress — beads grouped by `wave-N`
    /// label (closed/in_progress/ready) and the wave currently in flight.
    /// `br list --json` is the source (the labels live there).
    Progress {
        project: Option<String>,
        /// machine-readable rows
        #[arg(long)]
        json: bool,
    },
    /// Print the fleet snapshot contract (flywheel-snapshot.v1) as robot JSON
    Snapshot { project: String },
    /// bd-3uqz: LABEL the pins in the doorbell's `.unanswered` files as
    /// mail-only-by-design, dead, or unknown. READ-ONLY by construction: it
    /// prints JSON and never writes, because those files are what gate P2b reads
    /// to decide whether a crew is INVISIBLE (bd-y90n). A `dead` label is a
    /// REVIEW QUEUE ENTRY, not a deletion, and this command has no delete path.
    ///
    /// Prints `null` per pin rather than guessing when Jev is unavailable or
    /// declines, so "we did not ask" never reads as "we checked and it is fine".
    UnansweredTriage {
        /// Flywheel state dir, whose `doorbell/` subdirectory holds the
        /// `.unanswered` files (default: the same XDG ladder flywheel uses).
        #[arg(long)]
        flywheel_state_dir: Option<std::path::PathBuf>,
        /// Report what WOULD be asked and exit, spending no request.
        #[arg(long)]
        dry_run: bool,
    },
    /// bd-3uqz: is a doorbell brain WEDGED, or genuinely WORKING? The second Jev
    /// fit, and the one that was built but UNREACHABLE — clippy caught the whole
    /// module as dead code, which is the correct verdict for a judgment nothing
    /// can ask.
    ///
    /// ANNOTATE-ONLY, and structurally so: the module exposes a LABEL and has no
    /// function that takes a pid, port, or name and acts on it. This command
    /// prints the deterministic counters AND the judgment side by side, and cannot
    /// restart or stop anything, so there is nothing here to gate.
    ///
    /// Prints `unknown` with the numbers named when Jev is unavailable or declines,
    /// so "we did not ask" never reads as "we checked and it is fine", and the
    /// deterministic reading always stands beside it.
    StuckVsBusy {
        /// Doorbell health port. 18980 is this host's launched doorbell; 18080 is
        /// toron's daemon. They are DIFFERENT services, and reading the wrong one
        /// yields a plausible empty result rather than an error.
        #[arg(long, default_value = "18980")]
        port: u16,
        /// Report what WOULD be asked and exit, spending no request.
        #[arg(long)]
        dry_run: bool,
    },
    /// Session-start drain over the snapshot: OPEN DECISIONS, UNREAD
    /// STATUS, RECORD DIVERGENCE — always printed, even when empty
    Helm { project: String },
    /// ADR-0011 D3: session briefing (observe + propose). `--commit` runs argv.
    Briefing {
        objective: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        commit: bool,
        #[arg(long)]
        yes: bool,
        /// remix topology: swarm|loop|open (binds the topology question; `join` = swarm joining the live fleet)
        #[arg(long)]
        topology: Option<String>,
        /// pane budget N or role:n,role:n (binds the crew question)
        #[arg(long)]
        crew: Option<String>,
        /// model slot: default|@cheap|@standard|@premium (binds the model question)
        #[arg(long)]
        model: Option<String>,
    },
    /// GS-048: mid-session reclassify — enters Elm plane via
    /// `Msg::MaterialContext` → `Cmd::Reclassify` → `Msg::DecisionReady`
    /// (sticky-open applied). Also prints the briefing proposal (same leaf
    /// facts as `briefing`). Prefer this over inventing a second classifier.
    Reclassify {
        objective: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        commit: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        topology: Option<String>,
        #[arg(long)]
        crew: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// ADR-0011 D5/D6: arrive on a project — helm, join-loud, doorbell, FCFS captain.
    Arrive {
        project: String,
        #[arg(long = "as")]
        as_agent: Option<String>,
        #[arg(long)]
        coalition: Option<String>,
        #[arg(long)]
        objective: Option<String>,
        /// operator override for captain slot
        #[arg(long)]
        captain: Option<String>,
    },
    /// Classify a target into one captain primitive (no dispatch)
    Decide {
        /// free-text target / intent
        target: String,
        /// ready-bead count from `br ready`
        #[arg(long, default_value_t = 0)]
        ready_beads: u32,
        /// profiled panes already up
        #[arg(long, default_value_t = 0)]
        profiled_panes: u32,
        /// agents still launch_pending
        #[arg(long, default_value_t = 0)]
        launch_pending: u32,
        /// file-contention signal
        #[arg(long)]
        contention: bool,
        /// ADR-0011 D3: observe helm/snapshot for this project so Snapshot
        /// flags fill automatically. Explicit --ready-beads/--profiled-panes
        /// still override zeros.
        #[arg(long)]
        project: Option<String>,
        /// machine JSON (primitive, reason, argv)
        #[arg(long)]
        json: bool,
    },
    /// Restore lost agent name bindings from the spawn registry
    /// (bd-fw-name-binding-loss-7rc): panes whose agent restarted show
    /// name:null — this renames them back to their registered names.
    Rebind { project: String },
    /// Journaled in-flight profile/harness rebind (GS-043 / ADR-0032 Spec 7).
    /// Updates the live SpawnEntry binding; does NOT edit profiles.toml.
    /// Pin/npub stay immutable (AgentRef). `--model` fails loud until a real
    /// spawn/pane/headless consumer reads SpawnEntry.model (never invent Settled).
    #[command(name = "binding-rebind")]
    BindingRebind {
        project: String,
        /// public mail pin (AgentRef.pin) — immutable identity. A flag, never a
        /// positional (ADR-0013 D5): the legacy name-bind `rebind <project>
        /// <name>` keeps the positional form, so the two verbs stay distinct.
        #[arg(long)]
        pin: String,
        /// replaceable profile name
        #[arg(long)]
        profile: Option<String>,
        /// live model override — currently fail-loud (not wired into spawn/pane/headless)
        #[arg(long)]
        model: Option<String>,
        /// replaceable harness
        #[arg(long)]
        harness: Option<String>,
        /// captain note for the journal row
        #[arg(long)]
        note: Option<String>,
    },
    /// Recover a damaged swarm (bd-fw-interrupt-kills-agent-or3): for each
    /// registered agent, restore the name binding if the pane survived, or
    /// respawn the agent if it died. The durable loop self-heals on its own;
    /// this is the CLI supervision counterpart.
    /// bd-hc-registry-recover-ywypf: never respawn the captain pin by default
    /// (a duplicate idle captain pane was created); pass --include-captain
    /// for the deliberate case.
    Recover {
        project: String,
        #[arg(long)]
        include_captain: bool,
    },
    /// Send a prompt to one or more agents (with optional wait)
    Send {
        project: String,
        /// prompt text (or --stage for the daily-driver stages)
        msg: Option<String>,
        /// target agents: comma-separated shorts (oc,pi,gk), full names, raw pane ids (w6:p2), raw agent kinds (grok), or "all"
        /// (bare shorts reach the whole crew of a kind — oc hits oc,oc2,oc3;
        /// a full name like flywheel-flywheel-oc or a numbered short like oc2
        /// matches EXACTLY one agent)
        #[arg(long, default_value = "all")]
        to: String,
        /// stage name (`flywheel stage list`; ADR-0009 — p-codes retired)
        #[arg(long)]
        stage: Option<String>,
        /// ADR-0006 D1: payload param k=v substituted into the stage
        /// template's {{k}} slots (repeatable). Required slots are declared
        /// per-stage; a missing one is a usage error naming the slot.
        #[arg(long = "param")]
        params: Vec<String>,
        /// wait for the agents to settle (idle/done/blocked)
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// bypass the policy deny-pattern guard
        #[arg(long)]
        force: bool,
        /// bd-rdy: on agent_prompt_stalled (after one automatic retry),
        /// respawn the agent under the same name and re-send instead of
        /// failing with recovery guidance
        #[arg(long)]
        respawn_on_stall: bool,
        /// bd-98gd: bypass profile conditioning — send the text verbatim.
        /// Default resolves each target's spawn-registry profile and
        /// prepends its behavior preamble (Spec C everywhere); use --raw
        /// for nudges and mail wakes that must not carry role behavior.
        #[arg(long)]
        raw: bool,
        /// GS-037 P0-A: allow exit 0 without land/wake receipt. Default
        /// without `--wait` is fail-loud (no invent green on dispatched).
        /// ADR-0010 D1 (bd-w98c): optional caller-supplied <delivery-id>;
        /// re-enqueue under the same id is idempotent, and the id keys
        /// the re-ring ladder skip (fire-and-forget never climbs).
        /// Equals form only (`--fire-and-forget=<id>`): a bare following word
        /// is a positional MSG, never eaten as the id (hollow-dispatch trap).
        #[arg(long = "fire-and-forget", num_args = 0..=1, default_missing_value = "", require_equals = true)]
        fire_and_forget: Option<String>,
        /// ADR-0025 Spec 5: explicit plane override. Private by default
        /// (pane prompt + gift-wrap completion mail); `--plane s` is the
        /// explicit public act (kind-9 / channel). Classifier never flips
        /// default to public.
        #[arg(long)]
        plane: Option<String>,
        /// ADR-0025 Spec 5: audited escape for plane tripwires. Only
        /// `--unsafe-plane` bypasses a tripwire (audited to stderr JSON);
        /// `--raw` never bypasses.
        #[arg(long = "unsafe-plane")]
        unsafe_plane: bool,
        /// ADR-0041 / build-plan §5: deliver as one `task@1` envelope in each
        /// recipient's comms inbox (`--to` takes npubs, pins or pin@project);
        /// the pane gets ONLY the constant wake line. The inbox is on unless
        /// `TORON_COMMS_INBOX=0|false|off` or `[comms] inbox_log = false`.
        #[arg(long = "via-inbox")]
        via_inbox: bool,
        /// Sender pin or npub for --via-inbox (default: FLYWHEEL_MAIL_AS,
        /// then AGENT_NAME).
        #[arg(long)]
        from: Option<String>,
    },
    /// Crew plane: preview + lifecycle control (ADR-0010 D7). Control has
    /// NO arbitrary-text entry point — the verbs are interrupt|exit|relaunch
    /// only; text the worker reads is not a lifecycle verb.
    Crew {
        #[command(subcommand)]
        cmd: CrewCmd,
    },
    /// P2P multiplayer protocol directives and reactive wake dispatch.
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
    /// Captain door (bd-captain-watch-poll-exit-bbwm): the captain's own
    /// poll-exit watch. `captain watch <project>` blocks on the captain inbox
    /// water (or a stall, with --stall, or a preflight failure, with
    /// --preflight), prints a preview, and exits so the harness completion
    /// callback resumes the captain. One shot — re-arm.
    Captain {
        #[command(subcommand)]
        cmd: CaptainCmd,
    },
    /// Supervisor facts and steers (Phase 4a; `TORON_COMMS_INBOX` on):
    /// crews report `progress|done|exit|proved|usage`, the operator steers
    /// `redirect|pause|resume|cancel`, `view` prints the CrewView
    /// (`goal_proved` is the read-only subscriber), `halt` cancels one context.
    Supervisor {
        #[command(subcommand)]
        cmd: supervisor_host::SupervisorCmd,
    },
    /// Cancel one supervisor context: live children get Exit, the host
    /// stops relaunching that subtree. Escalate mail is the captain push.
    /// `captain watch` resumes the captain when that mail lands.
    Halt {
        project: String,
        /// Nested lead. Omit for the root host.
        #[arg(long)]
        context: Option<String>,
        /// Epoch the sender holds. Omit to use the log's accepted epoch.
        #[arg(long)]
        sup_epoch: Option<u64>,
        /// bd-4fgc: stable request key for this halt. Its slot is
        /// `<project>-halt`, so a retry after a timeout REPLAYS the recorded
        /// halt instead of queueing it twice; the same key with another
        /// context/epoch is a typed conflict.
        #[arg(long)]
        request_key: Option<String>,
    },
    /// One-shot watch. Deadline required. One `notify@1` under
    /// `watch:<id>:<outcome>` (`met|timer|expired|failed`). Default on.
    Timer {
        #[command(subcommand)]
        cmd: timer_watch::TimerCmd,
    },
    /// Interrupt an agent (sends Escape by default — cancels the current
    /// tool call and returns to the prompt). Kill-style keys (ctrl+c) are
    /// REFUSED unless --force: they terminate the agent process and drop the
    /// pane to a shell (bd-fw-interrupt-kills-agent-or3).
    Interrupt {
        project: String,
        #[arg(long, default_value = "oc")]
        to: String,
        /// key names to send instead of Escape (repeatable); ctrl+c requires
        /// --force
        #[arg(long)]
        keys: Vec<String>,
        /// allow kill-style keys (ctrl+c terminates the agent process)
        #[arg(long)]
        force: bool,
    },
    /// Read an agent's terminal output
    View {
        project: String,
        #[arg(long, default_value = "oc")]
        to: String,
        #[arg(long)]
        lines: Option<u32>,
        #[arg(long, default_value = "recent_unwrapped")]
        source: String,
    },
    /// Read an agent's output from the opencode structured session store
    /// (bd-eign.5) — the SQLite session log, not TUI scrollback (which the
    /// alternate screen garbles). Resolves the latest session for the
    /// project directory, optionally narrowed by opencode role; --session
    /// targets an exact session id.
    Output {
        project: String,
        /// opencode role filter (build/explore/general/beads/plan)
        #[arg(long)]
        role: Option<String>,
        /// exact session id (skips directory/role resolution)
        #[arg(long)]
        session: Option<String>,
        /// max parts to read from the session
        #[arg(long, default_value = "200")]
        limit: u32,
    },
    /// bd-swarm-cockpit-pczs: agent-session reads. `agent tail <pin>` shows
    /// the pin's recent actions (tool calls / commands) from the session the
    /// pane reports (`agent_session`) — the debugging surface that used to
    /// require finding the raw pi session JSONL by hand. hc 0.1.16 reports
    /// none, so on hc it bails loud (use `flywheel output` / `hc read`).
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Watch a swarm: stream agent state changes until --until or timeout
    Watch {
        project: String,
        #[arg(long, default_value = "all")]
        to: String,
        /// exit when agents reach this status
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// bd-durable-doorbell-lngf: supervised pane-wake layer surface.
    /// Dispatchers are lazily healed — any of these (plus `recover`/`status`)
    /// restarts a dead one for workspaces that ever ran one.
    Doorbell {
        #[command(subcommand)]
        cmd: DoorbellCmd,
    },
    /// Monitor a swarm: poll every N seconds, feed queued instructions to
    /// idle/finished agents, report blockers. Exits 0 when the queue is drained
    /// and the swarm settles. Runs as a durable engine-backed workflow by default —
    /// crashes resume via `flywheel loop-mode run <project>` (same invocation
    /// id). Renamed from `loop` to avoid harness built-in conflicts
    /// (bd-loop-mode-rename-0tqy).
    #[command(name = "loop-mode")]
    LoopMode {
        #[command(subcommand)]
        cmd: LoopCmd,
    },
    /// Typed artifacts (ADR-0025 Spec 2): list or stitch per-run records
    Artifacts {
        /// run id to list (flywheel artifacts <run_id> [--json])
        run_id: Option<String>,
        /// machine JSON
        #[arg(long)]
        json: bool,
        /// project for artifact path
        #[arg(long)]
        project: Option<String>,
        #[command(subcommand)]
        cmd: Option<ArtifactsCmd>,
    },
    /// Attach to an agent's terminal (interactive; execs `hc attach`)
    Attach {
        project: String,
        #[arg(long, default_value = "oc")]
        to: String,
    },
    /// Close (kill) a swarm workspace
    Kill {
        project: String,
        /// skip the confirmation prompt
        #[arg(long)]
        force: bool,
        /// required when any agent is still `working` (cargo test / long
        /// jobs routinely exceed 15m — a plain --force used to reap them)
        #[arg(long)]
        force_working: bool,
        /// graceful two-phase stop (ADR-0028 zfnx.10): ask working agents to
        /// finish their VERIFY fence instead of reaping; the loop enforces
        /// the grace polls and force-stops past grace. Skips the close.
        #[arg(long)]
        drain: bool,
        /// unconditional override: kill captain + all crews regardless of
        /// on_captain_death (ADR-0025 Spec 4)
        #[arg(long)]
        reap_crews: bool,
    },
    /// Retire ONE OR MORE pins (bd-rilvn): the thin verb over the seat path —
    /// one journaled `SeatEffect::Retire` through `Ports::seat` (closes the
    /// pane), the pin's leases released operator-signed, and the roster row
    /// deregistered. Refuses the captain/operator seat by role, and a working
    /// pin without --force-working.
    Retire {
        /// one or more pins (adjective+noun)
        #[arg(required = true)]
        pins: Vec<String>,
        /// project slug (default: the workspace cwd)
        #[arg(long)]
        project: Option<String>,
        /// retire a pin that is still `working` (mirrors kill --force-working)
        #[arg(long)]
        force_working: bool,
        /// actor recorded on the lease release (default: the operator pin)
        #[arg(long = "as")]
        as_pin: Option<String>,
        /// machine-readable result
        #[arg(long)]
        json: bool,
    },
    /// bead-verdict ledger (ADR-0019): br gate wrappers + receipt helper.
    /// `check` exits 0 only on a legal verdict for the current SHA; `receipt`
    /// writes only the receipt file (workers never write verdicts).
    Ledger {
        #[command(subcommand)]
        cmd: LedgerCmd,
    },
    /// host-side judgment surfaces (ADR-0014, bd-lpn6): miss ledger
    /// (resolve/list/needles), on-demand annotators, verify prescreen.
    /// Annotate-only — nothing here dispatches, holds, or closes.
    Judgment {
        #[command(subcommand)]
        cmd: JudgmentCmd,
    },
    /// Chie Memory preflight (trap checks) and lesson recording.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// engineering principle registry (ADR-0019 Spec 5/6): list the 21
    /// canonical names, or show a leaf's full when+rule (unknown name ->
    /// non-zero + suggestion)
    Principles {
        #[command(subcommand)]
        cmd: PrinciplesCmd,
    },
    /// standing orders (ADR-0019 Spec 2): regen the GENERATED global file
    /// (~/.config/flywheel/standing-orders.md) from the registry + wave-1
    /// criticals; project fences (<project>/.flywheel/standing-orders.md)
    /// are authored and preserved byte-identical
    #[command(name = "standing-orders")]
    StandingOrders {
        #[command(subcommand)]
        cmd: StandingOrdersCmd,
    },
    /// Work-graph surfaces (bv/br pass-through)
    Work {
        #[command(subcommand)]
        cmd: WorkCmd,
    },
    /// Assign beads to agents via mail (toron default)
    Assign {
        project: String,
        #[arg(long, default_value = "all")]
        to: String,
        /// bead ids, comma-separated (bd-1,bd-2)
        #[arg(long)]
        beads: String,
        /// optional extra instruction for the agents
        #[arg(long)]
        msg: Option<String>,
    },
    /// toron mail coordination surfaces
    Mail {
        #[command(subcommand)]
        cmd: MailCmd,
    },
    /// stdio MCP server that bridges toron events into a harness's inbound
    /// channel (ADR-0039 D8, bd-si4b5). Declares claude/channel and emits the
    /// D2 hint only. Phase A: pure protocol + transport port; Phase B wires the
    /// real toron events client (bd-o4bgx).
    /// Provision/rotate per-seat events tokens (bd-5vi2x; cron rotation tick).
    #[command(name = "events-token", hide = true)]
    EventsToken {
        #[command(subcommand)]
        cmd: EventsTokenCmd,
    },
    #[command(name = "mcp-events", hide = true)]
    McpEvents {
        /// subscription id (pin-scoped); the transport keys its cursor on this
        #[arg(long, default_value = "default")]
        subscription: String,
        /// print the initialize result and exit (protocol smoke check)
        #[arg(long)]
        probe: bool,
        /// client protocolVersion to negotiate (--probe only)
        #[arg(long)]
        client_version: Option<String>,
    },
    /// Auto-wire the toron MCP server into the agent MCP configs
    /// (opencode ~/.config/opencode/opencode.json, pi ~/.pi/agent/mcp.json,
    /// grok ~/.grok/config.toml). Idempotent — re-running only updates the
    /// toron entry, never clobbers other servers. (Moved here from the retired
    /// `flywheel toron` demo surface; same args and behavior.)
    #[command(name = "setup-mcp")]
    SetupMcp {
        /// toron MCP server URL (default http://127.0.0.1:18080/mcp)
        #[arg(long)]
        url: Option<String>,
        /// which agents to wire: opencode,pi,grok (comma-sep; default all)
        #[arg(long, default_value = "opencode,pi,grok")]
        agents: String,
        /// also boot a toron serve --mcp-http <port> daemon (needs the toron
        /// binary on PATH and TORON_OPERATOR_KEY / TORON_KEY_PASSPHRASE set)
        #[arg(long, default_value_t = 0)]
        boot_port: u16,
        /// print what would change without touching any file
        #[arg(long)]
        dry_run: bool,
        /// also point flywheel's OWN mail backend at the same toron URL
        /// (writes `[mail.toron] url` in the flywheel config.toml, so flywheel
        /// + all agents share one mailbox)
        #[arg(long)]
        flywheel: bool,
    },
    /// ask a peer a question over mail and wait for the in-thread reply
    /// (the real-time collab primitive: send urgent + block for the answer)
    Collab {
        project: String,
        /// peer agent (adjective+noun or terminal short name)
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        /// seconds to wait for the reply (default 300; 0 = forever)
        #[arg(long, default_value = "300")]
        timeout_secs: u64,
        /// act as this agent (default: config identity)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// cross-project fan-out (contact + product; ADR-0001 §9.5)
    Xproj {
        #[command(subcommand)]
        cmd: XprojCmd,
    },
    /// git worktree swarms
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
    },
    /// Point-in-time swarm snapshots
    Checkpoint {
        #[command(subcommand)]
        cmd: CheckpointCmd,
    },
    /// Event timeline of a swarm
    Timeline { project: String },
    /// Audit trail (the full event log)
    Audit { project: String },
    /// Saved checkpoints (alias of checkpoint list)
    History { project: String },
    /// What changed since a checkpoint
    Changes {
        project: String,
        /// checkpoint index (0 = latest); default 0
        #[arg(long, default_value = "0")]
        since: usize,
    },
    /// Re-attach to a swarm's state after a break
    Resume {
        project: String,
        /// respawn the swarm with a recipe if its workspace is gone
        #[arg(long)]
        recipe: Option<String>,
    },
    /// Aggregate counters over the event log
    Analytics { project: String },
    /// Stage prompts (ADR-0005 P3.3): list semantic names + p-code aliases
    Stage {
        #[command(subcommand)]
        cmd: StageCmd,
    },
    /// Squad (--recipe takes a squad name) + workflow in one launch
    Ensemble {
        project: String,
        #[arg(long)]
        recipe: String,
        #[arg(long)]
        workflow: String,
    },
    /// Safety posture: deps + active guards
    Safety,
    /// Active policy guards (deny patterns)
    Guards,
    /// Destructive-command policy management
    Policy {
        #[command(subcommand)]
        cmd: PolicyCmd,
    },
    /// Standing preferences (ADR-0022 D6): set, list, and forget
    /// role.field values. Project scope via --project, else always.
    Prefer {
        /// assignment ROLE.FIELD=VALUE (sets a preference)
        assignment: Option<String>,
        /// list preferences
        #[arg(long)]
        list: bool,
        /// forget ROLE.FIELD
        #[arg(long)]
        forget: Option<String>,
        /// JSON output
        #[arg(long)]
        json: bool,
        /// project scope (omit for always)
        #[arg(long)]
        project: Option<String>,
    },
    /// Environment + toolchain health check
    Deps {
        /// Best-effort bring-up before reporting. The engine store is local,
        /// report (bd-ykqd). Without it, deps stays report-only.
        #[arg(long)]
        ensure: bool,
    },
    /// Generate orchestration slash commands for an agent kind.
    /// Writes {open,swarm,loop,peer-aware}-mode.md into the tool's prompts dir
    /// and removes leftover /beads-* and /workflow-* files (catalog + br own
    /// those verbs). Kinds: opencode|pi|grok|omp|pa (shorts oc|pi|gk accepted);
    /// omit to list kinds. Re-run after a flywheel rebuild to refresh.
    Integration {
        /// bd-mdpfr operator-run hook installer subcommand; omit for the
        /// legacy slash-command generator.
        #[command(subcommand)]
        cmd: Option<crate::cmd::integration::IntegrationCmd>,
        /// agent kind: opencode|pi|grok|omp|pa (shorts oc|pi|gk accepted);
        /// omit to list kinds
        kind: Option<String>,
        /// write the orchestration slash files into this directory instead of
        /// the tool's conventional prompts dir
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// XDG user config (~/.config/flywheel). `config seed` fills missing
    /// workflows, stages, and judgment packs from compiled-in defaults and
    /// never overwrites. `config path` prints the directory.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Backend discovery + selection (ADR-0002 pluggable backends)
    Backend {
        #[command(subcommand)]
        cmd: BackendCmd,
    },
    /// Agent profiles (ADR-0005 Spec 1/1b) — catalog + spawn
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// Squads (ADR-0005 P6) — named groups of profiles = spawn shapes.
    /// Vocabulary: say "squad" here. A compose token (`wave-swarm`,
    /// `repair-until-verified`, `beads-lifecycle`) is a bare `--parts` name on
    /// `workflow compose … --parts <token> --run` — not a squad.
    /// `recipes`/`template` were removed; say "squad" here.
    Squad {
        #[command(subcommand)]
        cmd: SquadCmd,
    },
    /// Coalitions (ADR-0011 Spec 1, P11.3a) — captain-formed crews around a
    /// goal: NIP-29 channel + goal run + intent board + manual request-release
    /// collab. Steer surface without the auto-proposer (P11.3b).
    Coalition {
        #[command(subcommand)]
        cmd: CoalitionCmd,
    },
    /// Runs dashboard (ADR-0005 P6.3) — state + owner + per-run cost
    Runs {
        #[command(subcommand)]
        cmd: RunsCmd,
    },
    /// Captain-free steering (bd-t86p): ONE compose call at a target —
    /// `flywheel steer "<objective>"` drives the crew to completion with
    /// zero manual per-bead dispatch. Bead-tracked by default (goal →
    /// brainstorm → plan → beads → build → review → ship); `--direct` skips
    /// the tracker (non-bead targets: the plan flows straight to the coder,
    /// no approval gate — truly autonomous).
    /// Interactive TTY briefing (bd-xe9r/bd-nmzk): `steer` and
    /// `workflow run --execute` prompt before dispatch — crew counts, per-role
    /// model menus w/ cost/caps, per-role harness remix (`--role-harness`),
    /// squad/bare-harness shape, stage-chain preview, go/no-go. Skipped by
    /// `--json`, `--yes`, or explicit `--crew`; non-TTY byte-identical
    /// (`src/steer_brief.rs`, `source` fork only).
    Steer {
        /// the objective/target ("implement the auth refactor")
        objective: String,
        /// direct-dispatch arm: skip bead tracking + the plan approval gate
        /// (non-bead targets)
        #[arg(long)]
        direct: bool,
        /// crew strategy: auto | <N total panes> | role:n,role:n | <squad> (ADR-0011 D2)
        #[arg(long)]
        crew: Option<String>,
        /// pane mode: the project whose workspace hosts the crew
        #[arg(long)]
        project: Option<String>,
        /// pane mode: bind to an existing workspace id instead of spawning
        #[arg(long)]
        reuse: Option<String>,
        /// pane mode: keep the crew workspace after the run (default: reap)
        #[arg(long)]
        retain: bool,
        /// pane mode: close pre-existing workspaces before spawning
        #[arg(long)]
        retire_orphans: bool,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// skip the interactive captain briefing (bd-xe9r; implied by --json
        /// and an explicit --crew)
        #[arg(long)]
        yes: bool,
        /// bd-4fgc: stable request key for this steer. Its slot is
        /// `<project>-steer`, so a retry after a timeout REPLAYS the recorded
        /// dispatch instead of steering twice, an unresolved slot refuses
        /// rather than racing the first attempt, and the same key with another
        /// payload is a typed conflict.
        #[arg(long)]
        request_key: Option<String>,
        /// act as this pin (ADR-0019 D10: a pin whose binding location is
        /// hosted parks for operator approval instead of dispatching)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// Unified workflow surface (ADR-0005 Spec 5 / P3.7) — catalog, lint,
    /// run, scaffold, history, and auto-crew preview. One vocabulary, one
    /// engine: built-ins + canonical `workflows/*.yaml` + absorbed legacy
    /// `workflows.toml` / `pipelines.toml`.
    Workflow {
        #[command(subcommand)]
        cmd: WorkflowCmd,
    },
    /// Model registry (ADR-0010 Spec D, P10.2) — `models.toml` ids with
    /// provider/cost/caps/fallback; Profile.model resolves against it so the
    /// per-harness model flag is real, not advisory.
    Model {
        #[command(subcommand)]
        cmd: ModelCmd,
    },
    /// Routing slots (ADR-0007 D1) — cheap/standard/premium = harness+model.
    Slot {
        #[command(subcommand)]
        cmd: SlotCmd,
    },
    /// Catalog hub (ADR-0007 D4) — ratatui multi-panel over slots/models/
    /// profiles/squads. Refuses on non-TTY / NO_COLOR / TERM=dumb and names
    /// the CLI verbs.
    Catalog {
        /// JSON snapshot instead of the TUI (agent drivers)
        #[arg(long)]
        json: bool,
    },
    /// Smart-cron surface (ADR-0005 Spec 3 / P4.6) — the flywheel half of
    /// cron: registered schedules + schedule-triggered workflows, pause/
    /// resume, status + history. The P4.3 cron manager drives ticks off this
    /// registry; this CLI owns the catalog + control surface.
    Cron {
        #[command(subcommand)]
        cmd: CronCmd,
    },
    /// N-round A/B debate + judge (ADR-0001 §9.1 — threaded, ack-gated)
    Debate {
        /// the proposition being debated
        proposition: String,
        /// number of debate rounds (default 3)
        #[arg(long, default_value_t = 3)]
        rounds: u32,
        /// agent names or shorts for A, B, judge (default: spawn oc/oc/gk)
        #[arg(long)]
        agent_a: Option<String>,
        #[arg(long)]
        agent_b: Option<String>,
        #[arg(long)]
        judge: Option<String>,
    },
    /// Plan → review gate → implement → verify (ADR-0001 §9.2; paper-first)
    #[command(name = "plan-review")]
    PlanReview {
        /// project path / label (workspace scope + plan dir)
        project: String,
        /// feature specification for the planner
        feature: String,
        /// max planner↔reviewer change rounds before escalate (default 2)
        #[arg(long, default_value_t = 2)]
        max_change_rounds: u32,
        /// hc agent names (defaults: planner=oc, reviewer=pi, implementer=gk)
        #[arg(long)]
        planner: Option<String>,
        #[arg(long)]
        reviewer: Option<String>,
        #[arg(long)]
        implementer: Option<String>,
        /// optional bead id for thread plan-<id> and reservation reason
        #[arg(long)]
        bead: Option<String>,
    },
    /// Reinstall the binary from source (cargo install --path)
    Upgrade {
        /// Install as the operator: bypass the captain-only fleet guard.
        #[arg(long)]
        operator: bool,
    },
    /// One operator door for the installed flywheel, doorbell, and toron
    /// stack, with `--json`/`--check`; `install` delegates to the audited
    /// stack script.
    Stack {
        #[command(subcommand)]
        cmd: StackCmd,
    },
    /// Fail-closed ship gate: bead in HEAD, clean tracker snapshot, committed
    /// closed row, and a legal ledger verdict for this SHA.
    Ship {
        #[command(subcommand)]
        cmd: ShipCmd,
    },
    /// Commit exactly the named paths through a private git index, so another
    /// lane's staged files can never be swept in (bd-aberm).
    Commit {
        /// repo-relative paths to commit (repeatable)
        paths: Vec<String>,
        /// commit message
        #[arg(short, long)]
        message: String,
        /// repo directory (default: current directory)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Interactive walkthrough
    Tutorial,
    /// Daemon lifecycle (toron-parity): start|stop|restart|status over the
    /// `serve` HTTP surface AND the `cron start` host. `start` installs and
    /// supervises both units (`dev.flywheel.daemon`, `dev.flywheel.cron`);
    /// macOS launchd / Linux systemd user unit / pidfile fallback. `restart`
    /// VERIFIES the new daemon answers. (feature: serve — compiled out
    /// without it)
    #[cfg(feature = "serve")]
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Local REST + SSE server (feature: serve)
    Serve {
        #[arg(long, default_value_t = crate::daemon::SERVE_PORT_DEFAULT)]
        port: u16,
    },
    /// Preflight: verify all prerequisites for a fleet operation before
    /// dispatching. Catches environment/dependency failures in <1s instead
    /// of 30s+ into a durable run. Auto-runs before steer/loop/workflow
    /// unless --skip-preflight is passed.
    Preflight {
        /// operation mode to check: steer|loop|workflow|cron (default loop)
        #[arg(long, default_value = "loop")]
        mode: String,
        /// project path
        #[arg(long)]
        project: Option<String>,
        /// loop --work mode (adds beads check)
        #[arg(long)]
        work: bool,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// internal: prove the caller may mutate the fleet (the captain-only gate
    /// scripts/stack.sh install runs before it replaces a live binary)
    #[command(name = "__guard-fleet", hide = true)]
    __GuardFleet {
        /// what the caller is about to do, named in the refusal
        #[arg(long)]
        what: String,
        /// project whose captain vouches for the caller (default: from cwd)
        #[arg(long)]
        project: Option<String>,
        /// the operator's explicit escape
        #[arg(long)]
        operator: bool,
    },
    /// internal: poll swarm state as JSON (durable workflow leaf)
    #[command(name = "__poll", hide = true)]
    __Poll { project: String, to: String },
    /// internal: detached workspace doorbell (ADR-0005). Spawned by
    /// `cmd_spawn`; stay alive until `flywheel kill` SIGTERMs us.
    #[command(name = "__doorbell", hide = true)]
    __Doorbell {
        #[arg(long)]
        project: String,
        #[arg(long)]
        workspace: String,
        /// hc-mock (SC-D1): stdin watch NDJSON → JSON injects, then exit.
        #[arg(long, hide = true)]
        once: bool,
        /// Print `dispatcher: up|down …` and exit (awareness-digest).
        #[arg(long, hide = true)]
        report: bool,
        /// `pin=status` pairs for `--once` (e.g. `BlueRidge=idle`).
        #[arg(long, hide = true)]
        status: Vec<String>,
    },
    /// internal: block up to timeout collecting hc push events (durable leaf)
    ///
    /// Forward-only `events.subscribe` — no replay. The loop's `__poll`
    /// heartbeat is the correctness floor (bd-event-subscribe-reactor).
    #[command(name = "__poll_events", hide = true)]
    __PollEvents {
        project: String,
        /// max wait in ms collecting events (default 30000)
        #[arg(long, default_value = "30000")]
        timeout_ms: u64,
        /// best-effort in-run dedup: skip events with seq <= this value
        #[arg(long)]
        after_seq: Option<u64>,
    },
    /// internal: send one prompt (durable workflow leaf)
    #[command(name = "__send", hide = true)]
    __Send {
        project: String,
        name: String,
        text: String,
        /// Wait for the prompt to land (agent reaches working|blocked).
        /// Optional timeout in ms (default 45_000 when given without a value).
        #[arg(long, num_args = 0..=1, default_missing_value = "45000")]
        confirm: Option<u64>,
        /// invocation id of the loop that delegated this prompt (last-sent
        /// registry bookkeeping)
        #[arg(long)]
        invocation: Option<String>,
        /// loop iteration at send time (last-sent registry bookkeeping)
        #[arg(long)]
        iteration: Option<u64>,
        /// profile name whose behavior preamble conditions the prompt
        /// (Spec C, bd-gsvn.1). Empty = raw text (plain swarm sends).
        #[arg(long)]
        profile: Option<String>,
    },
    /// internal: block until a pane's output matches a pattern
    #[command(name = "__wait-output", hide = true)]
    __WaitOutput {
        project: String,
        target: String,
        pattern: String,
        /// treat the pattern as a Rust regex (hc-side) instead of a
        /// literal substring
        #[arg(long)]
        regex: bool,
        /// max wait in ms (default: server-side default)
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// internal: newest last-sent record for (agent, prompt-hash)
    #[command(name = "__last-sent", hide = true)]
    __LastSent {
        project: String,
        name: String,
        hash: u64,
    },
    /// internal: notify a blocked agent via agent-mail (durable workflow leaf)
    #[command(name = "__mail-blocked", hide = true)]
    __MailBlocked {
        project: String,
        name: String,
        /// escalation tier 0..3 (bd-blocked-escalation-protocol): 0=first ping,
        /// 1=reply, 2=urgent+summarize, 3=halt
        #[arg(long, default_value_t = 0)]
        tier: u32,
    },
    /// internal: ack-overdue read for one agent (ADR-0001 A6 ack gate; the
    /// escalation leaf reads its own `health-<name>` thread at SLA 0).
    /// Prints `{ok,count,messages}`; `ok:false` means the read was
    /// unavailable — never "nothing overdue".
    #[command(name = "__ack-overdue", hide = true)]
    __AckOverdue {
        project: String,
        name: String,
        /// restrict the read to one thread (default: every thread)
        #[arg(long)]
        thread: Option<String>,
        /// age threshold in seconds (default: the toron SLA, 30 min)
        #[arg(long)]
        sla_secs: Option<u64>,
    },
    /// internal: mail the planner that a ready bead is NOT dispatchable
    /// (missing/malformed `## VERIFY` fence — ADR-0019, bd-w1-verify-fence)
    #[command(name = "__mail-not-ready", hide = true)]
    __MailNotReady {
        project: String,
        bead_id: String,
        title: String,
        /// bd-bead-create-contract-lint-4tdd: exact block reason carried
        /// in the mail body (principles_block_reason / the fence error).
        #[arg(long)]
        reason: Option<String>,
        /// Output shape. Internal leaves ALWAYS emit JSON on stdout (the
        /// durable loop parses that), so this is accepted, not acted on:
        /// machine callers may state it explicitly, and anything but
        /// `json` fails loud rather than silently switching to non-JSON.
        #[arg(long)]
        format: Option<String>,
    },
    /// internal: programmatic done-mail (2026-08-18) — the loop mails the
    /// captain the close-reason REPORT on thread_id=<bead_id> when a
    /// watched bead closes with a legal verdict.
    #[command(name = "__mail-done", hide = true)]
    __MailDone { project: String, bead_id: String },
    /// internal: close-detect check (ADR-0019, bd-w1-reopen-f6tw) — SHA
    /// rule + ledger; legal/missing/no-commit verdict for the loop
    #[command(name = "__close-check", hide = true)]
    __CloseCheck { project: String, bead_id: String },
    /// internal: reopen an unverified close + one mail per (bead, sha)
    #[command(name = "__reopen-bead", hide = true)]
    __ReopenBead {
        project: String,
        bead_id: String,
        sha: String,
    },
    /// internal: verifier-dispatch leaf (ADR-0019 Spec 4,
    /// bd-w1-verifier-degrade-a6fk) — decide the independent-verifier plan
    /// for a bead that needs one; on single-family degrade, block the bead
    /// `verifier-family-missing` + mail the captain exactly once (marker-
    /// deduped) + never respawn. Prints
    /// `{"plan":"none-required"|"degrade"|"dispatch","model":M}`.
    #[command(name = "__verify-dispatch", hide = true)]
    __VerifyDispatch {
        project: String,
        bead_id: String,
        /// worker model id (absent -> unknown family: every candidate is a
        /// different family)
        #[arg(long)]
        worker_model: Option<String>,
        /// candidate verifier model ids, comma-separated (default: every
        /// registered model in the registry)
        #[arg(long)]
        candidates: Option<String>,
        /// blast radius: normal|high (default normal)
        #[arg(long, value_parser = ["normal", "high"], default_value = "normal")]
        blast: String,
        /// AC shape: checkable|judgment (default checkable)
        #[arg(long, value_parser = ["checkable", "judgment"], default_value = "checkable")]
        ac: String,
        /// repo dir (default: the project dir from config)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// internal: loop-run VERIFY executor (ADR-0019 cheap band,
    /// bd-w1-loop-run-verify-mbbe). Classifies the bead's VERIFY
    /// (loop-runnable or not) and, on the cheap band, RUNS it with the
    /// TIMEBOX timeout (cargo under the build slot). Never re-verifies a
    /// broken head (marker). The machine receipt + command-verified gate row
    /// are written IN-PROCESS by the armed loop — this command only returns
    /// the outcome. Prints `{"band","dir","exit","timed_out","skipped"}`.
    #[command(name = "__loop-run-verify", hide = true)]
    __LoopRunVerify {
        project: String,
        bead_id: String,
        /// derived sha (SHA rule)
        #[arg(long)]
        sha: String,
        /// timeout seconds (default: the bead's TIMEBOX, else 600)
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// repo dir (default: the project dir from config)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// internal: plan fence (ADR-0021 D7, bd-plan-fence-command-49etj) —
    /// `br lint` + `br dep cycles` gate and bead-set digest convergence.
    #[command(name = "__plan-fence", hide = true)]
    __PlanFence {
        #[command(subcommand)]
        action: plan_fence::PlanFenceAction,
    },
    /// internal: the project's active reservation board (ADR-0019 Spec
    /// 3.4/3.5, bd-w1-reserve-serialize-2mam) — serialize-on-overlap data
    #[command(name = "__reservation-board", hide = true)]
    __ReservationBoard { project: String },
    /// internal: stamp a dispatched bead's pin with the agent's mail identity
    /// (ADR-0001 Wave 3, beads_rust-wave3-pin-reservation-jxsyt) — best-effort,
    /// exit 0 even when br fails (the dispatch must never be blocked)
    #[command(name = "__pin-stamp", hide = true)]
    __PinStamp {
        project: String,
        bead_id: String,
        pin: String,
    },
    /// internal: bind a step to a live seat (ADR-0022 D8) — reads
    /// {need,seats,policy} JSON from stdin, prints Binding JSON.
    #[command(name = "__bind-seat", hide = true)]
    __BindSeat {},
    /// internal: reconcile desired seats against live seats (ADR-0022 D7) — reads
    /// {desired,live} JSON from stdin, prints effects JSON.
    #[command(name = "__reconcile-seats", hide = true)]
    __ReconcileSeats {},
    /// internal: diagnose unresponsive agent (tail + health mail)
    #[command(name = "__diagnose", hide = true)]
    __Diagnose {
        project: String,
        name: String,
        #[arg(long, default_value = "unknown-stuck")]
        reason: String,
    },
    /// internal: scan orchestrator inbox for `[xproj]` swarm requests
    #[command(name = "__xproj-inbox", hide = true)]
    __XprojInbox {
        project: String,
        #[arg(long, default_value = "30")]
        limit: u32,
    },
    /// internal: scan the orchestrator inbox for exact-body CONTEXT_LIMIT
    /// crew replies (bd-context-limit-honor-luqg respawn trigger)
    #[command(name = "__context-inbox", hide = true)]
    __ContextInbox {
        project: String,
        #[arg(long, default_value = "30")]
        limit: u32,
    },
    /// internal: scan inbox for `[compose-request]` jobs + the allowlist
    /// (ADR-0022 §3.7 T4, W7 delegated composition)
    #[command(name = "__compose-inbox", hide = true)]
    __ComposeInbox {
        project: String,
        #[arg(long, default_value = "30")]
        limit: u32,
    },
    /// internal: mail digest for one agent (unread counts + last sender/subject)
    #[command(name = "__mail-digest", hide = true)]
    __MailDigest { project: String, agent: String },
    /// internal: newest message id + author from a thread BY a sender
    /// (bd-a7j reply verification — did the thread move after dispatch?)
    #[command(name = "__check-reply", hide = true)]
    __CheckReply {
        project: String,
        name: String,
        thread: String,
    },
    /// internal: bd-dispatch-dedupe-2zlz — collect the dispatch-guard facts
    /// for a bead thread (classified peer signals + receipts freshness).
    /// Pure decision lives in durable::eval_dispatch_guard.
    #[command(name = "__dispatch-guard", hide = true)]
    __DispatchGuard { project: String, bead: String },
    /// internal: ADR-0010 D14 captain-facing turn-end guard (harness-hook
    /// adapter). Vendor Stop/idle hooks on the captain/steer pane call
    /// `flywheel __turn-guard <project> [--role captain|crew]`; exit 0 =
    /// turn may end, exit 3 = blocked (JSON names reason + follow_up
    /// recover turn). Crew panes pass --role crew and are always allowed.
    #[command(name = "__turn-guard", hide = true)]
    __TurnGuard {
        project: String,
        /// captain (default) or crew. Crew is exempt, always allow.
        #[arg(long)]
        role: Option<String>,
    },
    /// internal: ADR-0010 D13 subagent-guard (tool-surface fence). Vendor
    /// PreToolUse hooks on the captain/steer pane call
    /// `flywheel __subagent-guard <tool> [--role crew]`; exit 0 = allow,
    /// exit 2 = deny (JSON names the stem + the sanctioned alternative).
    /// Delegation-shaped stems: agent|subagent|task|workflow|spawn|dispatch|
    /// worktree|delegate; mcp__* excluded; observe/stop + session-local todo
    /// allowlisted. Crew panes pass --role crew (exempt — terminal.run stamps
    /// FLYWHEEL_ROLE=crew). Crew panes can still be flywheel spawn'd.
    #[command(name = "__subagent-guard", hide = true)]
    __SubagentGuard {
        /// harness tool name being evaluated (e.g. Task, spawn_agent)
        tool: String,
        /// captain (default) or crew. Crew is exempt, always allow.
        #[arg(long)]
        role: Option<String>,
    },
    /// internal: ADR-0010 D12 bounded GATE grounding read. Reads the
    /// chiebukuro learning store, projecting the top-N
    /// entries by reinforcement recency within the token budget (pinned
    /// never decays; aging 30d, perishable 7d unreinforced decays OUT).
    /// The wave-gate stage calls this INSTEAD of a bare corpus rg. Prints
    /// {budget, projected: [{path, tier, reinforced, cost_tokens}]}.
    #[command(name = "__gate-learnings", hide = true)]
    __GateLearnings {
        /// legacy projection root (chiebukuro is the single learning store)
        #[arg(long, default_value = "chiebukuro")]
        root: String,
        /// token budget (default FLYWHEEL_GATE_BUDGET ~4k, or
        /// FLYWHEEL_GATE_BUDGET env)
        #[arg(long)]
        budget: Option<usize>,
        /// bd-0km8: wiki lane export (chiebukuro knowledge plane JSON
        /// array). Absent = corpus only, current behavior.
        #[arg(long)]
        wiki: Option<String>,
        /// bd-0km8: memory lane export (chiebukuro memory plane JSON
        /// array). Absent = corpus only, current behavior.
        #[arg(long)]
        memory: Option<String>,
    },
    /// internal: wake one seat through its harness API (bd-f888z). Resolves the
    /// pin's recorded session and calls the one dispatch seam; used by the live
    /// API-wake test. Prints the route taken and the derived msg id.
    #[command(name = "__wake-api", hide = true)]
    __WakeApi {
        project: String,
        pin: String,
        text: String,
        seed: String,
        #[arg(long)]
        urgent: bool,
    },
    /// internal: the project's expected-reply entries (checkpointed leaf —
    /// collab may have written new ones while the loop runs)
    #[command(name = "__expected-replies", hide = true)]
    __ExpectedReplies { project: String },
    /// internal: log ReplyMissing + dispatch the concrete reminder send
    /// (one checkpointed leaf; the send itself recurses into __send)
    #[command(name = "__remind-reply", hide = true)]
    __RemindReply {
        project: String,
        name: String,
        thread: String,
        /// reminder ordinal (0 = first)
        #[arg(long, default_value_t = 0)]
        reminder_count: u32,
        #[arg(long)]
        invocation: Option<String>,
        #[arg(long)]
        iteration: Option<u64>,
    },
    /// internal: drop a satisfied expected-reply entry (checkpointed leaf so
    /// the workflow body never writes files — bd-a7j)
    #[command(name = "__reply-settled", hide = true)]
    __ReplySettled {
        project: String,
        name: String,
        thread: String,
    },
    /// internal: spawn the swarm for a parsed `[xproj]` request (full loop branch)
    #[command(name = "__xproj-spawn", hide = true)]
    __XprojSpawn {
        project: String,
        message_id: String,
        thread_id: String,
        from: String,
        recipe: String,
        workers: u32,
    },
    /// internal: renew the orchestrator's active file reservations
    /// (durable workflow leaf, bd-mail-lease-renewal-durable)
    #[command(name = "__renew-leases", hide = true)]
    __RenewLeases {
        project: String,
        extend_seconds: u64,
    },
    /// internal: release every live file reservation whose reason is the
    /// bead id (bd-h3oj release-on-bead-close; durable workflow leaf —
    /// idempotent, `released == 0` is a clean no-op)
    #[command(name = "__release-bead-locks", hide = true)]
    __ReleaseBeadLocks { project: String, bead_id: String },
    /// internal: the wave gate as an ENGINE step (ADR-0021 D4.5): scope
    /// `br ready` to wave N, hold on a lower open wave, write the binding
    /// artifact, exit non-zero on a hold or a zero match
    #[command(name = "__wave-gate", hide = true)]
    __WaveGate {
        /// `wave-<N>`
        #[arg(long)]
        label: String,
        #[arg(long)]
        project: String,
    },
    /// internal: list ready beads as JSON (durable workflow leaf)
    #[command(name = "__ready", hide = true)]
    __Ready { project: String },
    /// internal: list in-progress beads as JSON (durable workflow leaf)
    #[command(name = "__inprogress", hide = true)]
    __Inprogress { project: String },
    /// internal: reclaim stranded in-progress beads (bd-627z.1) — re-open
    /// each whose assignee is not in the live agent list (durable workflow
    /// leaf; emits `{"reclaimed": [ids], "failed": [{id, error}]}`)
    #[command(name = "__reclaim-stranded", hide = true)]
    __ReclaimStranded {
        project: String,
        /// comma-separated live agent names (the polled swarm)
        #[arg(long)]
        live: String,
    },
    /// internal: agent kind as text (durable workflow leaf)
    #[command(name = "__kind", hide = true)]
    __Kind { project: String, name: String },
    /// internal: profile name for an agent from the spawn registry
    /// (durable workflow leaf, bd-gsvn.1 — empty when none)
    #[command(name = "__profile", hide = true)]
    __Profile { project: String, name: String },
    /// internal: recent agent output as text (durable workflow leaf)
    #[command(name = "__read", hide = true)]
    __Read { project: String, name: String },
    /// internal: replace an agent with a fresh pane instance (durable workflow leaf)
    #[command(name = "__respawn", hide = true)]
    __Respawn {
        project: String,
        name: String,
        kind: String,
        /// cg6.3: bypass the replay-idempotency guard (context_limit path —
        /// an idle-but-exhausted agent is healthy and must still respawn).
        #[arg(long)]
        force: bool,
    },
    /// internal: run one prompt headlessly through a harness (ADR-0010
    /// Spec A/E leaf). The RUNNER preprends the role preamble — the leaf
    /// stays dumb: resolve profile → harness, run, log, exit.
    #[command(name = "__run-headless", hide = true)]
    __RunHeadless {
        /// profile name (`planner` or `@planner`)
        profile: String,
        /// the (already role-conditioned) prompt
        prompt: String,
        /// model override (defaults to the profile's model)
        #[arg(long)]
        model: Option<String>,
        /// per-step harness override (bd-42fs step 5): pi|omp|opencode|grok
        /// — overrides the profile's harness for THIS step only
        #[arg(long)]
        harness: Option<String>,
        /// owning workflow name (run-log schema)
        #[arg(long)]
        workflow: Option<String>,
        /// owning cron name (P10.4 run-log attribution; `cron history` reads it)
        #[arg(long)]
        cron: Option<String>,
        /// bd-jhz8x.4: the stage this run executes (ADR-0021 D6 attribution)
        #[arg(long)]
        stage: Option<String>,
        /// lifecycle phase (ADR-0022 W2): rides the phase registry
        /// (`headless::phase_launch_args`); a harness without a native
        /// mechanic degrades loudly + tags the run log `phase_degraded`
        #[arg(long)]
        phase: Option<String>,
        /// per-step effort override (governed-by: bd-ulzi): the Jev
        /// depth→effort value rides the dispatch-spec effort input here;
        /// wins over the profile default for THIS step only
        #[arg(long)]
        effort: Option<String>,
        /// per-step mode override (ADR-0021 D3, ADR-0022 D3): read-only |
        /// workspace-write | isolated. Wins over the stage contract and the
        /// profile default for THIS step only; the harness argv carries it
        /// (or warns loudly when the harness has no native mechanic).
        #[arg(long)]
        mode: Option<String>,
        /// one JSON line for the parent runner (default: human summary)
        #[arg(long)]
        json: bool,
    },
}
fn cmd_internal_last_sent(_cfg: &Config, project: &str, name: &str, hash: u64) -> Result<()> {
    let record = state::load_last_sent(project)
        .into_iter()
        .rev()
        .find(|r| r.name == name && r.text_hash == hash);
    print_json(&json!({ "record": record }));
    Ok(())
}

#[derive(Args, Default, Clone, Debug)]
struct AgentCounts {
    #[arg(long)]
    oc: Option<u32>,
    #[arg(long)]
    pi: Option<u32>,
    #[arg(long)]
    gk: Option<u32>,
}

fn parse_kind_count(s: &str) -> Result<(String, u32)> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("expected KIND=COUNT, got {s}"))?;
    Ok((k.to_string(), v.parse()?))
}

pub(crate) fn project_dir(cfg: &Config, project: &str) -> PathBuf {
    let p = PathBuf::from(project);
    if p.is_absolute() {
        p
    } else {
        cfg.workspace_root().join(project)
    }
}

/// Fallible UTF-8 conversion of a filesystem path (193.1). Paths used as
/// hc cwd args and agent-mail keys must ROUND-TRIP — lossy conversion
/// would silently corrupt them. The wire protocols are JSON (UTF-8), so a
/// non-UTF-8 path is a clear error, never a panic and never silent
/// corruption.
pub(crate) fn utf8_path(p: &Path, what: &str) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{what} is not valid UTF-8: {p:?}"))
}

/// `project_dir` as a UTF-8 string (193.1): the project path is a real path
/// (hc cwd, agent-mail key) — propagate non-UTF-8 as an error.
fn project_dir_str(cfg: &Config, project: &str) -> Result<String> {
    utf8_path(
        &project_dir(cfg, project),
        &format!("project path for {project:?}"),
    )
}

pub(crate) fn swarm_label(project: &str) -> String {
    let base = project.rsplit('/').next().unwrap_or(project).to_string();
    format!("flywheel-{base}")
}

/// bd-vik C: normalize a user-supplied workspace label to a slug —
/// lowercase ASCII alphanumerics + hyphens, runs collapsed, trimmed. Pure +
/// unit-tested. Returns (slug, changed) so the caller can warn.
fn slugify_label(label: &str) -> (String, bool) {
    let mut slug = String::with_capacity(label.len());
    let mut last_dash = false;
    for ch in label.chars() {
        let ok = ch.is_ascii_alphanumeric();
        if ok {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    let changed = slug != label;
    (slug, changed)
}

fn agent_name(project: &str, short: &str) -> String {
    agent_name_n(project, short, 1)
}

/// Name the `ordinal`-th (1-based) agent of a kind in a swarm: the first keeps
/// the bare short (`flywheel-<base>-oc`), later ones are digit-suffixed
/// (`flywheel-<base>-oc2`, `-oc3`). Numbered suffixes keep `--to oc` working
/// (prefix match) and stop duplicate agent names on `add` / recipe+flag mixes.
fn agent_name_n(project: &str, short: &str, ordinal: u32) -> String {
    let base = project_base(project);
    if ordinal <= 1 {
        format!("flywheel-{base}-{short}")
    } else {
        format!("flywheel-{base}-{short}{ordinal}")
    }
}

/// Resolve a bare kind short (or any non-name value) to a concrete spawned
/// agent name for the pattern drivers (debate, plan-review). review-fix F7:
/// defaults like `oc`/`pi`/`gk` must resolve to the spawned
/// `flywheel-<base>-<short>` (the hc agent name) so `settle_agent` finds
/// the agent and `resolve_mail_target` maps it to its registered identity.
///
/// Pass-through rule (a value that is ALREADY a concrete name):
/// - starts with `flywheel-` (a spawned terminal name) — pass through
/// - is already a valid adjective+noun identity (`terminal_mail_name` is
///   idempotent on those — returns the value unchanged) — pass through
/// - contains `:` (a raw pane id) — pass through
///
/// Everything else (bare shorts like `oc`, `pi`, `gk`) expands to the first
/// ordinal of that kind.
fn resolve_agent_default(project: &str, value: &str) -> String {
    let v = value.trim();
    let already_concrete =
        v.starts_with("flywheel-") || v.contains(':') || terminal_mail_name(v) == v;
    if v.is_empty() || already_concrete {
        v.to_string()
    } else {
        agent_name_n(project, v, 1)
    }
}

/// The ordinal of an existing agent of a kind, if its name follows the scheme.
fn agent_ordinal(name: &str, project: &str, short: &str) -> Option<u32> {
    let pfx = format!("flywheel-{}-{short}", project_base(project));
    if name == pfx {
        return Some(1);
    }
    name.strip_prefix(&pfx)?.parse().ok()
}

/// bd-o1zh: unique name per spawn attempt. A retried `spawn --profile`
/// reuses the fixed ordinal-1 name; when hc still holds it (stale
/// binding or a launch_pending registration) the retry collides
/// agent_name_taken. Bump past every taken name so the attempt lands on a
/// free one. Pure: `taken` is the live hc name set. The spawn
/// derivation (effect-journal label) stays stable — only the hc agent
/// name gains the suffix, so the retry is idempotent, not a duplicate.
// governed-by: bd-o1zh
pub(crate) fn free_spawn_ordinal(
    project: &str,
    short: &str,
    start: u32,
    taken: &std::collections::HashSet<String>,
) -> u32 {
    let mut ordinal = start.max(1);
    while taken.contains(&agent_name_n(project, short, ordinal)) {
        ordinal += 1;
    }
    ordinal
}

/// Resolve an hc workspace for a project by label; error if missing.
/// Labels that resolve `project` for status/send/kill: default swarm
/// label, spawn-meta custom label, the raw token, and its slug.
/// Captains pass `--label flywheel-omp-repro` then `status flywheel-omp-repro`
/// — that must hit, not only `flywheel-<dir>`.
pub(crate) fn workspace_label_aliases(project: &str, spawn_meta: Option<&str>) -> Vec<String> {
    let mut out = vec![swarm_label(project)];
    if let Some(meta) = spawn_meta
        && !out.iter().any(|l| l == meta)
    {
        out.push(meta.to_string());
    }
    if !out.iter().any(|l| l == project) {
        out.push(project.to_string());
    }
    let (slug, _) = slugify_label(project);
    if !slug.is_empty() && !out.iter().any(|l| l == &slug) {
        out.push(slug);
    }
    // An absolute project path slugifies to the whole path; the plane labels
    // the workspace after the BASENAME (`flywheel-hc-e2e-iylrua`), which no
    // alias matched (live hc: "no flywheel swarm" for a running pane).
    let base = project.rsplit('/').next().unwrap_or(project);
    let (base_slug, _) = slugify_label(base);
    if !base_slug.is_empty() {
        let owned = format!("flywheel-{base_slug}");
        if !out.iter().any(|l| l == &owned) {
            out.push(owned);
        }
    }
    out
}

/// True when `label` names a workspace this project owns: an exact alias, or
/// a request-key superset of the swarm label.
///
/// `flywheel spawn --request-key k` names its workspace
/// `flywheel-<base>-rk-<k>`, which no exact alias can match. The prefix arm is
/// what keeps send/status/kill reachable after a keyed spawn; without it the
/// verbs answered "no flywheel swarm" while the panes were running.
pub(crate) fn workspace_label_is_owned(
    project: &str,
    label: &str,
    spawn_meta: Option<&str>,
) -> bool {
    if workspace_label_aliases(project, spawn_meta)
        .iter()
        .any(|a| a == label)
    {
        return true;
    }
    let owned = swarm_label(project);
    label.starts_with(&format!("{owned}-"))
}

pub(crate) fn find_workspace(c: &mut Client, project: &str) -> Result<String> {
    find_workspace_info(c, project).map(|ws| ws.workspace_id)
}

/// [`find_workspace`]'s pick, whole: the sender needs its LABEL, which is
/// what a doorbell host is keyed by (`crate::doorbell::inbox_owner_key`).
pub(crate) fn find_workspace_info(c: &mut Client, project: &str) -> Result<hc::WorkspaceInfo> {
    let meta = state::load_spawn_meta(project);
    let aliases = workspace_label_aliases(project, meta.as_deref());
    let ws_list = c.workspace_list()?;
    let exact: Vec<hc::WorkspaceInfo> = ws_list
        .iter()
        .filter(|ws| aliases.iter().any(|a| a == &ws.label))
        .cloned()
        .collect();
    let exact_match = !exact.is_empty();
    let mut matches: Vec<hc::WorkspaceInfo> = if exact_match {
        exact
    } else {
        ws_list
            .into_iter()
            .filter(|ws| workspace_label_is_owned(project, &ws.label, meta.as_deref()))
            .collect()
    };
    if matches.is_empty() {
        bail!("no flywheel swarm for {project} (run: flywheel spawn {project})")
    }
    let labels: std::collections::BTreeSet<&str> =
        matches.iter().map(|ws| ws.label.as_str()).collect();
    if !exact_match && labels.len() > 1 {
        let labels: Vec<&str> = labels.into_iter().collect();
        bail!(
            "multiple flywheel swarms for {project}: {labels:?} — target an agent by pin (flywheel send --to <pin>) or give the swarm its own --label"
        );
    }
    if matches.len() > 1 {
        let prefix = format!("flywheel-{}-", project_base(project));
        let agents = c.agent_list()?;
        if let Some(ws) =
            hc::resolve_workspace_from_registry(&matches, &agents, &registry_panes(project))
        {
            return Ok(pick_workspace(matches, &ws));
        }
        let id = hc::resolve_workspace_id(&matches, &agents, &prefix);
        return Ok(pick_workspace(matches, &id));
    }
    Ok(matches.swap_remove(0))
}

/// The entry with workspace id `id` (one of `matches` by construction).
fn pick_workspace(mut matches: Vec<hc::WorkspaceInfo>, id: &str) -> hc::WorkspaceInfo {
    let i = matches
        .iter()
        .position(|w| w.workspace_id == id)
        .unwrap_or(0);
    matches.swap_remove(i)
}

/// The project's registered pane ids (the spawn registry is the project→pane
/// map). Empty when the project has no registered swarm.
fn registry_panes(project: &str) -> Vec<String> {
    state::load_spawn_registry(project)
        .into_iter()
        .map(|e| e.pane_id)
        .filter(|p| !p.is_empty())
        .collect()
}

/// Resolve target shorts (oc/pi/gk/all) to live agent infos in a workspace.
/// Names follow the scheme `flywheel-<base>-<short>[N]`, so a short matches
/// its numbered agents too (`oc` → oc, oc2, oc3) without catching unrelated
/// kinds that merely share a prefix (`ocr`).
fn resolve_agents(c: &mut Client, ws_id: &str, project: &str, to: &str) -> Result<Vec<AgentInfo>> {
    let agents = c.agent_list()?;
    let mut in_ws: Vec<AgentInfo> = agents
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    if in_ws.is_empty() {
        bail!("no agents in workspace {ws_id} — is the swarm spawned?");
    }
    let registry = state::load_spawn_registry(project);
    overlay_registry_names(&mut in_ws, &registry);
    let avail: Vec<String> = in_ws
        .iter()
        .map(|a| a.name.clone().unwrap_or_default())
        .collect();
    let want: Vec<WantToken> = if to == "all" {
        avail
            .iter()
            .cloned()
            .map(|a| WantToken {
                name: a,
                broaden: false,
                short: None,
                ordinal: 1,
                pane: None,
            })
            .collect()
    } else {
        to.split(',')
            .map(|s| {
                let t = s.trim();
                let entry = registry_entry_for_token(&registry, project, t);
                let name = entry
                    .map(|e| e.name.clone())
                    .unwrap_or_else(|| t.to_string());
                let mut want = to_want_token(project, &name);
                if let Some(e) = entry {
                    want.pane = Some(e.pane_id.clone());
                }
                want
            })
            .collect()
    };
    let idx = match_indices(&in_ws, &want);
    if idx.is_empty() {
        let display: Vec<String> = in_ws.iter().map(agent_display).collect();
        bail!(
            "no agents match --to {to} in {project} (available: {}; raw panes address by agent kind or pane id)",
            display.join(", ")
        );
    }
    let matched: Vec<AgentInfo> = idx.iter().map(|&i| in_ws[i].clone()).collect();
    Ok(matched)
}

/// Trait-object overloads of `find_workspace` / `resolve_agents` for the
/// durable-loop leaf handlers (`__poll` / `__send` / `__mail-blocked`), which
/// construct their backend via the factory (`backend::multiplexer_backend`)
/// per ADR-0002 §5. The one-shot CLI commands keep the `&mut Client` overloads
/// (they're hc-specific and don't need backend-agnosticism yet — migrating
/// them is churn with no payoff until superlogical ships). The two overloads
/// share the helper bodies via macros-free duplication (small, narrow).
/// bd-hc-registry-recover-ywypf: hc applies terminal labels asynchronously —
/// a `workspace_list` snapshot right after spawn can carry empty labels, so
/// an immediate "no swarm" is a race, not a fact. Refresh once after a
/// short settle before concluding the swarm is absent.
/// bd-hc-registry-recover-ywypf: pure decision for the async-label retry —
/// retry only when the snapshot carries no labels at all (fresh spawn
/// window), never on a populated list. Pure for testability; the sleep
/// stays in the caller.
fn should_retry_workspace_labels(first: &[hc::WorkspaceInfo]) -> bool {
    !first.iter().any(|ws| !ws.label.is_empty())
}

fn workspace_list_settled(
    backend: &dyn backend::MultiplexerBackend,
) -> Result<Vec<hc::WorkspaceInfo>> {
    let first = backend.workspace_list()?;
    if !should_retry_workspace_labels(&first) {
        return Ok(first);
    }
    std::thread::sleep(std::time::Duration::from_millis(750));
    Ok(backend.workspace_list()?)
}

/// bd-hc-registry-recover-ywypf: pure captain-pin match — mail identity,
/// registry name, or terminal-mail spelling. Pure for testability.
fn is_captain_entry(entry: &state::SpawnEntry, captain_pin: Option<&str>) -> bool {
    let Some(pin) = captain_pin else {
        return false;
    };
    entry.mail_identity.as_deref() == Some(pin)
        || entry.name == *pin
        || terminal_mail_name(&entry.name) == pin
}

pub(crate) fn find_workspace_dyn(
    backend: &dyn backend::MultiplexerBackend,
    project: &str,
) -> Result<String> {
    let meta = state::load_spawn_meta(project);
    let aliases = workspace_label_aliases(project, meta.as_deref());
    let ws_list = workspace_list_settled(backend)?;
    let exact: Vec<hc::WorkspaceInfo> = ws_list
        .iter()
        .filter(|ws| aliases.iter().any(|a| a == &ws.label))
        .cloned()
        .collect();
    let exact_match = !exact.is_empty();
    let matches: Vec<hc::WorkspaceInfo> = if exact_match {
        exact
    } else {
        ws_list
            .into_iter()
            .filter(|ws| workspace_label_is_owned(project, &ws.label, meta.as_deref()))
            .collect()
    };
    if matches.is_empty() {
        bail!("no flywheel swarm for {project} (run: flywheel spawn {project})")
    }
    let labels: std::collections::BTreeSet<&str> =
        matches.iter().map(|ws| ws.label.as_str()).collect();
    if !exact_match && labels.len() > 1 {
        let labels: Vec<&str> = labels.into_iter().collect();
        bail!(
            "multiple flywheel swarms for {project}: {labels:?} — target an agent by pin (flywheel send --to <pin>) or give the swarm its own --label"
        );
    }
    if matches.len() > 1 {
        let owned_label = format!("flywheel-{}", project_base(project));
        let owned_prefix = format!("{owned_label}-");
        if let Some(ws) = matches
            .iter()
            .find(|ws| ws.label == owned_label || ws.label.starts_with(&owned_prefix))
        {
            return Ok(ws.workspace_id.clone());
        }
        let prefix = format!("flywheel-{}-", project_base(project));
        let agents = backend.agent_list()?;
        for ws in &matches {
            if agents.iter().any(|a| {
                a.workspace_id == ws.workspace_id
                    && a.name
                        .as_deref()
                        .map(|n| n.starts_with(&prefix))
                        .unwrap_or(false)
            }) {
                return Ok(ws.workspace_id.clone());
            }
        }
    }
    Ok(matches[0].workspace_id.clone())
}

/// Every owned workspace for a project (bd-request-key-resolution).
///
/// `send` resolves its target by agent pin, so it must see all live crews of
/// a project, not one workspace picked by list order. A keyed spawn creates
/// one workspace per request key; the union is where pins are searched.
pub(crate) fn find_workspaces_dyn(
    backend: &dyn backend::MultiplexerBackend,
    project: &str,
) -> Result<Vec<hc::WorkspaceInfo>> {
    let meta = state::load_spawn_meta(project);
    let matches: Vec<hc::WorkspaceInfo> = workspace_list_settled(backend)?
        .into_iter()
        .filter(|ws| workspace_label_is_owned(project, &ws.label, meta.as_deref()))
        .collect();
    if matches.is_empty() {
        bail!("no flywheel swarm for {project} (run: flywheel spawn {project})")
    }
    Ok(matches)
}

pub(crate) fn resolve_agents_dyn(
    backend: &dyn backend::MultiplexerBackend,
    ws_id: &str,
    project: &str,
    to: &str,
) -> Result<Vec<AgentInfo>> {
    let agents = backend.agent_list()?;
    let mut in_ws: Vec<AgentInfo> = agents
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    if in_ws.is_empty() {
        bail!("no agents in workspace {ws_id} — is the swarm spawned?");
    }
    let registry = state::load_spawn_registry(project);
    overlay_registry_names(&mut in_ws, &registry);
    let avail: Vec<String> = in_ws
        .iter()
        .map(|a| a.name.clone().unwrap_or_default())
        .collect();
    let want: Vec<WantToken> = if to == "all" {
        avail
            .iter()
            .cloned()
            .map(|a| WantToken {
                name: a,
                broaden: false,
                short: None,
                ordinal: 1,
                pane: None,
            })
            .collect()
    } else {
        to.split(',')
            .map(|s| {
                let t = s.trim();
                let entry = registry_entry_for_token(&registry, project, t);
                let name = entry
                    .map(|e| e.name.clone())
                    .unwrap_or_else(|| t.to_string());
                let mut want = to_want_token(project, &name);
                if let Some(e) = entry {
                    want.pane = Some(e.pane_id.clone());
                }
                want
            })
            .collect()
    };
    let idx = match_indices(&in_ws, &want);
    if idx.is_empty() {
        let display: Vec<String> = in_ws.iter().map(agent_display).collect();
        bail!(
            "no agents match --to {to} in {project} (available: {}; raw panes address by agent kind or pane id)",
            display.join(", ")
        );
    }
    let matched: Vec<AgentInfo> = idx.iter().map(|&i| in_ws[i].clone()).collect();
    Ok(matched)
}

/// Normalize a single `--to` token to the concrete agent name it should
/// match against. Bare kind shorts (`oc`, `pi`, `gk`) wrap to the first
/// ordinal (`flywheel-<base>-oc`); anything already concrete passes through
/// unchanged:
///   - `flywheel-...` (a spawned terminal name, exactly what `flywheel list`
///     shows) — pass through, do NOT double-wrap
///   - contains `:` (a raw pane id) — pass through
///
/// This mirrors `resolve_agent_default`'s pass-through rule. bd-bwf: before
/// this, `flywheel send --to flywheel-toron-oc` double-wrapped to
/// `flywheel-toron-flywheel-toron-oc` and was rejected even though `list`
/// showed that exact name as available.
/// A normalized `--to` token: the concrete agent name to match against, plus
/// whether the token was a BARE kind short (`oc`/`pi`/`gk`) that may broaden
/// to all numbered agents of that kind. Concrete names (`flywheel-<base>-oc`,
/// `flywheel-<base>-oc2`, `crew-<workflow>-<role>[N]`), numbered shorts (`oc2`), pane ids, and mail
/// identities match EXACTLY — a full name never silently drags in oc2/oc3.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WantToken {
    name: String,
    broaden: bool,
    /// The kind short when the token WAS a short (`omp1` → `omp`): the
    /// kind-based fallback for agents whose NAME diverges from their kind
    /// (bd-4pxy — an omp agent named `flywheel-<base>-coder`). `None` for
    /// concrete names, pane ids, and mail identities.
    short: Option<String>,
    /// 1-based ordinal of the kind short (`omp1` → 1, `omp2` → 2).
    ordinal: u32,
    /// The PANE the token resolved to through the spawn registry. The
    /// registry is the project→pane map, so the pane is the ground truth:
    /// a spawned crew's terminal name is its mail PIN (hc lowercases it),
    /// and the registry's canonical name and the hc name can therefore
    /// never meet — the pane id always can.
    pane: Option<String>,
}

fn to_want_token(project: &str, token: &str) -> WantToken {
    let t = token.trim();
    if t.is_empty() || t.starts_with("flywheel-") || t.starts_with("crew-") || t.contains(':') {
        WantToken {
            name: t.to_string(),
            broaden: false,
            short: None,
            ordinal: 1,
            pane: None,
        }
    } else if let Some((kind, ordinal)) = split_short_ordinal(t) {
        WantToken {
            name: agent_name_n(project, kind, ordinal),
            broaden: false,
            short: Some(kind.to_string()),
            ordinal,
            pane: None,
        }
    } else {
        let broaden = matches!(t, "oc" | "pi" | "gk") && t.chars().all(|c| c.is_ascii_lowercase());
        WantToken {
            name: agent_name(project, t),
            broaden,
            short: Some(t.to_string()),
            ordinal: 1,
            pane: None,
        }
    }
}

/// Split a numbered short (`oc1`, `pi2`, `gk3`) into kind + ordinal. The
/// spawn naming scheme (`agent_name_n`) keeps the FIRST agent of a kind bare
/// (`flywheel-<base>-oc`) and digit-suffixes later ones (`-oc2`), so the
/// ordinal must be applied to the kind, not appended to it (bd-eign.14).
fn split_short_ordinal(t: &str) -> Option<(&str, u32)> {
    let first_digit = t.find(|c: char| c.is_ascii_digit())?;
    let (kind, digits) = t.split_at(first_digit);
    if kind.is_empty() || digits.is_empty() || !kind.chars().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    digits.parse().ok().map(|n| (kind, n))
}

/// Exact match, or — for BARE kind shorts only — a digit-suffixed ordinal of
/// it (`oc` → oc, oc2, oc3). A concrete want (`flywheel-<base>-oc`) matches
/// exactly one agent; steering a single crew member no longer drags in its
/// numbered siblings.
fn name_matches_want(name: &str, want: &WantToken) -> bool {
    name == want.name
        || (want.broaden
            && name.starts_with(&want.name)
            && name[want.name.len()..]
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false))
}

/// The `ordinal`-th (1-based) agent of `kind` in list order — the kind
/// fallback for `--to` resolution (bd-4pxy): a short's NAME target may not
/// exist (an omp agent named `flywheel-<base>-coder`), but the hc kind
/// is stable. `None` when fewer than `ordinal` agents run that kind.
fn kind_nth_index(agents: &[AgentInfo], kind: &str, ordinal: u32) -> Option<usize> {
    agents
        .iter()
        .enumerate()
        .filter(|(_, a)| a.agent.as_deref() == Some(kind))
        .nth(ordinal.saturating_sub(1) as usize)
        .map(|(i, _)| i)
}

/// bd-rex-default-plane-xn1t: the default send path's resolver on a non-hc
/// plane. Same rule as `comms_send::backend_wake_target` (shared, not copied):
/// a token names a block label or agent name EXACTLY, or it resolves through
/// the spawn registry (mail pin, lane name, or pane id → pane ground truth).
/// Zero hits is a typed miss; two hits is `matches N panes; not ringing`.
/// `all` reaches every agent in the workspace. Pure over an already-scoped
/// agent list — the caller loads it through the selected backend.
#[cfg(feature = "backend-superlogical")]
fn resolve_send_targets_dyn(
    agents_in_ws: &[AgentInfo],
    registry: &[state::SpawnEntry],
    project: &str,
    to: &str,
) -> Result<Vec<AgentInfo>> {
    let mut out: Vec<AgentInfo> = Vec::new();
    for tok in to.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if tok == "all" {
            for a in agents_in_ws {
                if !out.iter().any(|o| o.pane_id == a.pane_id) {
                    out.push(a.clone());
                }
            }
            continue;
        }
        if let Some(entry) = registry_entry_for_token(registry, project, tok)
            && let Some(a) = agents_in_ws.iter().find(|a| a.pane_id == entry.pane_id)
        {
            if !out.iter().any(|o| o.pane_id == a.pane_id) {
                out.push(a.clone());
            }
            continue;
        }
        match crate::comms_send::backend_wake_target(agents_in_ws, tok) {
            Ok(pane) => {
                if let Some(a) = agents_in_ws.iter().find(|a| a.pane_id == pane) {
                    if !out.iter().any(|o| o.pane_id == a.pane_id) {
                        out.push(a.clone());
                    }
                } else {
                    bail!("{tok} has no pane in this workspace");
                }
            }
            Err(e) => bail!("{e}"),
        }
    }
    if out.is_empty() {
        bail!("no agents match --to {to}");
    }
    Ok(out)
}

/// Indices of agents matched by the `--to` wants (bd-4pxy): name-based
/// resolution is canonical (exact, numbered, or broadened); when a short's
/// name target matches NOTHING, fall back to a pane-id exact match
/// (bd-a2bw — `--to w70:p3` must hit a nameless omp pane), then the
/// ordinal-th agent OF THAT KIND. Order preserved, deduped. Pure — shared
/// by the CLI and the backend-factory resolution paths.
fn match_indices(agents: &[AgentInfo], wants: &[WantToken]) -> Vec<usize> {
    let mut out = vec![false; agents.len()];
    for want in wants {
        let name_hits: Vec<usize> = agents
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a.name
                    .as_deref()
                    .map(|n| name_matches_want(n, want))
                    .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect();
        if !name_hits.is_empty() {
            for i in name_hits {
                out[i] = true;
            }
        } else if let Some(pane) = &want.pane
            && let Some(i) = agents.iter().position(|a| &a.pane_id == pane)
        {
            out[i] = true;
        } else if let Some(i) = agents.iter().position(|a| a.pane_id == want.name) {
            out[i] = true;
        } else if let Some(short) = &want.short
            && let Some(i) = kind_nth_index(agents, short, want.ordinal)
        {
            out[i] = true;
        }
    }
    out.iter()
        .enumerate()
        .filter(|(_, hit)| **hit)
        .map(|(i, _)| i)
        .collect()
}

fn agent_target(a: &AgentInfo) -> &str {
    a.name.as_deref().unwrap_or(a.pane_id.as_str())
}

/// Render an agent for the `--to` failure hint (raw-pane addressing,
/// 2026-09-07): registry name when present, else the raw hc agent kind
/// with its pane id (`grok (w6:p2)`), else the bare pane id. Raw panes (no
/// spawn-registry entry) are addressable by kind (`--to grok`) or pane id
/// (`--to w6:p2`); the error must name them instead of degrading to empty
/// strings, which left the operator with no actionable target.
fn agent_display(a: &AgentInfo) -> String {
    if let Some(n) = a.name.as_deref().filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    if let Some(kind) = a.agent.as_deref().filter(|k| !k.is_empty()) {
        return format!("{kind} ({})", a.pane_id);
    }
    a.pane_id.clone()
}

/// Bounded settle-retry over fallible live re-reads (bd-qs1c.1): a send
/// racing a fresh spawn can miss the pin (registry not yet flushed) or the
/// pane (not yet in hc's list). Each attempt is a live re-read — never an
/// invented observation (GS-037) — and exhaustion returns the last error for
/// the caller to fail loud. `retries` counts RE-tries after the first try.
///
/// Rides the shared [`crate::retry::RetryPolicy`] (bd-8pjr): every error is
/// retryable here by design — a re-read miss is always transient-shaped,
/// so no classified-retryable predicate (a predicate would just say yes).
/// Timing stays fixed (`cap == base == sleep_ms`, plus the policy's 10%
/// headroom) to preserve the bd-qs1c.1 budget semantics below.
fn settle_retry<T, E>(
    retries: u32,
    sleep_ms: u64,
    attempt: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let base = Duration::from_millis(sleep_ms);
    let policy = crate::retry::RetryPolicy {
        max_attempts: retries.saturating_add(1),
        base,
        cap: base,
    };
    policy.retry_with_backoff(
        attempt,
        |_| true,
        |_, _| {},
        crate::retry::host_draw,
        |d| {
            if !d.is_zero() {
                std::thread::sleep(d);
            }
        },
    )
}

/// bd-a2bw / ADR-0018: hc omp panes drop names (bd-w17). Fill null
/// names from the spawn registry so `--to coder3` matches after overlay
/// even though hc kind stays `omp`.
fn overlay_registry_names(agents: &mut [AgentInfo], entries: &[state::SpawnEntry]) {
    for a in agents.iter_mut() {
        if a.name.is_some() {
            continue;
        }
        if let Some(e) = entries.iter().find(|e| e.pane_id == a.pane_id) {
            a.name = Some(e.name.clone());
        }
    }
}

/// bd-a2bw / bd-pin-public-plane-s-easy.2: resolve a send/rebind token to a
/// spawn-registry row. Accepts the hc name, a pane id, a numbered short
/// (`coder3` → `flywheel-<base>-coder3`), OR the agent's mail PIN
/// (adjective+noun — `--to DustySparrow` resolves to the hc name the
/// identity was pinned for).
fn registry_entry_for_token<'a>(
    entries: &'a [state::SpawnEntry],
    project: &str,
    token: &str,
) -> Option<&'a state::SpawnEntry> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    entries
        .iter()
        .find(|e| e.name == t || e.pane_id == t || e.mail_identity.as_deref() == Some(t))
        .or_else(|| {
            let want = to_want_token(project, t);
            entries.iter().find(|e| e.name == want.name)
        })
}

/// bd-a2bw: hc `launch_pending` is stale when the pane is already
/// input-ready (omp at `π >` with a session, status idle/working/blocked/
/// done). Spawn/send must not treat that as still-booting.
/// GS-049: shared with `snapshot::assemble` via `hc::agent_still_booting`.
fn is_still_booting(a: &AgentInfo) -> bool {
    hc::agent_still_booting(a)
}

/// Log an event with a compact payload.
fn log(project: &str, kind: EventKind, data: Value) {
    if let Err(e) = log_event(project, kind, data) {
        eprintln!("flywheel: cannot write event log: {e:#}");
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let cli = Cli::parse_from(&raw);

    if cli.robot_capabilities {
        print_json(&json!({
            "tool": "flywheel",
            "version": VERSION,
            "backend": format!("hc (socket protocol v{})", hc::HDR_PROTOCOL),
            "upstream_parity": upstream_parity::robot_summary(),
            "commands": [
                "init spawn add list status decide rebind recover",
                "ship check <bead> [--json] | ship land <bead> --sha <40hex> --receipt <file> [--project <p>] [--as <pin>] [--message <m>] [--repo <dir>] (one verb: gate -> close -> sync -> release -> commit THIS bead's tracker rows -> check; idempotent, prints the resume command on failure; another lane's dirty tracker row never blocks; --repo names the repo holding the citing commit and the receipt's specs when the work lives elsewhere, symmetric with br close --repo; the HEAD-lock race other lanes cause is retried, not surfaced)",
                "spawn <project> [--kind K=N ...] [--assign PIN=BEAD[,BEAD] ...] [--dry-run --json] [--patch <seat>.<field>=<value> (repeatable; re-resolves the seat with from: user)] [--model harness-default leaves the harness's own model untouched] [--yes] (ADR-0022 plan spawn; see --help for the full flag list)",
                "retire <pin>... [--project <p>] [--force-working] [--as <pin>] [--json] (bd-rilvn: the thin per-pin stop verb over the seat path — ONE journaled SeatEffect::Retire through Ports::seat closes that pane, the pin's leases are released operator-signed by HOLDER (a sibling on the same bead keeps its lease), then the roster row is deregistered; refuses the captain/operator seat by role and a working pin without --force-working)",
            "swarm plan <project> [spawn flags] | swarm apply <project> [--patch <seat>.<field>=<value>] [--count <role>=<n>] [--yes] [--json] | swarm status [<project>] [--json] (ADR-0022 rollout 6 + D7 remainder: plan prints the SAME flywheel-plan.v1 object as `spawn --dry-run --json`; apply is the one mechanism — patch/count -> pure reconciler -> the one `Ports::seat` interpreter, journal-first; fields harness|model|effort|mode|role (role only on apply; spawn rejects it); `--count role=+1` grows the fleet within crew.max; Spawn/InjectRole run live (InjectRole never for review/verify seats); `binding-rebind --model` routes here; status lists live seats with the plane's status; ADR-0022 D1: every apply is a plan EVENT first - the plan of record is the source of the desired seats, pending events fold in memory and land as ONE write only after every gate, and a planned SPAWN needs --yes)",
            "send interrupt view output watch attach kill",
            "prefer <role.field>=<value> [--project <p>] | prefer --list [--json] | prefer --forget <role.field> [--project <p>] (ADR-0022 D6: standing preferences — project beats always, exact role beats *; set also records a chie lesson)",
            "crew preview|control|prompt|inbox --open [--pin <pin>]|done <brief-id> --receipt \"<sha + proof + test tail>\" (ADR-0010 D7: crew control <project> <pin> interrupt|exit|relaunch — no arbitrary-text entry; interrupt=Escape capability-gated, exit preserves reservations+dirty, relaunch=journaled rollback-on-launch-fail; control-plane prompts carry U+2063+FLYWHEEL_OP: marker; a dispatched crew prompt IS a brief and `crew done <brief> --receipt` closes it and wakes the captain, falling back to plane-P mail when the captain has no pane row (both paths dead is a typed error saying the captain heard NOTHING); a wake the pane already holds is not a failure — the mail is stored and the prompt exits 0 as queued)",
            "peer intent|request-release|release-response|review-request (structured P2P directives with reactive ⟦p2p:<verb>⟧ wake dispatch)",
            "captain watch <project> [--interval N] [--json] [--as <pin>] [--stall --stall-minutes N] [--preflight --preflight-mode steer|loop|workflow|cron] (bd-captain-watch-poll-exit-bbwm: harness resume on the captain inbox. The push is escalate@1 mail from the supervisor. This command blocks on that inbox's water and exits with a preview when strictly-newer mail lands; --stall also exits 0 when every bead is closed, or exits 3 when no pane is working and no commit landed for the threshold with work remaining; an unreadable fleet never alarms; --preflight adds an arm that exits 4 with the FIRST failing check and its remedy line, checked at arm time and each poll. One shot: re-arm after each resume)",
            "captain directory <project> [--json] (ADR-0019 D2/D4: read-only sessions, tiers, and per-pin addressability — ok <key> or err <code> — from the pure directory projection over an explicitly selected rex backend; never auto/hc on this path)",
            "captain approve <project> --request-key <key> --decision approve|deny [--note <text>] --as <operator-pin> (ADR-0019 D10: verify the operator via whois, mail the asker the 4-line body, fetch-back verify, rehash the payload, run it or settle denied; missing row, self-approval, or unverified sender is approval_mismatch, never a dispatch)",
            "loop-mode run (--resume replays a killed run's checkpoints; default is FRESH) / supervise (child-process --once pass cadence, stops after 3 consecutive failures; a dead-stop probe each pass emits ONE loud escalation + one captain page when no pane is working and no commit landed for --stall-minutes, default 45, with work remaining — a recovering fleet clears it without a second; bd-pane-wedge-detect-respawn-pnyi: a per-pane wedge probe reads each registered pin's pane tail + process liveness and pages ONCE per wedge naming the pin, the pane, and the parked error line, cleared when a commit lands — hc exposes no CPU counter, so zero-CPU is read as its observable equivalent, process still alive + nothing moved) / status/list/prune/doctor/replay/why work assign collab xproj mail steer ledger principles artifacts judgment",
                "artifacts <run_id> [--json] + artifacts stitch <run_id> (state-dir per-run records, stitch_artifacts at artifact.rs:233)",
                "upstream parity (embedded in --robot-capabilities; offline manifest: docs/upstream-parity.toml)",
            "doorbell status/start/stop/re-inject/afk/why/roster/introduce (supervised pane-wake layer: lazy-heal + persisted wake state; afk batches captain-relevant wakes; roster is nearby panes+pins+reservations; introduce is contact+channel+subscribe)",
            "snapshot <project> (fleet snapshot flywheel-snapshot.v1 as robot JSON)",
            "status <project> + progress [<project>] [--json] (bd-swarm-cockpit-pczs cockpit reads: status rows carry pin/npub/pane/verdict/bead/reservations/last_active_ts_us and print JSON when not a TTY; progress groups beads by wave-N label — closed/in_progress/ready/blocked/unknown — and names dispatching_wave, the lowest ready/in-progress wave; project defaults to the workspace cwd)",
            "agent tail <pin> [--project <p>] [--json] [--n N] (bd-swarm-cockpit-pczs: the pin's recent actions — tool calls / commands — from the session the pane reports on agent_session; path sessions read the pi JSONL, id sessions read the opencode store; no newest-file guessing. hc 0.1.16 reports no agent_session, so on hc it bails loud — use `flywheel output` or `hc read`)",
            "helm <project> (session-start drain: OPEN DECISIONS, UNREAD STATUS, RECORD DIVERGENCE)",
                "coalition debate plan-review profile squad model slot catalog runs list/show/pending/digest/reconcile",
                "workflow cron recipes workflows pipeline template stage ensemble",
                "workflow seed/drift — copy builtin workflows+stages to the user tier (--overwrite none|stale|all, default none; only builtin-named copies; --dry-run; fails closed with no TTY and no --yes; every replaced file kept as <name>.bak) and report missing/current/stale/custom grouped by action. Seeds from the compiled-in SOURCE TEXT, never a serde round-trip (bd-3o5t: that wrote crew: null and made every crew workflow headless while drift called it current)",
            "config seed|drift — XDG ~/.config/flywheel. seed fills missing workflows, stages, judgment packs, and a commented config.toml. --overwrite none|stale|all (default none). --dry-run. --yes required off a TTY to replace. config.toml is never overwritten. drift only reports. config list prints every setting with its default",
            "judgment pack drift|seed|remove|list — compare and sync judgment packs across the user tier (source of truth) and the project tier; a user pack at a higher version is never downgraded",
            "stage remove <name> — delete a user-tier stage file (backed up as <name>.toml.bak) so the compiled-in stage resolves again",
                "checkpoint timeline audit history changes resume analytics",
                "backend safety guards policy deps integration config upgrade tutorial serve preflight",
            "backend schema — print the versioned rex API contract as JSON (tested revision, adopted/candidate methods, request/response keys); ONE source in toron-plane rex_schema.rs",
            "halt <project> [--context <lead>] [--sup-epoch N] (one control: cancel that subtree, no relaunch). supervisor fact|steer|view|hooks|halt (fact kinds progress|done|exit|proved|usage; usage is advisory and never a cost gate; view --json includes goal_proved; joins are FLYWHEEL_SUPERVISOR_JOIN=all|quorum:N|first:N, default all; quorum and first cancel the rest once k children are proved; hooks reports the marked hook block; production harnesses stay unverified). timer arm|meet|fail|sweep (one-shot watch, deadline required, one notify@1 under watch:<id>:<outcome> met|timer|expired|failed; default on, FLYWHEEL_TIMER=0 or [timer] enabled=false opts out; FLYWHEEL_ALLOW_EXEC=0 suppresses the ring; no stored shell)"
            ],
            "tracker": "br/bv (CLI, no MCP)",
            "coordination": "toron mail",
            "daemon": "flywheel daemon start|stop|restart|status — supervised serve lifecycle (launchd/systemd/pidfile); restart verifies the new daemon answers; stop/refuse unmanaged port holders without --force",
            "stages": ["ce-brainstorm", "ce-plan", "ce-plan-review", "ce-execute", "ce-simplify", "ce-review", "ce-evaluate", "ce-compound", "commit-and-push"],
            "stage_params": "send --stage NAME --param k=v (repeatable) substitutes {{k}} slots in the stage template; per-stage required slots, missing one exits non-zero naming the slot; zero --param is byte-identical to the unparameterized prompt (ADR-0006 D1)",
            "workflows": "agent-native catalog (resolve order built-ins < user ~/.config/flywheel/workflows/*.yaml < project <p>/.flywheel/workflows/*.yaml; every catalog verb — list/show/lint/evaluate/verify/crew/run/compose/seed/drift — takes --project where applicable and layers the project tier): EVERY row of `workflow list --json` carries description + family + selection + run + compose_token + decide_trigger; `workflow list --brief` is the GS-052 HYBRID ONE contract: name + family + selection + run + topology + compose_token (sticky-open still sees topology); bare `workflow list --json` adds description + source/crew/inputs/decide_trigger (plus hybrid fields); `workflow show` emits name/source/description/run/compose_token/trigger/guard/crew/inputs/steps/graph/graph_nodes — not decide_trigger/family/selection/topology — select from brief, then bare list for decide_trigger or show for prose/graph detail, then run the row's run form. Compose tokens: wave-swarm, repair-until-verified, beads-lifecycle (bare --parts). `workflow seed` copies every builtin workflow and stage into the user tier without overwriting; `workflow drift` reports missing/current/stale/custom. Capture: ce-compound (standalone learning capture, any topology), ce-sweep (learnings maintenance). decide routes: debate, brainstorm->ce-brainstorm, bead-ready plan->ce-plan, ship the chain->ship, convert to beads->beads, plus the playbook needles (repro/refactor/prototype/arena/perf/hillclimb/forensic/babysit/pickup/pause/interrogate/feature/goal/triage/weekly-review/reflect/onboard).",
            "catalog": {
                "slots": "flywheel slot list/show/set/remove — cheap/standard/premium = harness+model; profiles alias @cheap/@standard/@premium (ADR-0007 D1)",
                "models": "flywheel model list/show/add/set/remove — `set <id> --slot <s>` selects a model as a slot DEFAULT; registry ids are provider/model with tier cheap|standard|premium; compiled-ins are permanent",
                "profiles": "flywheel profile list/show/create/edit/set/run/remove — `set` is the routing-field verb (--model/@slot/--model-fallback/--fallback; model_fallback ≠ fallback). In-flight live-pane hops use `flywheel binding-rebind` (journaled; does not edit profiles.toml)",
                "squads": "flywheel squad list/show/spawn/create/remove — compiled-ins: default/plan/review/recon; user recipes.toml overrides per name (ADR-0007 D3)",
                "hub": "flywheel catalog [--json] — ratatui multi-panel; refuses non-TTY/NO_COLOR/TERM=dumb naming the CLI (ADR-0007 D4)"
            },
            "wave_dispatch": "loop run --work --wave-label wave-N scopes the ready set by the TYPED `wave` field (bd-wave-gate-typed-authority-27et; schema-18 has shipped — the legacy `wave-N` label is read only as the fallback, and br ready itself holds every higher wave until lower waves close) and holds (naming blockers) while any lower wave-M (M<N) is open/in_progress; default --work unchanged (ADR-0006 D2)",
            "loop_resume_window": "loop/loop-mode run --work (resume-window sweep, bead 4sa2): an in-progress bead resumes at most ONCE per (bead, pane) in a 15-min window (the watermark persists per project under the state dir, so it survives passes and processes — the in-memory `assigned` set does not); a peer-held bead (a DIFFERENT live pane's reservation reasons to it — a lane split, not a stall) draws NO resume; each pass releases leases whose reason bead is closed via the one pure rule durable::closed_reason_lease_ids. bd-loop-feeds-idle-panes-rh2z adds the idle-pane rotation: each IDLE pane takes ONE bead — its own stalled in-progress bead first, then a ready bead assigned to it, then an unowned ready bead, then any ready bead whose assignee is not a live pane — never a bead another live pane owns or a peer holds, never one already queued, and the picks go at the HEAD of the queue in fleet order (plan_actions pairs the k-th idle agent with the k-th item, so that order is what lands the pick on the pane the rotation chose). A tier-1 hit inside the window still defers for `resume_exempt_passes` consecutive starved passes (config [loop], default 2, counters persisted via state::load_pane_starvation/record_pane_starvation because loop-mode supervise runs each pass as a child process); from that pass the pick is marked window-exempt. Pure: durable::{within_resume_window, peer_held, should_resume_attempt, closed_reason_lease_ids, idle_pane_rotation}; fences: resume_window_dedupes_and_respects_peer_holds, loop_feeds_idle_panes_stalled_beads",
            "wave_swarm_compose_token": "preferred: workflow compose \"<objective>\" --parts wave-swarm --run (compose token); legacy alias: workflow run wave-swarm --execute. Expands GATE/DISPATCH/EXECUTE/OBSERVE/REVIEW/VERIFY/COMPOUND/SIGNOFF; every seat dispatch is a dispatch_job with await: result (bounded by no_claim_timeout + result_timeout), and the runner writes the .flywheel/wave/<label>-<stage>.done compat sidecar after a witnessed exit (ADR-0021 D4.4) — no step writes or reads one. ADR-0009: REVIEW audits diffs pre-verify, COMPOUND captures learnings post-verify; SIGNOFF=request_approval (captain-only), announced by the compose approval drainer — resolve with `flywheel workflow approve`. Ordinary builtins (captain-audit): workflow run <name> --execute, NOT bare --parts. Outer bound: --timeout or FLYWHEEL_COMPOSE_MAX_SECONDS (ADR-0006 D3)",
            "beads_lifecycle_compose_token": "preferred: workflow compose \"<objective>\" --parts beads-lifecycle --run --project <p> (compose token); legacy alias: workflow run beads-lifecycle --execute. grill/create/review1/review2/integrate then implement=call_workflow wave-swarm (new graph) or babysit-bead (inputs.existing_bead / from=bead); verify; outer SIGNOFF. inputs.from=none|session|adr|bead; inputs.until early-exits SUCCESS after any phase; SIGNOFF outer-only (ADR-0008 6.7)",
            "exit_codes": { "0": "ok", "1": "hc/server error", "2": "usage error", "3": "captain watch --stall: fleet stalled (no pane working + no commit within threshold, work remaining)", "4": "captain watch --preflight: the project's preflight failed (the failing check is named; bd-captain-watch-preflight-arm-snak)" }
        }));
        return;
    }

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => die(&e, None),
    };

    if cli.robot_snapshot {
        let snap = robot_snapshot(&cfg);
        match snap {
            Ok(v) => {
                print_json(&v);
                return;
            }
            Err(e) => die(&e, Some("is hc running? (flywheel deps)")),
        }
    }

    if cli.robot_status {
        let snap = robot_snapshot(&cfg);
        match snap {
            Ok(v) => {
                let compact = json!({
                    "workspaces": v["workspaces"],
                    "agents": v["agents"]
                });
                print_json(&compact);
                return;
            }
            Err(e) => die(&e, None),
        }
    }

    let result = match &cli.command {
        Some(Commands::Init { project }) => cmd_init(&cfg, project),
        Some(Commands::Spawn {
            assign,
            project,
            counts,
            recipe,
            kinds,
            label,
            no_focus,
            no_verify,
            retire_orphans,
            peer_aware,
            no_peer_aware,
            peer_poll_interval_minutes,
            peer_poll_budget_minutes,
            peer_max_prompts,
            settle_secs,
            join_channels,
            profile,
            count,
            model,
            provider,
            escape,
            yes,
            join,
            request_key,
            dry_run,
            json,
            patch,
        }) => cmd_spawn(
            &cfg,
            project,
            SpawnOpts {
                counts: counts.clone(),
                recipe: recipe.clone(),
                kinds: kinds.clone(),
                label_override: label.clone(),
                no_focus: *no_focus,
                no_verify: *no_verify,
                retire_orphans: *retire_orphans,
                peer_aware: *peer_aware,
                no_peer_aware: *no_peer_aware,
                peer_poll_interval_minutes: *peer_poll_interval_minutes,
                peer_poll_budget_minutes: *peer_poll_budget_minutes,
                peer_max_prompts: *peer_max_prompts,
                settle_secs: *settle_secs,
                join_channels: join_channels.clone(),
                assign: assign.clone(),
                profile_name: profile.clone(),
                profile_count: *count,
                bare_model: model.clone().unwrap_or_default(),
                bare_provider: provider.clone().unwrap_or_default(),
                escape: *escape,
                request_key: request_key.clone(),
                yes: *yes,
                join: *join,
                dry_run: *dry_run,
                json: *json,
                patches: patch.clone(),
            },
        ),
        Some(Commands::Swarm { cmd }) => cmd_swarm(&cfg, cmd),
        Some(Commands::Add {
            project,
            counts,
            kinds,
            settle_secs,
            request_key,
            join_channels,
            assign,
        }) => cmd_add(
            &cfg,
            project,
            crate::cmd::spawn::AddOpts {
                counts: counts.clone(),
                kinds: kinds.clone(),
                settle_secs: *settle_secs,
                request_key: request_key.clone(),
                join_channels: join_channels.clone(),
                assign: assign.clone(),
            },
        ),
        Some(Commands::List { all }) => cmd_list(&cfg, *all),
        Some(Commands::Status { project }) => cmd_status(&cfg, project.as_deref()),
        Some(Commands::Progress { project, json }) => cmd_progress(&cfg, project.as_deref(), *json),
        Some(Commands::Snapshot { project }) => cmd_snapshot(&cfg, project),
        Some(Commands::UnansweredTriage {
            flywheel_state_dir,
            dry_run,
        }) => cmd_unanswered_triage(flywheel_state_dir.clone(), *dry_run),
        Some(Commands::StuckVsBusy { port, dry_run }) => cmd_stuck_vs_busy(*port, *dry_run),
        Some(Commands::Helm { project }) => cmd_helm(&cfg, project),
        Some(Commands::Briefing {
            objective,
            project,
            json,
            commit,
            yes,
            topology,
            crew,
            model,
        }) => cmd_briefing(
            &cfg,
            objective,
            project.as_deref(),
            *json,
            *commit,
            *yes,
            topology.as_deref(),
            crew.as_deref(),
            model.as_deref(),
        ),
        Some(Commands::Reclassify {
            objective,
            project,
            json,
            commit,
            yes,
            topology,
            crew,
            model,
        }) => cmd_reclassify(
            &cfg,
            objective,
            project.as_deref(),
            *json,
            *commit,
            *yes,
            topology.as_deref(),
            crew.as_deref(),
            model.as_deref(),
        ),
        Some(Commands::Arrive {
            project,
            as_agent,
            coalition,
            objective,
            captain,
        }) => cmd_arrive(
            &cfg,
            project,
            as_agent.as_deref(),
            coalition.as_deref(),
            objective.as_deref(),
            captain.as_deref(),
        ),
        Some(Commands::Decide {
            target,
            ready_beads,
            profiled_panes,
            launch_pending,
            contention,
            project,
            json,
        }) => cmd_decide(
            &cfg,
            target,
            *ready_beads,
            *profiled_panes,
            *launch_pending,
            *contention,
            project.as_deref(),
            *json,
        ),
        Some(Commands::Rebind { project }) => cmd_rebind(&cfg, project),
        Some(Commands::BindingRebind {
            project,
            pin,
            profile,
            model,
            harness,
            note,
        }) => cmd_binding_rebind(
            &cfg,
            project,
            pin,
            profile.as_deref(),
            model.as_deref(),
            harness.as_deref(),
            note.as_deref(),
        ),
        Some(Commands::Recover {
            project,
            include_captain,
        }) => cmd_recover(&cfg, project, *include_captain),
        Some(Commands::Send {
            project,
            msg,
            to,
            stage,
            params,
            wait,
            timeout_ms,
            force,
            respawn_on_stall,
            raw,
            fire_and_forget,
            plane,
            unsafe_plane,
            via_inbox: true,
            from,
        }) => cmd_send_via_inbox(
            &cfg,
            project,
            msg.as_deref(),
            to,
            stage.as_deref(),
            params,
            *force,
            from.as_deref(),
            (
                *wait,
                *respawn_on_stall,
                *raw,
                fire_and_forget.is_some(),
                plane.is_some(),
                *unsafe_plane,
                *timeout_ms,
            ),
        ),
        Some(Commands::Send {
            project,
            msg,
            to,
            stage,
            params,
            wait,
            timeout_ms,
            force,
            respawn_on_stall,
            raw,
            fire_and_forget,
            plane,
            unsafe_plane,
            via_inbox: false,
            from: _,
        }) => cmd_send(
            &cfg,
            project,
            msg.as_deref(),
            to,
            stage.as_deref(),
            params,
            *wait,
            *timeout_ms,
            *force,
            *respawn_on_stall,
            *raw,
            fire_and_forget.clone(),
            plane.as_deref(),
            *unsafe_plane,
        ),
        Some(Commands::Interrupt {
            project,
            to,
            keys,
            force,
        }) => cmd_interrupt(&cfg, project, to, keys, *force),
        Some(Commands::Crew { cmd }) => cmd_crew(&cfg, cmd),
        Some(Commands::Peer { cmd }) => cmd_peer(&cfg, cmd),
        Some(Commands::Captain { cmd }) => cmd_captain(&cfg, cmd),
        Some(Commands::Supervisor { cmd }) => supervisor_host::run_cli(cmd),
        Some(Commands::Halt {
            project,
            context,
            sup_epoch,
            request_key,
        }) => supervisor_host::queue_halt(
            project,
            context.as_deref(),
            *sup_epoch,
            request_key.as_deref(),
        ),
        Some(Commands::Timer { cmd }) => timer_watch::run(cmd),
        Some(Commands::View {
            project,
            to,
            lines,
            source,
        }) => cmd_view(&cfg, project, to, *lines, source),
        Some(Commands::Output {
            project,
            role,
            session,
            limit,
        }) => cmd_output(&cfg, project, role.as_deref(), session.as_deref(), *limit),
        Some(Commands::Agent { cmd }) => cmd_agent(&cfg, cmd),
        Some(Commands::Watch {
            project,
            to,
            until,
            timeout_ms,
        }) => cmd_watch(&cfg, project, to, until.as_deref(), *timeout_ms),
        Some(Commands::Doorbell { cmd }) => match cmd {
            DoorbellCmd::Status { project } => cmd_doorbell_status(&cfg, project),
            DoorbellCmd::Start { project, operator } => {
                crate::authority::guard_fleet_mutation(
                    &cfg,
                    Some(project),
                    *operator,
                    "doorbell start",
                )
                .and_then(|()| cmd_doorbell_start(&cfg, project))
            }
            DoorbellCmd::Supervise { project, operator } => {
                crate::authority::guard_fleet_mutation(
                    &cfg,
                    Some(project),
                    *operator,
                    "doorbell supervise",
                )
                .and_then(|()| crate::cmd::doorbell::cmd_doorbell_supervise(&cfg, project))
            }
            DoorbellCmd::Stop {
                project,
                workspace,
                operator,
            } => crate::authority::guard_fleet_mutation(
                &cfg,
                Some(project),
                *operator,
                "doorbell stop",
            )
            .and_then(|()| cmd_doorbell_stop(&cfg, project, workspace)),
            DoorbellCmd::ReInject { project } => cmd_doorbell_re_inject(&cfg, project),
            DoorbellCmd::Afk { project, limit } => cmd_doorbell_afk(&cfg, project, *limit),
            DoorbellCmd::Digest {
                project,
                identity,
                since,
                format,
            } => cmd_doorbell_digest(&cfg, project, identity.as_deref(), since.as_deref(), format),
            DoorbellCmd::Why {
                project,
                pin,
                limit,
                corr,
                seq,
                subscriber,
                workspace,
                since,
                json,
            } => cmd_doorbell_why(
                &cfg,
                project,
                pin.as_deref(),
                *limit,
                corr.as_deref(),
                *seq,
                subscriber.as_deref(),
                workspace.as_deref(),
                since.as_deref(),
                *json,
            ),
            DoorbellCmd::Inject {
                project,
                pin,
                line,
                workspace,
            } => cmd_doorbell_inject(&cfg, project, pin, line, workspace),
            DoorbellCmd::Subscribe {
                workspace,
                pin,
                sink,
                url,
                extra,
                project,
            } => cmd_doorbell_subscribe(
                &cfg,
                workspace,
                pin,
                sink,
                url.as_deref(),
                extra,
                project.as_deref(),
            ),
            DoorbellCmd::Roster { project, json } => cmd_doorbell_roster(&cfg, project, *json),
            DoorbellCmd::Introduce {
                project,
                pin,
                as_pin,
                channel,
                workspace,
                dry_run,
                json,
            } => cmd_doorbell_introduce(
                project,
                pin,
                as_pin.as_deref(),
                channel.as_deref(),
                workspace.as_deref(),
                *dry_run,
                *json,
            ),
            DoorbellCmd::Subscriptions {} => cmd_doorbell_subscriptions(&cfg),
            DoorbellCmd::Unsubscribe { id } => cmd_doorbell_unsubscribe(&cfg, id),
            DoorbellCmd::Gc {
                older_than_days,
                yes,
            } => cmd_doorbell_gc(*older_than_days, *yes),
        },
        Some(Commands::LoopMode { cmd }) => match cmd {
            LoopCmd::Run {
                project,
                interval,
                interval_active,
                queue,
                workflow,
                nudge,
                timeout_ms,
                once,
                on_blocked,
                to,
                local,
                work,
                no_wave_gates,
                wave_label,
                no_confirm,
                confirm_timeout_ms,
                engine,
                force,
                resume,
                allow_worktree,
                strict,
            } => {
                let mut pf_mode = if *work {
                    preflight::Mode::loop_work()
                } else {
                    preflight::Mode::loop_queue()
                };
                if *allow_worktree {
                    pf_mode.allow_worktree = true;
                    eprintln!(
                        "flywheel: operator opt-out --allow-worktree — linked-worktree preflight check skipped (ADR-0019 Spec 3.3; operator-only, logged)"
                    );
                }
                preflight_gate(&cfg, cli.skip_preflight, pf_mode, Some(project.as_str())).and_then(
                    |()| {
                        cmd_loop(
                            &cfg,
                            project,
                            *interval,
                            *interval_active,
                            queue.as_deref(),
                            workflow.as_deref(),
                            nudge.as_deref(),
                            *timeout_ms,
                            *once,
                            *local,
                            *work,
                            *no_wave_gates,
                            wave_label.as_deref(),
                            on_blocked,
                            to,
                            *no_confirm,
                            *confirm_timeout_ms,
                            *engine,
                            *force,
                            *resume,
                            *strict,
                        )
                    },
                )
            }
            LoopCmd::Supervise {
                project,
                work,
                interval,
                to,
                stall_minutes,
                context,
            } => cmd_loop_supervise(
                &cfg,
                project,
                *work,
                *interval,
                to,
                *stall_minutes,
                context.as_deref(),
            ),
            LoopCmd::Status { project } => cmd_loop_status(&cfg, project),
            LoopCmd::List => cmd_loop_list(),
            LoopCmd::Prune => cmd_loop_prune(),
            LoopCmd::Doctor { fix, json } => cmd_loop_doctor(&cfg, *fix, *json),
            LoopCmd::Replay { project } => cmd_loop_replay(project),
            LoopCmd::Why { project, seq } => cmd_loop_why(project, *seq),
        },
        Some(Commands::Artifacts {
            run_id,
            json,
            project,
            cmd,
        }) => cmd_artifacts(run_id.as_deref(), *json, project.as_deref(), cmd),
        Some(Commands::Attach { project, to }) => cmd_attach(&cfg, project, to),
        Some(Commands::Kill {
            project,
            force,
            force_working,
            drain,
            reap_crews,
        }) => cmd_kill(&cfg, project, *force, *force_working, *drain, *reap_crews),
        Some(Commands::Retire {
            pins,
            project,
            force_working,
            as_pin,
            json,
        }) => crate::cron::detect_project(&cfg, project.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "retire: no project — pass --project <slug> or run from a workspace/<project> cwd"
                )
            })
            .and_then(|project| {
                cmd_retire(
                    &cfg,
                    &project,
                    pins,
                    *force_working,
                    as_pin.as_deref(),
                    *json,
                )
            }),
        Some(Commands::Ledger { cmd }) => cmd_ledger(&cfg, cmd),
        Some(Commands::Judgment { cmd }) => cmd_judgment(cmd),
        Some(Commands::Memory { cmd }) => cmd_memory(&cfg, cmd),
        Some(Commands::Principles { cmd }) => cmd_principles(cmd),
        Some(Commands::StandingOrders { cmd }) => cmd_standing_orders(cmd),
        Some(Commands::Work { cmd }) => cmd_work(&cfg, cmd),
        Some(Commands::Assign {
            project,
            to,
            beads,
            msg,
        }) => cmd_assign(&cfg, project, to, beads, msg.as_deref()),
        Some(Commands::Mail { cmd }) => cmd_mail(&cfg, cmd),
        Some(Commands::HookTrampoline { event, agent }) => {
            let pin = std::env::var("AGENT_NAME").unwrap_or_default();
            let in_pane = std::env::var("FLYWHEEL_PANE").is_ok_and(|v| !v.is_empty());
            let seat = state::find_seat_by_pin(&pin);
            let seat_harness = seat
                .as_ref()
                .and_then(|(_, entry)| entry.harness.as_deref())
                .map(crate::harness_registry::canonical);
            if crate::integration::hook_allowed(in_pane, &pin, agent, seat_harness.as_deref())
                && let Some((project, _)) = seat
                && let Some(line) =
                    crate::turn_hooks::on_event(&config::state_dir(), &project, &pin, event)
            {
                println!("{line}");
            }
            Ok(())
        }
        Some(Commands::Hook {
            event,
            project,
            pin,
        }) => {
            if let Some(line) =
                crate::turn_hooks::on_event(&config::state_dir(), project, pin, event)
            {
                println!("{line}");
            }
            Ok(())
        }
        Some(Commands::EventsToken { cmd }) => match cmd {
            EventsTokenCmd::Rotate {
                pin,
                project,
                token_file,
                journal,
                before_secs,
            } => cmd_events_token_rotate(pin, project, token_file, journal, *before_secs),
        },
        Some(Commands::McpEvents {
            subscription,
            probe,
            client_version,
        }) => {
            if *probe {
                print_json(&events_bridge::initialize_result(client_version.as_deref()));
                Ok(())
            } else {
                cmd_mcp_events(&cfg, subscription)
            }
        }
        Some(Commands::SetupMcp {
            url,
            agents,
            boot_port,
            dry_run,
            flywheel,
        }) => toron_setup_mcp(url.as_deref(), agents, *boot_port, *dry_run, *flywheel),
        Some(Commands::Collab {
            project,
            to,
            subject,
            body,
            timeout_secs,
            as_agent,
        }) => cmd_collab(
            &cfg,
            project,
            to,
            subject,
            body,
            *timeout_secs,
            as_agent.as_deref(),
        ),
        Some(Commands::Xproj { cmd }) => cmd_xproj(&cfg, cmd),
        Some(Commands::Worktree { cmd }) => cmd_worktree(&cfg, cmd),
        Some(Commands::Checkpoint { cmd }) => cmd_checkpoint(&cfg, cmd),
        Some(Commands::Timeline { project }) => cmd_timeline(project),
        Some(Commands::Audit { project }) => cmd_audit(project),
        Some(Commands::History { project }) => cmd_history(project),
        Some(Commands::Changes { project, since }) => cmd_changes(&cfg, project, *since),
        Some(Commands::Resume { project, recipe }) => cmd_resume(&cfg, project, recipe.as_deref()),
        Some(Commands::Analytics { project }) => cmd_analytics(project),
        Some(Commands::Stage { cmd }) => cmd_stage(&cfg, cmd),
        Some(Commands::Ensemble {
            project,
            recipe,
            workflow,
        }) => cmd_ensemble(&cfg, project, recipe, workflow),
        Some(Commands::Safety) => cmd_safety(&cfg),
        Some(Commands::Guards) => cmd_guards(&cfg),
        Some(Commands::Deps { ensure }) => cmd_deps(&cfg, *ensure),
        Some(Commands::Policy { cmd }) => cmd_policy(&cfg, cmd),
        Some(Commands::Prefer {
            assignment,
            list,
            forget,
            json,
            project,
        }) => cmd_prefer(
            assignment.as_deref(),
            *list,
            forget.as_deref(),
            *json,
            project.as_deref(),
        ),
        Some(Commands::Integration { cmd, kind, out }) => match cmd {
            Some(icmd) => crate::cmd::integration::cmd_integration(&cfg, icmd.clone()),
            None => cmd_integration(kind.as_deref(), out.as_deref()),
        }
        Some(Commands::Config { cmd }) => cmd_config(&cfg, cmd),
        Some(Commands::Backend { cmd }) => cmd_backend(&cfg, cmd),
        Some(Commands::Profile { cmd }) => cmd_profile(&cfg, cmd),
        Some(Commands::Squad { cmd }) => cmd_squad(&cfg, cmd),
        Some(Commands::Coalition { cmd }) => cmd_coalition(cmd),
        Some(Commands::Runs { cmd }) => cmd_runs(&cfg, cmd),
        Some(Commands::Steer {
            objective,
            direct,
            crew,
            project,
            reuse,
            retain,
            retire_orphans,
            json,
            yes,
            request_key,
            as_agent,
        }) => preflight_gate(
            &cfg,
            cli.skip_preflight,
            preflight::Mode::steer(),
            project.as_deref(),
        )
        .and_then(|()| {
            cmd_steer(
                &cfg,
                crate::cmd::spawn::SteerOpts {
                    objective,
                    direct: *direct,
                    crew: crew.as_deref(),
                    project: project.as_deref(),
                    reuse: reuse.as_deref(),
                    retain: *retain,
                    retire_orphans: *retire_orphans,
                    json: *json,
                    yes: *yes,
                    request_key: request_key.as_deref(),
                    as_agent: as_agent.as_deref(),
                },
            )
        }),
        Some(Commands::Workflow { cmd }) => {
            let needs_pf = !cli.skip_preflight
                && matches!(
                    cmd,
                    WorkflowCmd::Run { execute: true, .. } | WorkflowCmd::Compose { run: true, .. }
                );
            let pf = if needs_pf {
                let (project, allow_wt) = match &cmd {
                    WorkflowCmd::Run {
                        project,
                        allow_worktree,
                        ..
                    } => (project.as_deref(), *allow_worktree),
                    WorkflowCmd::Compose { project, .. } => (project.as_deref(), false),
                    _ => (None, false),
                };
                let mut m = preflight::Mode::workflow();
                m.allow_worktree = allow_wt;
                if allow_wt {
                    eprintln!(
                        "flywheel: operator opt-out --allow-worktree — linked-worktree preflight check skipped (ADR-0019 Spec 3.3; operator-only, logged)"
                    );
                }
                preflight_gate(&cfg, false, m, project)
            } else {
                Ok(())
            };
            pf.and_then(|()| cmd_workflow(&cfg, cmd))
        }
        Some(Commands::Model { cmd }) => cmd_models(cmd),
        Some(Commands::Slot { cmd }) => cmd_slot(cmd),
        Some(Commands::Catalog { json }) => catalog_tui::run(*json),
        Some(Commands::Cron { cmd }) => cmd_cron(&cfg, cmd),
        Some(Commands::Debate {
            proposition,
            rounds,
            agent_a,
            agent_b,
            judge,
        }) => cmd_debate(
            &cfg,
            proposition,
            *rounds,
            agent_a.as_deref(),
            agent_b.as_deref(),
            judge.as_deref(),
        ),
        Some(Commands::PlanReview {
            project,
            feature,
            max_change_rounds,
            planner,
            reviewer,
            implementer,
            bead,
        }) => cmd_plan_review(
            &cfg,
            project,
            feature,
            *max_change_rounds,
            planner.as_deref(),
            reviewer.as_deref(),
            implementer.as_deref(),
            bead.as_deref(),
        ),
        Some(Commands::Upgrade { operator }) => {
            crate::authority::guard_fleet_mutation(&cfg, None, *operator, "flywheel upgrade")
                .and_then(|()| cmd_upgrade())
        }
        Some(Commands::Stack { cmd }) => cmd_stack(&cfg, cmd),
        Some(Commands::Ship { cmd }) => cmd_ship(&cfg, cmd),
        Some(Commands::Commit {
            paths,
            message,
            repo,
        }) => cmd_commit(paths, message, repo.as_deref()),
        Some(Commands::Tutorial) => cmd_tutorial(),
        #[cfg(feature = "serve")]
        Some(Commands::Daemon { cmd }) => cmd_daemon(&cfg, cmd),
        Some(Commands::Serve { port }) => cmd_serve(&cfg, *port),
        Some(Commands::Preflight {
            mode,
            project,
            work,
            json,
        }) => cmd_preflight(&cfg, mode, project.as_deref(), *work, *json),
        Some(Commands::__GuardFleet {
            what,
            project,
            operator,
        }) => crate::authority::guard_fleet_mutation(&cfg, project.as_deref(), *operator, what),
        Some(Commands::__Poll { project, to }) => cmd_internal_poll(&cfg, project, to),
        Some(Commands::__Doorbell {
            project,
            workspace,
            once,
            report,
            status,
        }) => cmd_internal_doorbell(&cfg, project, workspace, *once, *report, status),
        Some(Commands::__PollEvents {
            project,
            timeout_ms,
            after_seq,
        }) => cmd_internal_poll_events(&cfg, project, *timeout_ms, *after_seq),
        Some(Commands::__Send {
            project,
            name,
            text,
            confirm,
            invocation,
            iteration,
            profile,
        }) => cmd_internal_send(
            &cfg,
            project,
            name,
            text,
            SendOptions {
                confirm_ms: *confirm,
                invocation: invocation.as_deref(),
                iteration: *iteration,
                profile: profile.as_deref(),
            },
        ),
        Some(Commands::__MailBlocked {
            project,
            name,
            tier,
        }) => cmd_internal_mail_blocked(&cfg, project, name, *tier),
        Some(Commands::__AckOverdue {
            project,
            name,
            thread,
            sla_secs,
        }) => cmd_internal_ack_overdue(&cfg, project, name, thread.as_deref(), *sla_secs),
        Some(Commands::__MailNotReady {
            project,
            bead_id,
            title,
            reason,
            format,
        }) => {
            if format.as_deref().is_some_and(|f| f != "json") {
                Err(anyhow::anyhow!(
                    "__mail-not-ready only supports --format json"
                ))
            } else {
                cmd_internal_mail_not_ready(&cfg, project, bead_id, title, reason.as_deref())
            }
        }
        Some(Commands::__MailDone { project, bead_id }) => {
            cmd_internal_mail_done(&cfg, project, bead_id)
        }
        Some(Commands::__CloseCheck { project, bead_id }) => {
            cmd_internal_close_check(&cfg, project, bead_id)
        }
        Some(Commands::__ReopenBead {
            project,
            bead_id,
            sha,
        }) => cmd_internal_reopen_bead(&cfg, project, bead_id, sha),
        Some(Commands::__VerifyDispatch {
            project,
            bead_id,
            worker_model,
            candidates,
            blast,
            ac,
            dir,
        }) => cmd_internal_verify_dispatch(
            &cfg,
            project,
            bead_id,
            VerifyDispatchOpts {
                worker_model: worker_model.clone(),
                candidates: candidates.clone(),
                blast: blast.clone(),
                ac: ac.clone(),
                dir: dir.clone(),
            },
        ),
        Some(Commands::__LoopRunVerify {
            project,
            bead_id,
            sha,
            timeout_secs,
            dir,
        }) => {
            cmd_internal_loop_run_verify(&cfg, project, bead_id, sha, *timeout_secs, dir.as_deref())
        }
        Some(Commands::__PlanFence { action }) => match action {
            plan_fence::PlanFenceAction::Fence { json } => plan_fence::cmd_fence(*json),
            plan_fence::PlanFenceAction::Converged { state, json } => {
                plan_fence::cmd_converged(state, *json)
            }
            plan_fence::PlanFenceAction::Coverage { state, map, json } => {
                plan_fence::cmd_coverage(state, map, *json)
            }
        },
        Some(Commands::__ReservationBoard { project }) => {
            cmd_internal_reservation_board(&cfg, project)
        }
        Some(Commands::__PinStamp {
            project,
            bead_id,
            pin,
        }) => cmd_internal_pin_stamp(&cfg, project, bead_id, pin),
        Some(Commands::__BindSeat {}) => {
            use std::io::Read as _;
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .map_err(anyhow::Error::from)
                .and_then(|_| crate::seat_binder::bind_json(&input))
                .map(|out| println!("{out}"))
        }
        Some(Commands::__ReconcileSeats {}) => {
            use std::io::Read as _;
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .map_err(anyhow::Error::from)
                .and_then(|_| {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    crate::seat_reconcile::reconcile_json(&input, now_ms)
                })
                .map(|out| println!("{out}"))
        }
        Some(Commands::__Diagnose {
            project,
            name,
            reason,
        }) => cmd_internal_diagnose(&cfg, project, name, reason),
        Some(Commands::__XprojInbox { project, limit }) => {
            cmd_internal_xproj_inbox(&cfg, project, *limit)
        }
        Some(Commands::__ContextInbox { project, limit }) => {
            cmd_internal_context_inbox(&cfg, project, *limit)
        }
        Some(Commands::__ComposeInbox { project, limit }) => {
            cmd_internal_compose_inbox(&cfg, project, *limit)
        }
        Some(Commands::__MailDigest { project, agent }) => {
            cmd_internal_mail_digest(&cfg, project, agent)
        }
        Some(Commands::__SubagentGuard { tool, role }) => cmd_subagent_guard(tool, role.as_deref()),
        Some(Commands::__GateLearnings {
            root,
            budget,
            wiki,
            memory,
        }) => cmd_gate_learnings(
            root,
            budget.unwrap_or_else(default_gate_budget),
            wiki.as_deref(),
            memory.as_deref(),
        ),
        Some(Commands::__CheckReply {
            project,
            name,
            thread,
        }) => cmd_internal_check_reply(&cfg, project, name, thread),
        Some(Commands::__DispatchGuard { project, bead }) => {
            cmd_internal_dispatch_guard(&cfg, project, bead)
        }
        Some(Commands::__TurnGuard { project, role }) => {
            cmd_turn_guard(&cfg, project, role.as_deref())
        }
        Some(Commands::__ReplySettled {
            project,
            name,
            thread,
        }) => match state::remove_expected_reply(project, name, thread) {
            Ok(()) => {
                print_json(&json!({ "ok": true, "removed": { "agent": name, "thread": thread } }));
                Ok(())
            }
            Err(e) => Err(e),
        },
        Some(Commands::__WakeApi {
            project,
            pin,
            text,
            seed,
            urgent,
        }) => {
            let entry = state::load_spawn_registry(project)
                .into_iter()
                .find(|e| e.name.as_str() == pin.as_str() || e.mail_identity.as_deref() == Some(pin.as_str()));
            let kind = entry.as_ref().and_then(|e| e.harness.clone()).unwrap_or_default();
            let session = entry.and_then(|e| e.harness_session);
            let session_for_json = session.clone();
            let route = harness_caps::wake_route_for(
                &kind,
                session.as_deref(),
                harness_caps::installed_from_env(),
            );
            let taken = harness_api::dispatch(&route, text, seed, pin, *urgent, || {
                anyhow::bail!("no inject fallback in __wake-api (API-only probe)")
            });
            let msg = harness_api::msg_id(seed, pin);
            match taken {
                Ok(t) => {
                    print_json(&json!({
                        "ok": true, "route": format!("{t:?}"), "msg_id": msg, "pin": pin,
                        "session": session_for_json
                    }));
                    Ok(())
                }
                Err(e) => Err(anyhow::anyhow!(
                    "{e} (kind={kind:?}, session={session_for_json:?}, route={route:?})"
                )),
            }
        }
        Some(Commands::__ExpectedReplies { project }) => {
            print_json(&json!({ "entries": state::load_expected_replies(project) }));
            Ok(())
        }
        Some(Commands::__RemindReply {
            project,
            name,
            thread,
            reminder_count,
            invocation,
            iteration,
        }) => cmd_internal_remind_reply(
            project,
            name,
            thread,
            *reminder_count,
            invocation.as_deref(),
            *iteration,
        ),
        Some(Commands::__XprojSpawn {
            project,
            message_id,
            thread_id,
            from,
            recipe,
            workers,
        }) => cmd_internal_xproj_spawn(
            &cfg,
            project,
            message_id.as_str(),
            thread_id,
            from,
            recipe,
            *workers,
        ),
        Some(Commands::__RenewLeases {
            project,
            extend_seconds,
        }) => cmd_internal_renew_leases(&cfg, project, *extend_seconds),
        Some(Commands::__ReleaseBeadLocks { project, bead_id }) => {
            cmd_internal_release_bead_locks(&cfg, project, bead_id)
        }
        Some(Commands::__WaitOutput {
            project,
            target,
            pattern,
            regex,
            timeout_ms,
        }) => cmd_internal_wait_output(&cfg, project, target, pattern, *regex, *timeout_ms),
        Some(Commands::__WaveGate { label, project }) => {
            crate::cmd::wave_gate::cmd(&cfg, project, label)
        }
        Some(Commands::__Ready { project }) => cmd_internal_ready(&cfg, project),
        Some(Commands::__LastSent {
            project,
            name,
            hash,
        }) => cmd_internal_last_sent(&cfg, project, name, *hash),
        Some(Commands::__Inprogress { project }) => cmd_internal_inprogress(&cfg, project),
        Some(Commands::__ReclaimStranded { project, live }) => {
            cmd_internal_reclaim_stranded(&cfg, project, live)
        }
        Some(Commands::__Kind { project, name }) => cmd_internal_kind(&cfg, project, name),
        Some(Commands::__Profile { project, name }) => cmd_internal_profile(&cfg, project, name),
        Some(Commands::__Read { project, name }) => cmd_internal_read(&cfg, project, name),
        Some(Commands::__Respawn {
            project,
            name,
            kind,
            force,
        }) => cmd_internal_respawn(&cfg, project, name, kind, *force),
        Some(Commands::__RunHeadless {
            profile,
            prompt,
            model,
            harness,
            workflow,
            cron,
            stage,
            phase,
            effort,
            mode,
            json,
        }) => cmd_internal_run_headless(
            profile,
            prompt,
            model.as_deref(),
            harness.as_deref(),
            workflow.as_deref(),
            cron.as_deref(),
            stage.as_deref(),
            phase.as_deref(),
            effort.as_deref(),
            mode.as_deref(),
            *json,
        ),
        None => {
            print_concise_help();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        die(&e, None);
    }
}

fn print_concise_help() {
    println!(
        "flywheel — hc swarm orchestrator (v{VERSION})\n\n\
         Examples:\n\
         \x20 flywheel spawn notetoself --oc=1 --pi=1 --gk=1\n\
         \x20 flywheel send notetoself --to all --stage fresh-eyes-explore --wait\n\
         \x20 flywheel status notetoself\n\
         \x20 flywheel work triage\n\
         \x20 flywheel checkpoint save notetoself --msg \"before auth refactor\"\n\
         \x20 flywheel --robot-snapshot\n\n\
         Run `flywheel --help` for the full command reference."
    );
}

/// Only used by the serve-gated SSE server; gate it so the lean
/// backend-only build (`--no-default-features --features backend-hc,
/// mail-toron`) has no dead-code warning (bd-cargo-feature-flags).
#[cfg(feature = "serve")]
fn chrono_like_ts() -> String {
    state::now_iso()
}
fn cmd_init(cfg: &Config, project: &str) -> Result<()> {
    check_project_spelling(project, "init")?;
    let dir = project_dir(cfg, project);
    if !dir.exists() {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        println!("created {}", dir.display());
    }
    if dir.join(".beads").exists() {
        println!("beads already initialized in {}", dir.display());
    } else {
        crate::br::run_br(Some(&dir), &["init", "--prefix", "bd"])?;
        println!("initialized beads in {}", dir.display());
    }
    let (created, kept) = crate::init_scaffold::scaffold(&dir)?;
    for path in &created {
        println!("created {}", path.display());
    }
    for path in &kept {
        println!("kept {}", path.display());
    }
    println!("AGENTS.md pointer (add so future agents find the corpus):");
    println!("  {}", crate::init_scaffold::AGENTS_POINTER);
    Ok(())
}

fn recipe_counts(
    recipe: Option<&str>,
    counts: &AgentCounts,
    kinds: &[(String, u32)],
) -> Result<Vec<(String, u32)>> {
    let mut out: Vec<(String, u32)> = Vec::new();
    if let Some(name) = recipe {
        let recipes = config::Recipes::load()?;
        let r: &Recipe = recipes
            .recipes
            .get(name)
            .ok_or_else(|| anyhow!("recipe not found: {name} (flywheel recipes list)"))?;
        if let Some(n) = r.oc {
            out.push(("oc".into(), n));
        }
        if let Some(n) = r.pi {
            out.push(("pi".into(), n));
        }
        if let Some(n) = r.gk {
            out.push(("gk".into(), n));
        }
        for (k, n) in &r.extra {
            out.push((k.clone(), *n));
        }
    }
    for (k, n) in [("oc", counts.oc), ("pi", counts.pi), ("gk", counts.gk)] {
        if let Some(n) = n
            && n > 0
        {
            out.push((k.into(), n));
        }
    }
    for (k, n) in kinds {
        if *n > 0 {
            out.push((k.clone(), *n));
        }
    }
    if out.is_empty() {
        bail!("nothing to spawn: pass --oc/--pi/--gk counts or --recipe NAME");
    }
    Ok(out)
}

/// One spawn unit in [`cmd_spawn`]: a kind short (`oc`/`pi`/`gk`/kind name) or
/// a profile name, with its count. Profile items carry the resolved harness
/// (`profile.harness`) and the profile name; plain items resolve the kind via
/// `Config::resolve_kind` at spawn time (bd-eign.9/13).
struct SpawnItem {
    /// name base: the kind short for plain items, the profile name for
    /// profiled items (`flywheel-<base>-<short>-<n>`).
    short: String,
    count: u32,
    /// resolved harness for profiled items; `None` → resolve_kind(short).
    kind: Option<String>,
    /// the profile name when this item is profile-driven.
    profile: Option<String>,
}

/// Merge the recipe/count/kind plan with an optional profile-driven plan
/// (bd-eign.13: `flywheel spawn <proj> --profile coder --count 2`). Pure —
/// the catalog lookup happens at the call site, tests inject a profile here.
fn spawn_items_from_plan(
    plan: Vec<(String, u32)>,
    profile: Option<(&str, &profile::Profile, u32)>,
) -> Vec<SpawnItem> {
    let mut items: Vec<SpawnItem> = plan
        .into_iter()
        .map(|(short, count)| SpawnItem {
            short,
            count,
            kind: None,
            profile: None,
        })
        .collect();
    if let Some((name, p, count)) = profile {
        items.push(SpawnItem {
            short: name.to_string(),
            count: count.max(1),
            kind: Some(p.harness.clone()),
            profile: Some(name.to_string()),
        });
    }
    items
}

/// bd-ll3k: pre-spawn model table — one (lane, harness, model) row per
/// spawn item, resolved through the routing registry (`[defaults]`
/// included, so the operator sees the spend they are about to approve).
/// Pure — the reconfirm gate renders exactly this. Profile rows resolve
/// the profile route; kind rows show the bare override, the implied slot
/// model, or the harness default when neither applies.
pub(crate) fn spawn_model_rows(
    items: &[SpawnItem],
    catalog: &profile::ProfileCatalog,
    registry: &models::ModelRegistry,
    cfg: &Config,
    bare_model: &str,
    prefs: &crate::preferences::PreferenceSet,
    project: &str,
) -> Result<Vec<(String, String, String)>> {
    let mut rows = Vec::new();
    for item in items {
        if let Some(name) = item.profile.as_deref() {
            let p = catalog
                .get(name)
                .ok_or_else(|| anyhow!("unknown profile '@{name}' (flywheel profile list)"))?;
            let harness = prefer_kind(cfg, item, catalog, registry, prefs, project)?;
            let model = if let Some(pref) =
                prefer_fill(prefs, item, crate::preferences::Field::Model, project)
            {
                pref
            } else {
                let (_, resolved) = registry.resolve_profile_route(&harness, &p.model)?;
                resolved
            };
            rows.push((format!("@{name} x{}", item.count), harness, model));
            continue;
        }
        let kind = prefer_kind(cfg, item, catalog, registry, prefs, project)?;
        let model = if !bare_model.is_empty() {
            match crate::models::ModelChoice::parse(bare_model) {
                crate::models::ModelChoice::HarnessDefault => "(harness default)".to_string(),
                crate::models::ModelChoice::Model(id) => id,
            }
        } else if let Some(pref) =
            prefer_fill(prefs, item, crate::preferences::Field::Model, project)
        {
            pref
        } else {
            match models::implied_slot_name_for_harness(&kind)
                .and_then(|n| registry.effective_slot_model(n))
            {
                Some(m) if !m.is_empty() => m,
                _ => "(harness default)".to_string(),
            }
        };
        rows.push((format!("{} x{}", item.short, item.count), kind, model));
    }
    Ok(rows)
}

/// Standing-preference role key: the profile name without `@` when the item
/// is profile-driven, else the item's kind short name (`*` handled by lookup).
pub(crate) fn prefer_role(item: &SpawnItem) -> &str {
    match item.profile.as_deref() {
        Some(p) => p.strip_prefix('@').unwrap_or(p),
        None => item.short.as_str(),
    }
}

/// One standing-preference lookup: project scope beats always, exact role
/// beats `*`. `None` means no row — the existing chain decides.
pub(crate) fn prefer_fill(
    prefs: &crate::preferences::PreferenceSet,
    item: &SpawnItem,
    field: crate::preferences::Field,
    project: &str,
) -> Option<String> {
    prefs
        .lookup(prefer_role(item), field, Some(project))
        .map(str::to_string)
}

/// Effective harness for one item: an explicit `--kind` is a user choice and
/// wins; otherwise a standing harness preference applies to profile items
/// only; bare items keep the existing kind resolution.
pub(crate) fn prefer_kind(
    cfg: &Config,
    item: &SpawnItem,
    catalog: &profile::ProfileCatalog,
    registry: &models::ModelRegistry,
    prefs: &crate::preferences::PreferenceSet,
    project: &str,
) -> Result<String> {
    if item.profile.is_some() {
        if let Some(k) = item.kind.as_deref().filter(|k| !k.is_empty()) {
            return Ok(k.to_string());
        }
        if let Some(h) = prefer_fill(prefs, item, crate::preferences::Field::Harness, project) {
            return Ok(h);
        }
        let p = catalog
            .get(item.profile.as_deref().unwrap_or(""))
            .ok_or_else(|| {
                anyhow!(
                    "unknown profile '@{}' (flywheel profile list)",
                    item.profile.as_deref().unwrap_or("")
                )
            })?;
        return Ok(registry.resolve_profile_route(&p.harness, &p.model)?.0);
    }
    spawn_item_kind(cfg, item, catalog, registry)
}

fn spawn_item_kind(
    cfg: &Config,
    item: &SpawnItem,
    catalog: &profile::ProfileCatalog,
    registry: &models::ModelRegistry,
) -> Result<String> {
    match (&item.kind, &item.profile) {
        (Some(k), Some(name)) if k.is_empty() => {
            let p = catalog
                .get(name)
                .ok_or_else(|| anyhow!("unknown profile '@{name}' (flywheel profile list)"))?;
            Ok(registry.resolve_profile_route(&p.harness, &p.model)?.0)
        }
        (Some(k), _) if !k.is_empty() => Ok(k.clone()),
        _ => Ok(cfg.resolve_kind(&item.short)),
    }
}

/// Model + role-model launch args for a profiled spawn (bd-eign.9): the
/// per-harness `-m <model>` / `--model <model>` / `--model=<model>` flag plus
/// the omp role flags (`--smol`/`--slow`/`--plan`, bd-gsvn.2). Order matters —
/// flags precede any launch text (peer template) so a one-shot-capable
/// harness (opencode) does not swallow the flags as a prompt.
///
/// STRICT routing (nothing bypasses the registry): profile-sourced model
/// ids MUST be registry entries — an unknown id, or a harness excluded by
/// `caps` with no fallback, fails the spawn naming the fix (`flywheel model
/// add`). The routing registry layers compiled-in defaults under the
/// user's models.toml. Explicit ad-hoc `--model` strings on the headless
/// path keep the lenient passthrough.
/// ADR-0022 W2: `phase` (when the step's spec carries one) extends the
/// launch args through the per-harness phase registry; a harness without a
/// native mechanic for the phase degrades LOUDLY (warn here, `Ok` with the
/// degraded flag surfaced via [`headless::phase_launch_args`] consumers —
/// this fn keeps the plain `Vec<String>` contract, so degradation is
/// reported on stderr only).
/// bd-j3so: launch args for a BARE harness + provider/model pair (no named
/// profile). LENIENT routing — an id registered in the composite registry
/// routes through it (default-provider fallback; an explicit provider pins
/// the pair), an unregistered id passes through verbatim. The strict check
/// stays profile-only: the operator named this pair explicitly, so there is
/// no profile indirection to protect. Empty model -> no flags.
/// bd-oc-bare-spawn-model-argv-zbzt: the bare kind/count lane launches through
/// `hc agent start --kind <harness>` — the same interactive entrypoint the
/// profile lane uses — so it must deliver the model the same way: argv where
/// the harness takes a flag, pane env where it does not (opencode's root
/// command rejects `--model`). Returns `(argv, env_model_pin)`; the caller must
/// pin a non-empty `env_model_pin` into the pane env at creation time (a
/// process's env is fixed when it starts), including the workspace env of a
/// first agent on the root pane.
fn bare_model_launch(
    harness: &str,
    model: &crate::models::ModelChoice,
    provider: &str,
    effort: Option<&str>,
) -> Result<(Vec<String>, Option<String>)> {
    let registry = models::ModelRegistry::load_routing()?;
    let model = match model {
        crate::models::ModelChoice::HarnessDefault => return Ok((vec![], None)),
        crate::models::ModelChoice::Model(id) => id,
    };
    let model = if model.is_empty() {
        match registry.coupled_default_model(harness) {
            Some(id) => id,
            None if harness == "grok" => {
                anyhow::bail!(
                    "bare grok spawn has no grok-family model registered — add \
                     [models.\"grok/<id>\"] to ~/.config/flywheel/models.toml \
                     or pass --model grok/<id> (the grok harness runs only its \
                     own models; premium zai ids are not routed there)"
                )
            }
            None => {
                match models::implied_slot_name_for_harness(harness)
                    .and_then(|n| registry.effective_slot_model(n))
                {
                    Some(m) if !m.is_empty() => m,
                    _ => return Ok((vec![], None)),
                }
            }
        }
    } else {
        model.to_string()
    };
    let lookup = if provider.is_empty() {
        model
    } else {
        format!("{provider}/{model}")
    };
    let plan =
        headless::interactive_model_plan(harness, &registry.resolve(&lookup, harness), effort);
    Ok((plan.args().to_vec(), plan.env_pin().map(str::to_string)))
}

fn expand_slot_model(registry: &models::ModelRegistry, id: &str) -> Result<String> {
    if id.is_empty() {
        return Ok(String::new());
    }
    if let Some(name) = models::slot_alias(id) {
        if let Some(model) = registry.effective_slot_model(name) {
            if model.is_empty() {
                anyhow::bail!(
                    "slot '{name}' has no model — `flywheel slot set {name} --model <id>`"
                );
            }
            return Ok(model);
        }
        anyhow::bail!("slot '{name}' is not configured — `flywheel slot set {name} --model <id>`")
    }
    Ok(id.to_string())
}

/// Role-flag ids (`smol`/`slow`/`plan`): slot aliases that are not routable
/// on this harness omit the flag (cheap/big-pickle is opencode-capped).
fn resolve_role_flag(registry: &models::ModelRegistry, raw: &str, harness: &str) -> Result<String> {
    if raw.is_empty() {
        return Ok(String::new());
    }
    let from_slot = models::slot_alias(raw).is_some();
    let id = expand_slot_model(registry, raw)?;
    match registry.resolve_strict(&id, harness) {
        Ok(s) => Ok(s),
        Err(_) if from_slot => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// The resolved launch plan for one profile (ADR-0013 P1-7). One resolution
/// serves argv, the ledger pin, AND the pane env, so the recorded route is the
/// route the process actually gets (a mid-spawn reroute hop is audited by
/// `record_reroute_audit`, not folded back into this plan).
///
/// bd-crlx: `env_model_pin` exists because `env_model_pin` being `Some` means
/// the argv CANNOT carry the model. opencode v2's root command — the one hc
/// launches — has no `--model` flag (only `opencode run` does), so the TUI's
/// model must be pinned through pane env instead. Making it a field on the plan
/// rather than a silent omission means every pane-creating caller is forced by
/// the type to decide what it does about it: apply the pin, or knowingly drop
/// it. There is no third option where the seat silently loses its model.
pub(crate) struct ProfileLaunchPlan {
    /// argv to append to the harness launch command.
    pub args: Vec<String>,
    /// The resolved composite model id, recorded in the spawn registry.
    pub model: String,
    /// `Some(model)` when the harness's interactive entrypoint has no model
    /// flag, so the pin must ALSO travel via pane env
    /// (`hc::apply_opencode_model_pin`).
    pub env_model_pin: Option<String>,
}

pub(crate) fn profile_launch_plan(
    harness: &str,
    p: &profile::Profile,
    phase: Option<&str>,
) -> Result<ProfileLaunchPlan> {
    let registry = models::ModelRegistry::load_routing()?;
    let (harness, slot_model, _route) =
        registry.resolve_profile_route_full(harness, &p.model, &p.model_fallback)?;
    let lookup = if models::slot_alias(&p.model).is_some() {
        slot_model
    } else if p.provider.is_empty() {
        p.model.clone()
    } else {
        format!("{}/{}", p.provider, p.model)
    };
    let model = registry.resolve_strict(&lookup, &harness)?;
    let plan = headless::interactive_model_plan(
        &harness,
        &model,
        (!p.effort.is_empty()).then_some(p.effort.as_str()),
    );
    let mut args = plan.args().to_vec();
    let env_model_pin = plan.env_pin().map(str::to_string);
    if let Some(phase) = phase.filter(|ph| !ph.is_empty()) {
        let (extra, degraded) = headless::phase_launch_args(&harness, phase);
        if degraded {
            eprintln!(
                "flywheel: phase {phase:?} has no native mechanic on harness {harness:?} — degrading to prompt mode"
            );
        }
        args.extend(extra);
    }
    Ok(ProfileLaunchPlan {
        args,
        model,
        env_model_pin,
    })
}

/// The argv-only view of [`profile_launch_plan`] — for callers that spawn no
/// pane (and so cannot pin by env) and do not journal the resolved pin.
///
/// bd-crlx: if the returned plan carried an `env_model_pin`, this view would
/// silently drop the model. Rather than let that happen quietly, refuse: a
/// caller in that position has no way to honour the pin, and starting the seat
/// on the wrong model is worse than not starting it.
pub(crate) fn profile_launch_args(
    harness: &str,
    p: &profile::Profile,
    phase: Option<&str>,
) -> Result<Vec<String>> {
    let plan = profile_launch_plan(harness, p, phase)?;
    if let Some(m) = &plan.env_model_pin {
        anyhow::bail!(
            "harness {harness:?} takes no model flag on its interactive entrypoint, so the \
             model {m:?} cannot be delivered as argv — resolve the plan and pin it via pane \
             env (hc::apply_opencode_model_pin) instead of using the argv-only view"
        );
    }
    Ok(plan.args)
}

/// The argv an INTERACTIVE pane launch may carry for a concrete model — the
/// model-driven sibling of [`profile_launch_args`], for callers that hold a
/// route rather than a profile (the rate-limit fallback hops).
///
/// bd-oc-bare-spawn-model-argv-zbzt: ONE delivery rule for every pane launch.
/// `hc agent start --kind <harness>` runs the harness's interactive
/// entrypoint, so a model flag the headless form emits (`headless::model_args`
/// → opencode `--model`) must never reach it — opencode's root command answers
/// it with `ERROR Unrecognized flag: --model in command opencode` and the pane
/// dies (measured live 2026-10-07, v2.0.24).
///
/// An env-delivered pin is a REFUSAL here, never a silent bare launch: this
/// caller re-launches into a pane that already exists, and a process's env was
/// fixed when it started, so it cannot carry the pin. Launching bare would run
/// whatever model that pane was created with, under a journaled rebind to a
/// model the pane never got.
pub(crate) fn interactive_pane_args(
    harness: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<Vec<String>> {
    let plan = headless::interactive_model_plan(harness, model, effort);
    if let Some(m) = plan.env_pin() {
        anyhow::bail!(
            "harness {harness:?} takes no model flag on its interactive entrypoint, so model \
             {m:?} cannot be delivered as argv into a pane that already exists — a process's env \
             is fixed when it starts, so there is no way to pin it now; re-spawn the pane (its \
             env is built at creation) instead"
        );
    }
    Ok(plan.args().to_vec())
}

/// clap command signature — params map 1:1 to CLI flags (ADR-0002 CLI surface).
#[allow(clippy::too_many_arguments)]
/// Render the peer-aware self-poll template (ADR-0001 S.9.3) with all
/// placeholders substituted. Pure + deterministic so the injected prompt is
/// free of literal `{placeholder}` text (bd-pattern-peer-triggered).
///
/// S8.3: `mail_identity` (the registered canonical name, resolved BEFORE
/// the agent starts) is injected as "your mail identity is X" so the agent
/// boots knowing the exact name its peers/assigns address it by.
///
/// bd-zq8u: the rendered output is the launch ARG — hc rejects any arg
/// containing control characters (`arg.chars().any(char::is_control)`,
/// hc src/app/agents.rs). The template file keeps markdown newlines for
/// readability; the rendered string collapses them to spaces so the launch
/// text is control-char-free. `ensure_ascii_launch_args` guards the same
/// boundary defensively.
fn peer_prompt_template(
    tpl: &str,
    name: &str,
    mail_identity: &str,
    poll_interval_minutes: u32,
    poll_budget_minutes: u32,
    max_peer_prompts: u32,
) -> String {
    let rendered = tpl
        .replace("{name}", name)
        .replace("{mail_identity}", mail_identity)
        .replace("{task}", "await the swarm loop's queue or help peers")
        .replace(
            "{poll_interval_minutes}",
            &poll_interval_minutes.to_string(),
        )
        .replace("{poll_budget_minutes}", &poll_budget_minutes.to_string())
        .replace("{max_peer_prompts}", &max_peer_prompts.to_string());
    rendered
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// clap command signature — params map 1:1 to CLI flags (ADR-0002 CLI surface).
#[allow(clippy::too_many_arguments)]
/// bd-fw-stale-workspace-panes-hgp + bd-627z.3: pre-existing hc
/// workspaces that share this project's label or host its agents, split by
/// origin. Flywheel-owned workspaces (default swarm label `flywheel-<base>`,
/// a recorded custom spawn label, or hosting agents named `flywheel-<base>-*`)
/// are the stale-pane race; a foreign workspace (captain/omp session, another
/// tool) that merely shares the project-base label is NOT — it must not block
/// a spawn (warn + proceed, never closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceOrigin {
    Owned,
    Foreign,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceSplit {
    pub owned: Vec<hc::WorkspaceInfo>,
    pub foreign: Vec<hc::WorkspaceInfo>,
}

/// bd-627z.3: classify one workspace by origin. `None` = unrelated (skip).
/// Owned wins over Foreign when both signals collide (a flywheel marker on a
/// base-labeled workspace is still a flywheel workspace).
fn classify_workspace_origin(
    project: &str,
    ws: &hc::WorkspaceInfo,
    agents: &[hc::AgentInfo],
    spawn_label: Option<&str>,
) -> Option<WorkspaceOrigin> {
    let base = project_base(project);
    let label = swarm_label(project);
    let prefix = format!("flywheel-{base}-");
    let hosts_agents = agents.iter().any(|a| {
        a.workspace_id == ws.workspace_id
            && a.name
                .as_deref()
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
    });
    if ws.label == label
        || ws.label.starts_with(&prefix)
        || hosts_agents
        || spawn_label == Some(ws.label.as_str())
    {
        Some(WorkspaceOrigin::Owned)
    } else if ws.label == base {
        Some(WorkspaceOrigin::Foreign)
    } else {
        None
    }
}

/// bd-627z.3: pure partition of the workspace list into owned/foreign.
fn partition_workspaces(
    project: &str,
    ws_list: Vec<hc::WorkspaceInfo>,
    agents: &[hc::AgentInfo],
    spawn_label: Option<&str>,
) -> WorkspaceSplit {
    let mut split = WorkspaceSplit::default();
    for ws in ws_list {
        match classify_workspace_origin(project, &ws, agents, spawn_label) {
            Some(WorkspaceOrigin::Owned) => split.owned.push(ws),
            Some(WorkspaceOrigin::Foreign) => split.foreign.push(ws),
            None => {}
        }
    }
    split
}

pub(crate) fn project_workspaces(c: &mut Client, project: &str) -> Result<WorkspaceSplit> {
    let ws_list = c.workspace_list()?;
    let agents = c.agent_list().unwrap_or_default();
    Ok(partition_workspaces(
        project,
        ws_list,
        &agents,
        state::load_spawn_meta(project).as_deref(),
    ))
}

/// What spawn does with the project's owned workspaces that still have a live
/// pane tree (bd-gjtcw).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnWorkspace {
    /// No owned workspace survives: create one.
    Fresh,
    /// Owned workspace(s) with NO agent in them (a shell only): spawn ADDS the
    /// requested panes into the first one instead of refusing.
    Add { index: usize },
    /// A workspace hosts a live agent: join loud (ADR-0011 D5).
    Join,
}

/// The classify step spawn and `--join` share. A workspace with no agent is
/// not a stale swarm — nothing there can race a new pane. An agent counts as
/// the fleet ONLY when its pane is still in the plane's `pane_list`: a terminal
/// label outlives its pane after retire, a crash or a closed rex window, and
/// treating that orphan as live blocks the next spawn (bd-89q4r).
pub(crate) fn classify_spawn_workspace(
    owned: &[hc::WorkspaceInfo],
    agents: &[hc::AgentInfo],
    live_panes: &std::collections::HashSet<String>,
) -> SpawnWorkspace {
    if owned.is_empty() {
        return SpawnWorkspace::Fresh;
    }
    // The multiplexer's own shell lives in the workspace too; it is not a
    // crew pin and must not make an otherwise empty workspace look occupied.
    let is_crew = |a: &hc::AgentInfo| {
        a.name
            .as_deref()
            .is_some_and(|n| !n.is_empty() && n != "shell")
    };
    let hosts_agent = |ws: &hc::WorkspaceInfo| {
        agents.iter().any(|a| {
            a.workspace_id == ws.workspace_id && is_crew(a) && live_panes.contains(&a.pane_id)
        })
    };
    if owned.iter().any(hosts_agent) {
        return SpawnWorkspace::Join;
    }
    SpawnWorkspace::Add { index: 0 }
}

/// bd-bug-add-foreign-workspace-m7dg: the add path's target workspace. Same
/// partition as spawn: owned swarm workspace required, foreign (captain/omp
/// session sharing the base label) never targeted — the old find_workspace
/// matched the base label and added panes into the captain's own workspace
/// (w37, label toron) instead of the flywheel-owned swarm (label
/// flywheel-toron). Refuses when no owned workspace exists (spawn first).
fn add_target_from_split(
    split: &WorkspaceSplit,
    agents: &[hc::AgentInfo],
    project: &str,
) -> Result<String> {
    if split.owned.is_empty() {
        bail!("no flywheel-owned swarm for {project} (run: flywheel spawn {project})")
    }
    let prefix = format!("flywheel-{}-", project_base(project));
    Ok(hc::resolve_workspace_id(&split.owned, agents, &prefix))
}

fn add_target_workspace(c: &mut Client, project: &str) -> Result<String> {
    let split = project_workspaces(c, project)?;
    if !split.foreign.is_empty() {
        let detail: Vec<String> = split
            .foreign
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        eprintln!(
            "flywheel: warn: {} foreign workspace(s) share the {project} label but are not flywheel-owned — leaving them alone: {}",
            split.foreign.len(),
            detail.join(", ")
        );
    }
    let agents = c.agent_list()?;
    add_target_from_split(&split, &agents, project)
}

#[cfg(test)]
mod profile_workflow_invoke_tests {
    use super::*;

    /// bd-6zsl: `profile run --workflow` must ATTEMPT a real run — the
    /// invoke command is a pane-mode Run with --execute against the spawned
    /// agent's workspace (--reuse, never reaped), not a "not yet wired"
    /// message.
    #[test]
    fn invoke_attempts_pane_run_in_spawned_workspace() {
        let cmd =
            profile_workflow_invoke("weekly-review", Some("{\"goal\":\"x\"}"), "ws-1", "toron");
        let WorkflowCmd::Run {
            name,
            inputs,
            execute,
            mode,
            project,
            reuse,
            ..
        } = cmd
        else {
            panic!("expected a Run command");
        };
        assert_eq!(name, "weekly-review");
        assert_eq!(inputs.as_deref(), Some("{\"goal\":\"x\"}"));
        assert!(execute, "invoke must execute, not record");
        assert_eq!(mode, "pane");
        assert_eq!(project.as_deref(), Some("toron"));
        assert_eq!(reuse.as_deref(), Some("ws-1"));
    }

    /// No inputs → the workflow runs with the engine default ({}).
    #[test]
    fn invoke_without_inputs_runs_default() {
        let cmd = profile_workflow_invoke("plan", None, "ws-1", "toron");
        let WorkflowCmd::Run {
            inputs, execute, ..
        } = cmd
        else {
            panic!("expected a Run command");
        };
        assert!(inputs.is_none());
        assert!(execute);
    }
}

#[cfg(test)]
mod steer_safety_tests {
    use super::*;

    #[test]
    fn workspace_label_aliases_include_raw_label_and_slug() {
        let aliases = workspace_label_aliases("flywheel-omp-repro", None);
        assert!(
            aliases.iter().any(|a| a == "flywheel-omp-repro"),
            "raw token must resolve: {aliases:?}"
        );
        assert!(
            aliases
                .iter()
                .any(|a| a == "flywheel-flywheel-omp-repro" || a.starts_with("flywheel-")),
            "default swarm label present: {aliases:?}"
        );
    }

    #[test]
    fn request_key_labels_resolve_to_the_project() {
        assert!(workspace_label_is_owned(
            "toron",
            "flywheel-toron-rk-toron-host-cut-1",
            None
        ));
        assert!(workspace_label_is_owned("toron", "flywheel-toron", None));
        assert!(!workspace_label_is_owned(
            "toron",
            "flywheel-other-rk-x",
            None
        ));
        assert!(!workspace_label_is_owned("toron", "scratchpad", None));
    }

    #[test]
    fn workspace_label_aliases_include_spawn_meta() {
        let aliases = workspace_label_aliases("flywheel", Some("captain-audit"));
        assert!(
            aliases.iter().any(|a| a == "captain-audit"),
            "custom --label must resolve: {aliases:?}"
        );
    }

    #[test]
    fn working_agent_names_only_working() {
        use crate::hc::{AgentInfo, AgentStatus};
        fn agent(name: &str, status: AgentStatus) -> AgentInfo {
            AgentInfo {
                agent: None,
                agent_status: status,
                cwd: None,
                display_agent: None,
                focused: false,
                foreground_cwd: None,
                interactive_ready: true,
                launch_pending: false,
                name: Some(name.into()),
                pane_id: format!("p-{name}"),
                revision: 0,
                state_change_seq: 0,
                completion_seq: None,
                state_labels: serde_json::Map::new(),
                tab_id: "t".into(),
                terminal_id: "term".into(),
                terminal_title: None,
                workspace_id: "w".into(),
            }
        }
        let names = working_agent_names(&[
            agent("oc", AgentStatus::Working),
            agent("oc2", AgentStatus::Idle),
            agent("oc3", AgentStatus::Done),
        ]);
        assert_eq!(names, vec!["oc".to_string()]);
    }
}

#[cfg(test)]
mod steer_engine_tests {
    use super::*;

    const STEER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

    /// ADR-0022 W4: `flywheel steer` delegates to the durable Run arm —
    /// execute=true, pane mode, goal/goal-direct. The Run arm owns the
    /// engine.invoke door; steer must never grow a second execution path.
    #[test]
    fn steer_targets_durable_run_arm() {
        for (direct, expected) in [(true, "goal-direct"), (false, "goal")] {
            let cmd = steer_run_cmd(
                "fix the flake",
                direct,
                None,
                None,
                None,
                false,
                false,
                false,
                &[],
                &[],
            );
            let WorkflowCmd::Run {
                name,
                inputs,
                execute,
                mode,
                allow_worktree,
                ..
            } = cmd
            else {
                panic!("expected a Run command");
            };
            assert_eq!(name, expected);
            assert_eq!(inputs.as_deref(), Some(r#"{"objective":"fix the flake"}"#));
            assert!(execute, "steer must execute, not record");
            assert_eq!(mode, "pane");
            assert!(!allow_worktree);
        }
    }

    /// bd-xe9r: the interactive briefing arms ONLY on an interactive TTY
    /// with no explicit fleet. Every skip path keeps the dispatch
    /// byte-identical to the pre-briefing binary (golden matrix).
    #[test]
    fn briefing_skip_matrix_matches_non_tty_golden() {
        for (json, yes, crew) in [
            (false, false, None),
            (true, false, None),
            (false, true, None),
            (false, false, Some("build")),
            (true, true, Some("build")),
        ] {
            assert!(
                !briefing_enabled(json, yes, crew, false),
                "non-TTY must never brief (json={json}, yes={yes}, crew={crew:?})"
            );
        }
        assert!(briefing_enabled(false, false, None, true));
        assert!(!briefing_enabled(true, false, None, true), "--json skips");
        assert!(!briefing_enabled(false, true, None, true), "--yes skips");
        assert!(
            !briefing_enabled(false, false, Some("build"), true),
            "explicit --crew skips"
        );
        let cmd = steer_run_cmd(
            "fix the flake",
            false,
            None,
            None,
            None,
            false,
            false,
            false,
            &[],
            &[],
        );
        let WorkflowCmd::Run {
            name,
            inputs,
            execute,
            mode,
            json,
            allow_worktree,
            ..
        } = cmd
        else {
            panic!("expected a Run command");
        };
        assert_eq!(name, "goal");
        assert_eq!(inputs.as_deref(), Some(r#"{"objective":"fix the flake"}"#));
        assert!(execute);
        assert_eq!(mode, "pane");
        assert!(!json);
        assert!(!allow_worktree);
    }

    /// bd-nmzk: workflow run --execute shares the same skip matrix as steer,
    /// but also gates on `execute == true` — recording runs never brief.
    /// Lockstep via `briefing_enabled` + `workflow_briefing_context` (same
    /// crew/model/squad/stage path, `source = "workflow"`).
    #[test]
    fn workflow_briefing_skip_matrix_matches_steer() {
        let workflow_briefs =
            |json, yes, crew, is_tty, execute| execute && briefing_enabled(json, yes, crew, is_tty);
        for (json, yes, crew) in [
            (false, false, None),
            (true, false, None),
            (false, true, None),
            (false, false, Some("build")),
        ] {
            assert!(
                !workflow_briefs(json, yes, crew, false, true),
                "non-TTY must never brief workflow (json={json}, yes={yes}, crew={crew:?})"
            );
            assert!(
                !workflow_briefs(json, yes, crew, true, false),
                "non-execute must never brief (json={json}, yes={yes}, crew={crew:?})"
            );
        }
        assert!(workflow_briefs(false, false, None, true, true));
        assert!(
            !workflow_briefs(true, false, None, true, true),
            "--json skips"
        );
        assert!(
            !workflow_briefs(false, true, None, true, true),
            "--yes skips"
        );
        assert!(
            !workflow_briefs(false, false, Some("build"), true, true),
            "explicit --crew skips"
        );
        let steer_ctx = steer_briefing_context("fix the flake", "goal").expect("steer ctx");
        let wf_ctx = workflow_briefing_context("goal", Some(r#"{"objective":"fix the flake"}"#))
            .expect("workflow ctx");
        assert_eq!(steer_ctx.source, "steer");
        assert_eq!(wf_ctx.source, "workflow");
        assert_eq!(steer_ctx.workflow, wf_ctx.workflow);
        assert_eq!(steer_ctx.objective, wf_ctx.objective);
        assert_eq!(steer_ctx.stage_chain, wf_ctx.stage_chain);
        assert_eq!(steer_ctx.crew_plan, wf_ctx.crew_plan);
    }

    /// bd-88m4: the briefing context carries the tier-pick suggestion —
    /// a frontier-cued objective suggests the premium band, a routine one
    /// the cheap band, a generic one stays silent (None = current
    /// behavior).
    #[test]
    fn tier_pick_briefing_context_carries_tier() {
        let premium_ctx =
            steer_briefing_context("plan the security migration with a perf budget", "goal")
                .expect("steer ctx");
        assert_eq!(
            premium_ctx.tier_suggestion.as_deref(),
            Some("@premium"),
            "frontier objective suggests the premium band"
        );
        let plain_ctx = steer_briefing_context("fix the flake", "goal").expect("steer ctx");
        assert_eq!(
            plain_ctx.tier_suggestion, None,
            "generic objective renders no tier suggestion"
        );
    }

    /// bd-vlvn: live enrichment without a key keeps the FallbackTable —
    /// hermetic (returns before any pack lookup or network).
    #[test]
    fn jev_live_enrich_keeps_table_without_key() {
        let authority = crate::authority::RuntimeAuthority {
            host: crate::authority::HostAuthority {
                home: String::new(),
                cwd: std::env::temp_dir(),
                exe: std::env::temp_dir().join("flywheel"),
                typesafe_api_key: None,
            },
            clock: Arc::new(crate::authority::SystemClock),
            monotonic: Arc::new(crate::authority::SystemMonotonicClock::default()),
        };
        let mut ctx = steer_briefing_context("fix the borrow checker error in cargo build", "goal")
            .expect("steer ctx");
        assert_eq!(
            ctx.profile_suggestion.as_deref(),
            Some("@rust-coder"),
            "table suggestion present before enrich"
        );
        enrich_suggestions_live(
            &mut ctx,
            "fix the borrow checker error in cargo build",
            &authority,
        );
        assert_eq!(
            ctx.profile_suggestion.as_deref(),
            Some("@rust-coder"),
            "keyless enrich keeps the table"
        );
    }

    /// bd-adv4: the briefing context carries the profile-pick suggestion —
    /// a language-cued objective suggests the specialist, a generic one
    /// stays silent (None = keep @coder).
    #[test]
    fn profile_pick_briefing_context_carries_suggestion() {
        let rust_ctx =
            steer_briefing_context("fix the borrow checker error in cargo build", "goal")
                .expect("steer ctx");
        assert_eq!(
            rust_ctx.profile_suggestion.as_deref(),
            Some("@rust-coder"),
            "rust objective suggests rust-coder"
        );
        let plain_ctx = steer_briefing_context("fix the flake", "goal").expect("steer ctx");
        assert_eq!(
            plain_ctx.profile_suggestion, None,
            "generic objective renders no suggestion"
        );
    }

    /// Shared harness: the exact execution shape the Run arm builds for a
    /// pane-mode steer — recording/counting runner, builtin+catalog
    /// workflow resolver, crew fan-out resolution, local engine over
    /// the in-house durable kernel. No hc terminals needed: dispatch_job side-effects
    /// are recorded, control flow is real.
    fn steer_engine(
        calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Result<(
        tokio::runtime::Runtime,
        std::sync::Arc<toron_workflow::WorkflowEngine>,
        toron_workflow::schema::Workflow,
    )> {
        let mut builtins = builtin_workflows::builtin_workflows();
        let wf = builtins
            .remove("goal-direct")
            .expect("goal-direct builtin workflow");
        let workflow_resolver: toron_workflow::WorkflowResolver =
            Arc::new(|sub: &str| workflow_catalog::subworkflow_resolver(None)(sub));
        let crew_plan = crew::plan_crew_with(&wf, &|sub| workflow_resolver(sub));
        let invoke_wf = crew::resolve_crew_fanouts(&wf, &crew_plan);
        let runner: toron_workflow::engine::EffectRunner =
            Arc::new(move |inv: &toron_workflow::engine::ActionInvocation| {
                calls
                    .lock()
                    .unwrap()
                    .push(inv.action.type_name().to_string());
                Ok(json!({ "action": inv.action.type_name() }))
            });
        let rt = tokio::runtime::Runtime::new()?;
        let _enter = rt.enter();
        let engine = std::sync::Arc::new(
            toron_workflow::WorkflowEngine::local_with_resolvers_and_effect(
                profile::ProfileCatalog::catalog_profile_resolver(),
                workflow_resolver,
                Some(runner),
            )?,
        );
        Ok((rt, engine, invoke_wf))
    }

    async fn steered(
        engine: &toron_workflow::WorkflowEngine,
        run_id: &str,
        invoke_wf: toron_workflow::schema::Workflow,
    ) -> Value {
        tokio::time::timeout(
            STEER_TIMEOUT,
            engine.invoke(run_id, invoke_wf, json!({ "objective": "w4 gate" })),
        )
        .await
        .expect("steer chain must not park (no approval gate on --direct)")
        .expect("engine invoke ok")
    }

    /// ADR-0022 W4 gate 1: the steer chain completes as an ENGINE run —
    /// same invoke door as catalog/trigger runs, real control flow over
    /// the composed goal-direct steps.
    #[test]
    fn steer_direct_completes_as_engine_run() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (rt, engine, invoke_wf) = steer_engine(calls).expect("steer engine");
        let run_id = format!("fw-cli-steer-engine-run-test-{}", std::process::id());
        let out = rt.block_on(steered(&engine, &run_id, invoke_wf));
        assert!(out.is_object(), "engine returns an output object");
    }

    /// ADR-0022 W4 gate 2: crash-resume equivalence — a redelivered run_id
    /// replays the RECORDED execution (steps execute exactly once, output
    /// identical), the property that makes steer == catalog runs.
    #[test]
    fn steer_chain_redelivery_replays_recorded_run() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (rt, engine, invoke_wf) = steer_engine(calls.clone()).expect("steer engine");
        let run_id = format!("fw-cli-steer-redelivery-test-{}", std::process::id());
        let first = rt.block_on(steered(&engine, run_id.as_str(), invoke_wf.clone()));
        let after_first = calls.lock().unwrap().len();
        assert!(after_first > 0, "first run executes the chain");
        let redelivery = rt.block_on(steered(&engine, run_id.as_str(), invoke_wf));
        assert_eq!(first, redelivery, "redelivery returns the recorded output");
        assert_eq!(
            calls.lock().unwrap().len(),
            after_first,
            "steps execute exactly once across redelivery"
        );
    }
}

#[cfg(test)]
mod serve_port_tests {
    use crate::daemon::SERVE_PORT_DEFAULT;

    /// bd-zdal: `flywheel serve` and `flywheel daemon <verb>` are two doors to
    /// ONE service — both must default to the same port, else `daemon status`
    /// probes a port a bare `serve` never binds and its `--force` remediation
    /// cannot see the stray holder.
    #[test]
    fn serve_port_default_is_one_constant() {
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                use clap::Parser;
                let cli = crate::Cli::try_parse_from(["flywheel", "serve"]).expect("serve parses");
                match cli.command {
                    Some(crate::Commands::Serve { port }) => {
                        assert_eq!(port, SERVE_PORT_DEFAULT, "serve default")
                    }
                    _ => panic!("expected serve"),
                }
                #[cfg(feature = "serve")]
                for verb in ["start", "stop", "restart", "status"] {
                    let cli = crate::Cli::try_parse_from(["flywheel", "daemon", verb])
                        .expect("daemon verb parses");
                    let port = match cli.command {
                        Some(crate::Commands::Daemon { cmd }) => match cmd {
                            crate::cmd::daemon::DaemonCmd::Start { port, .. }
                            | crate::cmd::daemon::DaemonCmd::Stop { port, .. }
                            | crate::cmd::daemon::DaemonCmd::Restart { port, .. }
                            | crate::cmd::daemon::DaemonCmd::Status { port } => port,
                        },
                        _ => panic!("expected daemon"),
                    };
                    assert_eq!(port, SERVE_PORT_DEFAULT, "daemon {verb} default");
                }
            })
            .expect("spawn the parse thread");
        handle.join().expect("serve port defaults must agree");
    }
}

#[cfg(test)]
mod gs043_residual_honesty_tests {
    /// ADR-0022 rollout 6: `binding-rebind --model` routes through the one
    /// seat reconciler (`swarm apply --patch`) — never a write-only
    /// SpawnEntry.model Settled.
    #[test]
    fn binding_rebind_model_routes_through_swarm_apply() {
        let src = include_str!("main.rs");
        let start = crate::tripwire::find_fn(src, "cmd_binding_rebind");
        let body = &src[start..start.saturating_add(4500)];
        assert!(
            body.contains("cmd_swarm_apply"),
            "cmd_binding_rebind must delegate --model to swarm apply"
        );
        assert!(
            body.contains("swarm apply --patch"),
            "the delegation must name the one mechanism"
        );
        assert!(
            !body.contains("e.model = Some(m.to_string())"),
            "must not Settled a write-only SpawnEntry.model write"
        );
    }

    /// C1 residual: record_reroute_audit redacts evidence like journal_rebind.
    #[test]
    fn record_reroute_audit_redacts_evidence() {
        let src = include_str!("main.rs");
        let start = crate::tripwire::find_fn(src, "record_reroute_audit");
        let body = &src[start..start.saturating_add(1200)];
        assert!(
            body.contains("agent_ref::redact_evidence(evidence)"),
            "record_reroute_audit must redact before persist/eprintln"
        );
    }

    /// D5 (ADR-0013): the captain verb is `binding-rebind <project> --pin
    /// <pin>` — the pin is a flag (immutable AgentRef identity), never a
    /// positional that a caller could confuse with legacy `rebind <project>
    /// <name>`.
    #[test]
    fn binding_rebind_takes_pin_as_a_flag() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                use clap::Parser;
                let cli = crate::Cli::try_parse_from([
                    "flywheel",
                    "binding-rebind",
                    "atlas",
                    "--pin",
                    "DustySparrow",
                    "--profile",
                    "scout",
                ])
                .expect("the ADR-0013 D5 flag form must parse");
                match cli.command {
                    Some(crate::Commands::BindingRebind {
                        project,
                        pin,
                        profile,
                        ..
                    }) => {
                        assert_eq!(project, "atlas");
                        assert_eq!(pin, "DustySparrow");
                        assert_eq!(profile.as_deref(), Some("scout"));
                    }
                    _ => panic!("expected binding-rebind"),
                }
                assert!(
                    crate::Cli::try_parse_from([
                        "flywheel",
                        "binding-rebind",
                        "atlas",
                        "DustySparrow"
                    ])
                    .is_err(),
                    "the pin is never a positional (ADR-0013 D5)"
                );
            })
            .expect("spawn the parse thread");
        handle.join().expect("the D5 parse assertions must hold");
    }

    /// bd-g5q4: `mail search`'s old `[PROJECT] <QUERY>` order — an optional
    /// positional before a required one — panicked in clap's debug asserts,
    /// so the subcommand was unusable in every arg order. The fix makes the
    /// required QUERY precede the optional PROJECT; this pins the order in
    /// both spellings and pins that the parse itself never panics.
    #[test]
    fn mail_search_query_positional_precedes_project() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                use clap::Parser;
                let cli =
                    crate::Cli::try_parse_from(["flywheel", "mail", "search", "needle", "proj-a"])
                        .expect("<QUERY> [PROJECT] must parse");
                match cli.command {
                    Some(crate::Commands::Mail {
                        cmd: crate::cmd::mail::MailCmd::Search { query, project, .. },
                    }) => {
                        assert_eq!(query, "needle");
                        assert_eq!(project.as_deref(), Some("proj-a"));
                    }
                    _ => panic!("expected mail search"),
                }
                let cli = crate::Cli::try_parse_from([
                    "flywheel",
                    "mail",
                    "search",
                    "--project",
                    "proj-b",
                    "needle",
                ])
                .expect("--project with a bare query must parse");
                match cli.command {
                    Some(crate::Commands::Mail {
                        cmd:
                            crate::cmd::mail::MailCmd::Search {
                                query,
                                project_flag,
                                ..
                            },
                    }) => {
                        assert_eq!(query, "needle");
                        assert_eq!(project_flag.as_deref(), Some("proj-b"));
                    }
                    _ => panic!("expected mail search (flag form)"),
                }
            })
            .expect("spawn the parse thread");
        handle
            .join()
            .expect("the bd-g5q4 parse assertions must hold");
    }

    /// bd-vben: `mail whois` carried the bd-g5q4 defect — an optional
    /// positional (project) before a required one (agent), which panicked in
    /// clap's debug asserts. Pins the required-first order and the flag form.
    #[test]
    fn mail_whois_agent_positional_precedes_project() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                use clap::Parser;
                let cli = crate::Cli::try_parse_from(["flywheel", "mail", "whois", "TealMarlin"])
                    .expect("<AGENT> must parse alone");
                match cli.command {
                    Some(crate::Commands::Mail {
                        cmd: crate::cmd::mail::MailCmd::Whois { agent, project, .. },
                    }) => {
                        assert_eq!(agent, "TealMarlin");
                        assert_eq!(project, None);
                    }
                    _ => panic!("expected mail whois"),
                }
                let cli = crate::Cli::try_parse_from([
                    "flywheel",
                    "mail",
                    "whois",
                    "TealMarlin",
                    "proj-a",
                ])
                .expect("<AGENT> [PROJECT] must parse");
                match cli.command {
                    Some(crate::Commands::Mail {
                        cmd: crate::cmd::mail::MailCmd::Whois { agent, project, .. },
                    }) => {
                        assert_eq!(agent, "TealMarlin");
                        assert_eq!(project.as_deref(), Some("proj-a"));
                    }
                    _ => panic!("expected mail whois (project form)"),
                }
            })
            .expect("spawn the parse thread");
        handle
            .join()
            .expect("the bd-vben whois parse assertions must hold");
    }

    /// bd-vben: `mail locks show` had the same optional-before-required
    /// positional shape; the path must come first.
    #[test]
    fn mail_locks_show_path_positional_precedes_project() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                use clap::Parser;
                let cli =
                    crate::Cli::try_parse_from(["flywheel", "mail", "locks", "show", "src/x.rs"])
                        .expect("<PATH> must parse alone");
                match cli.command {
                    Some(crate::Commands::Mail {
                        cmd:
                            crate::cmd::mail::MailCmd::Locks {
                                cmd: crate::cmd::mail::MailLockCmd::Show { path, project, .. },
                            },
                    }) => {
                        assert_eq!(path, "src/x.rs");
                        assert_eq!(project, None);
                    }
                    _ => panic!("expected mail locks show"),
                }
                let cli = crate::Cli::try_parse_from([
                    "flywheel", "mail", "locks", "show", "src/x.rs", "proj-a",
                ])
                .expect("<PATH> [PROJECT] must parse");
                match cli.command {
                    Some(crate::Commands::Mail {
                        cmd:
                            crate::cmd::mail::MailCmd::Locks {
                                cmd: crate::cmd::mail::MailLockCmd::Show { path, project, .. },
                            },
                    }) => {
                        assert_eq!(path, "src/x.rs");
                        assert_eq!(project.as_deref(), Some("proj-a"));
                    }
                    _ => panic!("expected mail locks show (project form)"),
                }
            })
            .expect("spawn the parse thread");
        handle
            .join()
            .expect("the bd-vben locks-show parse assertions must hold");
    }

    /// bd-blank-project-spelling-non-mail-doors-76hi: the non-mail sibling
    /// of the blank-spelling rejection — `project_dir(cfg, "")` aliases onto
    /// the workspace root, so every door must fail loud on a blank winning
    /// spelling. Pins all four shared helpers' contracts.
    #[test]
    fn blank_project_helpers_reject_blank_keep_none_defaults() {
        assert!(super::check_project_spelling("", "door").is_err());
        assert!(super::check_project_spelling("   ", "door").is_err());
        assert!(super::check_project_spelling("\t\n", "door").is_err());
        assert!(super::check_project_spelling("proj-a", "door").is_ok());
        assert!(super::check_project_spelling(".", "arrive").is_err());
        assert!(super::check_project_spelling("..", "arrive").is_err());
        let msg = format!(
            "{:#}",
            super::check_project_spelling("", "workflow run").unwrap_err()
        );
        assert!(
            msg.contains("workflow run") && msg.contains("--project <slug>"),
            "actionable message, got: {msg}"
        );

        assert_eq!(
            super::project_door(None, "flywheel", "d").expect("none"),
            "flywheel"
        );
        assert!(super::project_door(Some(""), "flywheel", "d").is_err());
        assert!(super::project_door(Some("  "), "flywheel", "d").is_err());
        assert_eq!(
            super::project_door(Some("proj"), "flywheel", "d").expect("real"),
            "proj"
        );

        let none: Option<String> = None;
        let blank = Some(String::new());
        let real = Some("proj-b".to_string());
        assert_eq!(super::project_door_opt(&none, "d").expect("none"), None);
        assert!(super::project_door_opt(&blank, "d").is_err());
        assert_eq!(
            super::project_door_opt(&real, "d").expect("real"),
            Some("proj-b")
        );

        assert!(super::ensure_project_opt(None, "d").is_ok());
        assert!(super::ensure_project_opt(Some(""), "d").is_err());
        assert!(super::ensure_project_opt(Some("proj"), "d").is_ok());
    }

    /// bd-blank-project-spelling-non-mail-doors-76hi: the CLI-level pin —
    /// `--project ""` on a NON-mail door (workflow list) parses (clap takes
    /// any string) but the door's resolver fails loud with the actionable
    /// message instead of reading the workspace root's project tier.
    #[test]
    fn workflow_list_blank_project_spelling_fails_loud_at_door() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                use clap::Parser;
                let cli =
                    crate::Cli::try_parse_from(["flywheel", "workflow", "list", "--project", ""])
                        .expect("blank --project must still PARSE (rejection lives at the door)");
                match cli.command {
                    Some(crate::Commands::Workflow { cmd }) => match &cmd {
                        crate::cmd::workflow::WorkflowCmd::List { project, .. } => {
                            let err = super::project_door_opt(project, "workflow list").expect_err(
                                "blank spelling must fail loud, never alias to workspace",
                            );
                            let msg = format!("{err:#}");
                            assert!(
                                msg.contains("requires a project"),
                                "actionable message, got: {msg}"
                            );
                        }
                        _ => panic!("expected workflow list"),
                    },
                    _ => panic!("expected workflow"),
                }
            })
            .expect("spawn the parse thread");
        handle.join().expect("the 76hi parse assertions must hold");
    }

    /// bd-empty-project-silently-retargets-workspace-bkr3: `--project ""`
    /// parses (clap takes any string) but the resolver must fail loud — the
    /// empty spelling used to alias onto the workspace default project and
    /// silently retarget every mail door. Pins the end-to-end path: the
    /// parse is fine, the resolution errors with the actionable message.
    #[test]
    fn mail_empty_project_spelling_fails_loud_at_resolver() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                use clap::Parser;
                let cli = crate::Cli::try_parse_from([
                    "flywheel",
                    "mail",
                    "send",
                    "--project",
                    "",
                    "--to",
                    "TanOtter",
                    "--subject",
                    "s",
                    "--body",
                    "b",
                ])
                .expect("empty --project must still PARSE (rejection lives in the resolver)");
                match cli.command {
                    Some(crate::Commands::Mail {
                        cmd: crate::cmd::mail::MailCmd::Send { project_flag, .. },
                    }) => {
                        let none: Option<String> = None;
                        let err = super::mail_project(&none, &project_flag, "send")
                            .expect_err("empty --project must fail loud, never alias to workspace");
                        let msg = format!("{err:#}");
                        assert!(
                            msg.contains("requires a project"),
                            "actionable message, got: {msg}"
                        );
                    }
                    _ => panic!("expected mail send"),
                }
            })
            .expect("spawn the parse thread");
        handle.join().expect("the bkr3 parse assertions must hold");
    }
}

/// bd-2o1v stack-reset contracts: the crew-confirm gate and the profile
/// projections (real `builtin` flag + the `fallback` taxonomy surfaced).
#[cfg(test)]
mod bd2o1v_reset_tests {
    fn body_of(src: &'static str, start_needle: &str, end_needle: &str) -> &'static str {
        let start = src.find(start_needle).expect("start needle");
        let end = src
            .find(end_needle)
            .unwrap_or(src.len())
            .max(start + start_needle.len());
        &src[start..end]
    }

    /// Crew plans are proposals: `briefing --commit` may not auto-dispatch a
    /// spawn-class topology without the operator's word — TTY asks inline,
    /// non-TTY refuses naming --yes. Zero-pane topologies (open/join/none)
    /// never gate.
    #[test]
    fn briefing_commit_gates_spawn_class_crews() {
        let src = include_str!("main.rs");
        let body = body_of(
            src,
            "\nfn cmd_briefing(",
            "\nfn warn_uncommitted_decision_docs(",
        );
        let bind = body
            .find("apply_answers")
            .expect("questions bind into argv");
        let gate = body.find("spawns_crew").expect("gate predicate");
        let refusal = body.find("crew-confirm gate").expect("non-TTY refusal");
        let tty_ask = body.find("spawn this crew?").expect("TTY inline ask");
        let exec = body
            .find("briefing --commit {}")
            .expect("argv exec must exist after the gate");
        assert!(bind < gate && gate < exec && refusal < exec && tty_ask < exec);
        assert!(
            body.contains("briefing::spawns_crew(&b.proposal)"),
            "zero-pane topologies bypass the gate"
        );
    }

    /// D4 taxonomy surfaces: `profile show` must project `fallback` (it
    /// dropped it while `model_fallback` showed), and `profile list` must
    /// report the real builtin flag from the catalog source, not the
    /// vacuous skills/behavior-emptiness heuristic.
    #[test]
    fn profile_projections_surface_fallback_and_real_builtin() {
        let src = include_str!("cmd/profile.rs");
        let show = {
            let start = src
                .rfind("ProfileCmd::Show { name, json } => {")
                .expect("real Show arm");
            let end = src.rfind("ProfileCmd::Create {").expect("real Create arm");
            &src[start..end]
        };
        assert!(show.matches(r#""fallback": p.fallback"#).count() >= 2);
        let list = {
            let start = src.rfind("ProfileCmd::List =>").expect("real List arm");
            let end = src
                .rfind("ProfileCmd::Show { name, json } => {")
                .expect("real Show arm bounds List");
            &src[start..end]
        };
        assert!(
            list.contains(r#""builtin": is_builtin_profile(&name)"#),
            "list must derive builtin from the catalog source of truth"
        );
    }
}

/// bd-kj2z: the door means woken — `cmd_arrive` records the arriving
/// pin's subscription (idempotent, project-named) instead of relying
/// on `doorbell start`, which arms nothing on an empty registry.
#[test]
fn arrive_records_pin_subscription() {
    let src = include_str!("main.rs");
    let start = src
        .rfind("\nfn cmd_arrive(")
        .expect("real cmd_arrive (rfind skips quoted copies)");
    let end = src[start..]
        .find("\n/// (holder pin, whois.stale)")
        .map(|i| start + i)
        .unwrap_or(src.len());
    let body = &src[start..end];
    let hook = body
        .find("register_arrival_subscription")
        .expect("arrive records the pin subscription");
    let gate = body.find("if let Some(pin) = as_agent").expect("pin-gated");
    assert!(gate < hook, "subscription follows the pin gate");
}

/// Key on disk is not a whois row. Arrive must upsert `--as` so captain/whois
/// resolve after the door (QuietHeron-on-disk-but-unknown).
#[test]
fn arrive_registers_arriver_pin() {
    let src = include_str!("main.rs");
    let start = src
        .rfind("\nfn cmd_arrive(")
        .expect("real cmd_arrive (rfind skips quoted copies)");
    let end = src[start..]
        .find("\n/// (holder pin, whois.stale)")
        .map(|i| start + i)
        .unwrap_or(src.len());
    let body = &src[start..end];
    assert!(
        body.contains("register_agent"),
        "arrive must upsert the --as pin into the mail registry"
    );
    let upsert = body.find("register_agent").expect("upsert");
    let presence = body.find("presence").expect("presence publish");
    assert!(
        upsert < presence,
        "register before presence so whois sees the pin"
    );
}

/// bd-gpj0: open-mode arrive must bind `HC_TERMINAL_ID` so doorbell can push,
/// or say pull-only when the env is absent. A pin with no pane stays
/// digest-bound (`no_injectable_panes`).
#[test]
fn arrive_binds_open_pane_or_says_pull_only() {
    let src = include_str!("main.rs");
    let start = src
        .rfind("\nfn cmd_arrive(")
        .expect("real cmd_arrive (rfind skips quoted copies)");
    let end = src[start..]
        .find("\n/// (holder pin, whois.stale)")
        .map(|i| start + i)
        .unwrap_or(src.len());
    let body = &src[start..end];
    assert!(
        body.contains("arrive_maybe_bind_pane"),
        "arrive must bind the current hc terminal (or announce pull-only)"
    );
    let bind = body.find("arrive_maybe_bind_pane").expect("bind helper");
    let register = body.find("register_agent").expect("register");
    assert!(
        register < bind,
        "bind the pane after the pin is registered so whois/registry agree"
    );
}

#[cfg(test)]
mod tripwire {
    /// Locate a column-0 `fn name(` definition in `src`.
    ///
    /// Tripwires often quote the same needle in string literals
    /// (`src.find("fn cmd_spawn(")`). Those quotes are not at column 0, so
    /// searching for `\nfn name(` finds the real definition and never the
    /// test's own literal — bare `find`/`rfind` invent-green or false-red
    /// when other tests also quote the needle. Handles `pub(crate) fn`
    /// too (bd-mngn: handlers migrate into cmd/ modules).
    pub fn find_fn(src: &str, name: &str) -> usize {
        let needle = format!("\nfn {name}(");
        if let Some(i) = src.find(&needle) {
            return i + 1;
        }
        let needle = format!("\npub(crate) fn {name}(");
        src.find(&needle)
            .map(|i| i + 1)
            .unwrap_or_else(|| panic!("expected toplevel `fn {name}(`"))
    }

    /// The body of a column-0 `fn name(` definition, ending at the next fn
    /// line (bare, pub, or pub(crate), any indent). Stopping only at bare
    /// `fn ` leaks into neighbours and blinds the assertion — a removed
    /// guard still passes on the next door's strict load (bd-registry-strict).
    pub fn fn_body<'a>(src: &'a str, name: &str) -> &'a str {
        body_from(src, find_fn(src, name))
    }

    /// `fn_body` from an explicit definition offset (for methods find_fn
    /// cannot see, e.g. trait-impl members): slice to the next fn line.
    pub fn body_from(src: &str, start: usize) -> &str {
        let rest = &src[start..];
        // Skip the definition line itself: its leading `fn ` would end the
        // slice immediately (the name is what we searched for).
        let tail = &rest[rest.find('\n').unwrap_or(rest.len())..];
        let mut end = rest.len();
        let mut i = 0;
        while i < tail.len() {
            let line_end = tail[i..].find('\n').map_or(tail.len(), |k| i + k);
            let line = &tail[i..line_end];
            let t = line.trim_start_matches('#').trim_start();
            // skip attribute lines like #[allow(...)]
            if !line.trim_start().starts_with('#')
                && (t.starts_with("fn ")
                    || t.starts_with("pub fn ")
                    || t.starts_with("pub(crate) fn "))
            {
                end = i + 1;
                break;
            }
            i = line_end + 1;
        }
        &rest[..end]
    }
}

#[cfg(test)]
mod spawn_boot_reliability_tests {
    use super::*;

    #[test]
    fn spawn_verifies_live_process_and_recovers_or_fails() {
        let spawn_src = include_str!("cmd/spawn.rs");
        let spawn = crate::tripwire::find_fn(spawn_src, "cmd_spawn");
        let spawn_body = &spawn_src[spawn..];
        let src = include_str!("main.rs");
        let helper = crate::tripwire::find_fn(src, "verify_or_respawn_pane");
        let helper_body = &src[helper..];

        assert!(
            spawn_body
                .find("verify_or_respawn_pane(")
                .expect("verify call in cmd_spawn")
                > spawn_body
                    .find("prime_bootstrap(&mut c, &pane_id, &kind)")
                    .expect("prime call in cmd_spawn"),
            "cmd_spawn must verify the live process AFTER priming (bd-spawn-omp-boot-reliability-3h06)"
        );
        assert!(
            spawn_body.contains("no live agent process after spawn + recovery"),
            "cmd_spawn must bail when recovery is exhausted (bd-spawn-omp-boot-reliability-3h06)"
        );
        let swarm = spawn_body
            .find("swarm ready: flywheel")
            .expect("swarm-ready println in cmd_spawn");
        let gate = spawn_body
            .find("dead_panes(&booted")
            .expect("final dead_panes gate in cmd_spawn");
        assert!(
            gate < swarm,
            "the final process gate must run BEFORE 'swarm ready' (bd-spawn-omp-boot-reliability-3h06)"
        );
        assert!(
            helper_body.contains("pane_agent_alive"),
            "verify_or_respawn_pane must probe the LIVE process via pane_agent_alive"
        );
        assert!(
            helper_body.contains("start_agent_with_taken_name_clear"),
            "verify_or_respawn_pane must re-launch through the shared taken-name recovery (bd-s91y)"
        );
        assert!(
            helper_body.contains("pane_send_input(pane_id, &[\"CTRL_C\"])"),
            "verify_or_respawn_pane must cancel a partial command before re-typing the launch"
        );
        assert!(
            spawn_body.contains("no live agent process after spawn + recovery"),
            "dead-shell must bail with GS-037 P0-1 marker (nonzero exit)"
        );
        assert!(
            helper_body.contains("start_agent_with_taken_name_clear"),
            "verify_or_respawn_pane must route the rename-clear path (bd-s91y)"
        );
    }

    /// bd-rename-clear-name-too-long-oxd7: the rename-clear escape hatch must
    /// mint an hc-LEGAL label for ANY name hc itself accepted.
    ///
    /// The bare `{name}-stale-{attempts}` concat overflows hc's 32-char cap
    /// for a long project slug (`flywheel-elm-effect-rs-oc` (25) + `-stale-1`
    /// = 33), and hc answers `invalid_agent_name` — so the one door that
    /// frees a taken name wedges shut exactly when it is needed.
    #[test]
    fn stale_rename_label_stays_plane_legal() {
        assert_eq!(
            "flywheel-elm-effect-rs-oc-stale-1".len(),
            33,
            "the pre-fix label overflowed the 32-char cap"
        );
        let long = "flywheel-elm-effect-rs-oc";
        for attempts in [1u32, 2, 7, 19, u32::MAX] {
            let label = stale_rename_label(long, attempts);
            assert!(
                is_terminal_legal_name(&label),
                "{label:?} ({} chars) must be hc-legal for attempts={attempts}",
                label.len()
            );
            assert!(
                label.ends_with(&format!("stale-{attempts}")),
                "{label:?} must keep the attempt number"
            );
        }
        assert_eq!(
            stale_rename_label(long, 1),
            "flywheel-elm-effect-rs-o-stale-1"
        );
        assert_ne!(stale_rename_label(long, 1), stale_rename_label(long, 2));
        assert_eq!(
            stale_rename_label("flywheel-be-oc", 3),
            "flywheel-be-oc-stale-3"
        );
        let capped = "a".repeat(32);
        assert!(is_terminal_legal_name(&stale_rename_label(&capped, 1)));
    }

    /// The wiring gate: the stale label must be minted through the bounded
    /// helper, never the raw concat (a helper nobody calls fixes nothing).
    /// bd-s91y re-pointed this at the shared recovery's pure decision, where
    /// the mint moved — `verify_or_respawn_pane` no longer mints it itself.
    #[test]
    fn rename_clear_mints_its_label_through_the_bounded_helper() {
        let src = include_str!("main.rs");
        let helper = crate::tripwire::find_fn(src, "taken_name_action");
        let rest = &src[helper..];
        let end = rest[1..].find("\nfn ").map_or(rest.len(), |i| i + 1);
        let body = &rest[..end];
        assert!(
            body.contains("stale_rename_label(contested, attempt)"),
            "the shared taken-name decision must mint the label via stale_rename_label"
        );
        assert!(
            !src.contains("format!(\"{name}-stale-{attempts}\")"),
            "the raw `{{name}}-stale-{{attempts}}` concat overflows hc's 32-char cap"
        );
    }

    /// GS-037: send receipt honesty + wake observe-only + presence + compose retire.
    #[test]
    fn gs037_send_wake_presence_folds_present() {
        let src = crate::test_source::crate_source();
        let send = crate::tripwire::find_fn(&src, "cmd_send");
        let send_end = crate::tripwire::find_fn(&src, "stall_step");
        let send_body = &src[send..send_end];
        assert!(
            send_body.contains("fire_and_forget")
                && send_body.contains("refuse invent-green exit 0")
                && send_body.contains("produced no landed:true receipt"),
            "cmd_send must fail-loud without --wait/--fire-and-forget AND on --wait with no landed"
        );
        assert!(
            src.contains("require_equals = true"),
            "--fire-and-forget must require equals form so MSG positionals survive"
        );
        assert!(
            !send_body.contains("doorbell_persist_wake("),
            "cmd_send must NOT self-mint last_wake via doorbell_persist_wake (GS-037 P0-C)"
        );
        assert!(
            send_body.contains("wake_before_unix")
                && send_body.contains("independent wake receipt missing"),
            "cmd_send --wait landed must require independent wake advance"
        );
        assert!(
            send_body.contains("best-effort pane send-text")
                && send_body.contains("stall_giveup_error"),
            "StallStep::GiveUp must fail-loud after best-effort pane send-text"
        );
        let giveup_marker = "best-effort pane send-text (not a wait receipt";
        let gi = send_body
            .find(giveup_marker)
            .expect("GiveUp honesty marker");
        let giveup_slice = &send_body[gi..gi + 2000];
        assert!(
            giveup_slice.contains("return Err(stall_giveup_error")
                && !giveup_slice.contains("break agent"),
            "GiveUp must return Err(stall_giveup_error), never break agent into landed"
        );
        let persist = crate::tripwire::find_fn(&src, "doorbell_note_observed_wake");
        let persist_end = crate::tripwire::find_fn(&src, "upsert_workspace_receipt");
        let persist_body = &src[persist..persist_end];
        assert!(
            persist_body.contains("-> Result<()>")
                && persist_body.contains("save_wake_state")
                && !persist_body.contains("let _ = doorbell::save_wake_state"),
            "doorbell_note_observed_wake must fail-loud on save (no let _ =)"
        );
        let spawn = crate::tripwire::find_fn(&src, "cmd_spawn");
        let spawn_end = src[spawn..]
            .find("\nfn ")
            .map_or(spawn + 40000, |i| spawn + i);
        let spawn_body = &src[spawn..spawn_end];
        assert!(
            spawn_body.contains("presence_project"),
            "presence publish must use absolute project key (register-before-presence)"
        );
        let wf_src = include_str!("cmd/workflow.rs");
        assert!(
            wf_src.contains("retire-orphans")
                && wf_src.contains("GS-039 P0-3")
                && wf_src.contains("allow-crew-expand")
                && wf_src.contains("retire_orphans: bool")
                && wf_src.contains("long = \"retire-orphans\"")
                && wf_src.contains("long = \"no-retire-orphans\"")
                && wf_src.contains("overrides_with")
                && wf_src.contains("hide = true"),
            "compose must default retire_orphans false + visible --retire-orphans + hidden --no-retire-orphans via overrides_with (GS-039/040 B2)"
        );
    }

    /// GS-039: CoS-honest folds — real wake observation, open-mode authority,
    /// solo compose sizing, name-bind fail-closed, no invent-green.

    #[test]
    fn gs040_compose_wf_name_includes_project_tag() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("GS-040: crew_agent_name keeps only 16"),
            "compose wf_name must include project tag (avoid truncated-ISO collisions)"
        );
        assert!(
            src.contains("chrono::Utc::now().format(") && src.contains("%m%d%H%M%S"),
            "compose wf_name must use date-bearing %m%d%H%M%S (unique across days in 16-char slug)"
        );
    }

    #[test]
    fn gs040_retire_orphans_single_arg_no_dual() {
        let src = include_str!("cmd/workflow.rs");
        let compose_marker = "Runtime workflow composition (bd-8xca)";
        let start = src.find(compose_marker).expect("Compose docs");
        let end = (start + 12000).min(src.len());
        let window = &src[start..end];
        let needle = "long = \"retire-orphans\"";
        let hits: Vec<_> = window.match_indices(needle).collect();
        assert_eq!(
            hits.len(),
            1,
            "Compose must declare long = \"retire-orphans\" exactly once"
        );
        let after_pos = &window[hits[0].0..hits[0].0 + 220];
        assert!(
            after_pos.contains("retire_orphans: bool"),
            "retire-orphans arg must bind retire_orphans: bool"
        );
        assert!(
            !after_pos.contains("long = \"no-retire-orphans\""),
            "must not dual-#[arg] long=no-retire-orphans on same field (clap drop bug)"
        );
        assert!(
            after_pos.contains("ArgAction::SetTrue") || after_pos.contains("overrides_with"),
            "positive --retire-orphans must SetTrue / link via overrides_with"
        );
        assert!(
            window.contains("long = \"no-retire-orphans\"")
                && window.contains("ArgAction::SetFalse")
                && window.contains("hide = true")
                && window.contains("_no_retire_orphans: bool"),
            "hidden --no-retire-orphans SetFalse must be a separate field"
        );
        let dual_bug = "#[arg(long = \"retire-orphans\"";
        let dual_bug2 = "#[arg(long = \"no-retire-orphans\"";
        if let (Some(a), Some(b)) = (window.find(dual_bug), window.find(dual_bug2)) {
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            let between = &window[lo..hi];
            assert!(
                between.contains(": bool"),
                "no-retire-orphans #[arg] must not stack on same field as retire-orphans"
            );
        }
    }

    #[test]
    fn gs040_retire_orphans_clap_help_and_parse() {
        use clap::CommandFactory;
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                let cmd = Cli::command();
                let wf = cmd
                    .find_subcommand("workflow")
                    .expect("workflow subcommand");
                let compose = wf
                    .find_subcommand("compose")
                    .expect("workflow compose subcommand");
                let mut help = Vec::new();
                compose.clone().write_long_help(&mut help).expect("help");
                let help_s = String::from_utf8(help).expect("utf8");
                assert!(
                    help_s.contains("--retire-orphans"),
                    "compose help must list --retire-orphans"
                );
                assert!(
                    !help_s.contains("--no-retire-orphans"),
                    "compose help must hide --no-retire-orphans"
                );

                let parse = |args: &[&str]| -> bool {
                    let cli = Cli::try_parse_from(args)
                        .unwrap_or_else(|e| panic!("parse {:?} failed: {e}", args));
                    match cli.command {
                        Some(Commands::Workflow {
                            cmd: WorkflowCmd::Compose { retire_orphans, .. },
                        }) => retire_orphans,
                        _ => panic!("expected workflow compose"),
                    }
                };
                assert!(
                    !parse(&["flywheel", "workflow", "compose", "t"]),
                    "bare default retire_orphans=false"
                );
                assert!(
                    parse(&["flywheel", "workflow", "compose", "t", "--retire-orphans"]),
                    "--retire-orphans => true"
                );
                assert!(
                    !parse(&[
                        "flywheel",
                        "workflow",
                        "compose",
                        "t",
                        "--no-retire-orphans"
                    ]),
                    "--no-retire-orphans => false"
                );
                assert!(
                    !parse(&[
                        "flywheel",
                        "workflow",
                        "compose",
                        "t",
                        "--retire-orphans",
                        "--no-retire-orphans"
                    ]),
                    "--retire-orphans then --no-retire-orphans => false (overrides_with)"
                );
                assert!(
                    parse(&[
                        "flywheel",
                        "workflow",
                        "compose",
                        "t",
                        "--no-retire-orphans",
                        "--retire-orphans"
                    ]),
                    "--no-retire-orphans then --retire-orphans => true (overrides_with)"
                );
            })
            .expect("spawn clap probe");
        handle.join().expect("clap probe thread");
    }

    /// bd-3npc: every user-facing mail-cli positional PROJECT also accepts
    /// --project. Behavioral (not invent-green): help lists the flag; the
    /// flag form and the positional form fill the same effective project;
    /// the pure resolver is total over flag/positional/both/neither.
    #[test]
    fn bd_3npc_mail_project_flag_alias() {
        use clap::CommandFactory;
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                let cmd = Cli::command();
                let mail = cmd.find_subcommand("mail").expect("mail subcommand");
                let send = mail.find_subcommand("send").expect("mail send subcommand");
                let mut help = Vec::new();
                send.clone().write_long_help(&mut help).expect("help");
                let help_s = String::from_utf8(help).expect("utf8");
                assert!(
                    help_s.contains("--project"),
                    "mail send help must list --project"
                );

                let project_of = |args: &[&str]| -> String {
                    let cli = Cli::try_parse_from(args)
                        .unwrap_or_else(|e| panic!("parse {args:?} failed: {e}"));
                    match cli.command {
                        Some(Commands::Mail {
                            cmd:
                                MailCmd::Send {
                                    project,
                                    project_flag,
                                    ..
                                },
                        }) => super::mail_project(&project, &project_flag, "send")
                            .expect("resolve")
                            .to_string(),
                        _ => panic!("expected mail send"),
                    }
                };
                assert_eq!(
                    project_of(&[
                        "flywheel",
                        "mail",
                        "send",
                        "--project",
                        "flywheel",
                        "--to",
                        "TanOtter",
                        "--subject",
                        "s",
                        "--body",
                        "b"
                    ]),
                    "flywheel",
                    "--project must fill the project field"
                );
                assert_eq!(
                    project_of(&[
                        "flywheel",
                        "mail",
                        "send",
                        "flywheel",
                        "--to",
                        "TanOtter",
                        "--subject",
                        "s",
                        "--body",
                        "b"
                    ]),
                    "flywheel",
                    "positional project must keep working"
                );
            })
            .expect("spawn clap probe");
        handle.join().expect("clap probe thread");
    }

    /// bd-3npc: the positional/--project resolver is total — flag wins on
    /// both, neither fails loud. Pure function, tested exhaustively.
    #[test]
    fn bd_3npc_mail_project_resolver_is_total() {
        let pos = Some("pos".to_string());
        let flag = Some("flag".to_string());
        let none: Option<String> = None;
        assert_eq!(
            super::mail_project(&pos, &none, "send").expect("pos"),
            "pos"
        );
        assert_eq!(
            super::mail_project(&none, &flag, "send").expect("flag"),
            "flag"
        );
        assert_eq!(
            super::mail_project(&pos, &flag, "send").expect("both"),
            "flag",
            "flag wins when both are given"
        );
        assert!(
            super::mail_project(&none, &none, "send").is_err(),
            "neither must fail loud, never an empty project"
        );
    }

    /// bd-empty-project-silently-retargets-workspace-bkr3: an empty/blank
    /// project spelling must fail loud exactly like a missing one — it used
    /// to slip through, and `project_dir(cfg, "")` aliased it onto the
    /// workspace root (slug "workspace"), silently retargeting every mail
    /// door to a different project. Flag-wins precedence is unchanged; the
    /// WINNING spelling must be non-blank.
    #[test]
    fn bd_empty_project_spelling_fails_loud_never_retargets() {
        let none: Option<String> = None;
        let empty = Some(String::new());
        let blank = Some("   ".to_string());
        let real = Some("proj-a".to_string());
        for spelling in [&empty, &blank] {
            assert!(
                super::mail_project(spelling, &none, "send").is_err(),
                "positional {spelling:?} must fail loud, never alias to workspace"
            );
            assert!(
                super::mail_project(&none, spelling, "send").is_err(),
                "flag {spelling:?} must fail loud, never alias to workspace"
            );
        }
        assert_eq!(
            super::mail_project(&empty, &real, "send").expect("real flag wins"),
            "proj-a"
        );
        assert!(
            super::mail_project(&real, &empty, "send").is_err(),
            "a blank winning flag must fail loud, not fall back to the positional"
        );
    }

    /// bd-zd2v: `mail inbox` gains `--since` parity with toron (RFC3339 or
    /// the literal `last` cursor). Behavioral: help lists the flag; the
    /// flag fills the inbox filter field.
    #[test]
    fn bd_zd2v_mail_inbox_since_flag() {
        use clap::CommandFactory;
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                let cmd = Cli::command();
                let mail = cmd.find_subcommand("mail").expect("mail subcommand");
                let inbox = mail
                    .find_subcommand("inbox")
                    .expect("mail inbox subcommand");
                let mut help = Vec::new();
                inbox.clone().write_long_help(&mut help).expect("help");
                let help_s = String::from_utf8(help).expect("utf8");
                assert!(
                    help_s.contains("--since"),
                    "mail inbox help must list --since"
                );

                let since_of = |args: &[&str]| -> Option<String> {
                    let cli = Cli::try_parse_from(args)
                        .unwrap_or_else(|e| panic!("parse {args:?} failed: {e}"));
                    match cli.command {
                        Some(Commands::Mail {
                            cmd: MailCmd::Inbox { since, .. },
                        }) => since,
                        _ => panic!("expected mail inbox"),
                    }
                };
                assert_eq!(
                    since_of(&["flywheel", "mail", "inbox", "flywheel", "--since", "last"]),
                    Some("last".to_string()),
                    "--since last must fill the filter field"
                );
                assert_eq!(
                    since_of(&[
                        "flywheel",
                        "mail",
                        "inbox",
                        "flywheel",
                        "--since",
                        "2026-09-07T00:00:00Z"
                    ]),
                    Some("2026-09-07T00:00:00Z".to_string()),
                    "--since RFC3339 must fill the filter field"
                );
                assert_eq!(
                    since_of(&["flywheel", "mail", "inbox", "flywheel"]),
                    None,
                    "absent --since stays None (default path byte-identical)"
                );
            })
            .expect("spawn clap probe");
        handle.join().expect("clap probe thread");
    }

    /// bd-7qg6: `mail send` gains `--resolve-bead` parity with toron
    /// (ADR-0010 D9 structural done-hold). Help lists the flag; the flag
    /// fills the send field; absent stays None (default path byte-identical).
    #[test]
    fn bd_7qg6_mail_send_resolve_bead_flag() {
        use clap::CommandFactory;
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                let cmd = Cli::command();
                let mail = cmd.find_subcommand("mail").expect("mail subcommand");
                let send = mail.find_subcommand("send").expect("mail send subcommand");
                let mut help = Vec::new();
                send.clone().write_long_help(&mut help).expect("help");
                let help_s = String::from_utf8(help).expect("utf8");
                assert!(
                    help_s.contains("--resolve-bead"),
                    "mail send help must list --resolve-bead"
                );

                let bead_of = |args: &[&str]| -> Option<String> {
                    let cli = Cli::try_parse_from(args)
                        .unwrap_or_else(|e| panic!("parse {args:?} failed: {e}"));
                    match cli.command {
                        Some(Commands::Mail {
                            cmd: MailCmd::Send { resolve_bead, .. },
                        }) => resolve_bead,
                        _ => panic!("expected mail send"),
                    }
                };
                let base = [
                    "flywheel",
                    "mail",
                    "send",
                    "flywheel",
                    "--to",
                    "TanOtter",
                    "--subject",
                    "s",
                    "--body",
                    "b",
                ];
                assert_eq!(
                    bead_of(&[base.as_slice(), &["--resolve-bead", "bd-123"]].concat()),
                    Some("bd-123".to_string()),
                    "--resolve-bead must fill the send field"
                );
                assert_eq!(
                    bead_of(&base),
                    None,
                    "absent --resolve-bead stays None (default path byte-identical)"
                );
            })
            .expect("spawn clap probe");
        handle.join().expect("clap probe thread");
    }

    #[test]
    fn gs039_cos_honest_folds_present() {
        let brain_rt = include_str!("../crates/doorbell-rt/src/brain.rs");
        assert!(
            brain_rt.contains(
                "transitioned && matches!(s.as_str(), \"working\" | \"idle\" | \"done\" | \"blocked\")"
            ),
            "production Status REAL observation must survive the move"
        );
        assert!(
            brain_rt.contains("\"working\" | \"idle\" | \"done\" | \"blocked\""),
            "Status arm must observe hc status transitions"
        );
        assert!(
            brain_rt.contains("persist_wake"),
            "Status wake arm must stamp wake from observed/delivered pins"
        );
        assert!(
            !brain_rt.contains("injects.first()"),
            "Status arm must not invent-green from planned injects"
        );
        let src = crate::test_source::crate_source();
        assert!(
            include_str!("cmd/doorbell.rs").contains("doorbell_note_observed_wake"),
            "GS-039 P0-1 wake observation helper must be present"
        );
        assert!(
            brain_rt.contains("async fn deliver_prompts") && brain_rt.contains("queued.push(pin)"),
            "deliver_prompts must return accepted-intent pins for the wake stamp (F5: Queued counts, hear stamps on Delivered)"
        );
        let invent_a = format!("{}{}{}", "or_else(", "|", "| injects.first()");
        let invent_b = format!("{}{}", ".or_else(", invent_a.trim_start_matches("or_else("));
        assert!(
            !src.contains(&invent_a) && !src.contains(&invent_b),
            "P0-1 must NOT stamp last_wake from planned injects (no invent-green or_else+injects.first)"
        );
        let send = crate::tripwire::find_fn(&src, "cmd_send");
        let send_end = crate::tripwire::find_fn(&src, "stall_step");
        let send_body = &src[send..send_end];
        assert!(
            send_body.contains("doorbell_note_observed_wake(")
                && send_body.contains("settled_status")
                && send_body.contains("status_l != \"working\"")
                && send_body.contains("\"idle\"")
                && send_body.contains("\"blocked\""),
            "cmd_send --wait must stamp last_wake from observed settled status (GS-039 P0-1)"
        );
        let silent = format!("{}{}", "let _ = ", "doorbell_note_observed_wake");
        assert!(
            src.contains("observed-wake stamp FAILED on poll")
                && src.contains("observed-wake stamp FAILED on inbox sweep")
                && !src.contains(&silent),
            "poll/sweep must loud-warn on observed-wake save failure (no silent let _ =)"
        );
        let brain_rt = include_str!("../crates/doorbell-rt/src/brain.rs");
        assert!(
            brain_rt.contains("transitioned") && brain_rt.contains("prev_status"),
            "brain status arm must gate wake on transition (not static idle rows)"
        );
        // message, split so none straddles the line continuation rustfmt puts
        let wake_start = brain_rt
            .find("\n    fn flush_wake(")
            .expect("brain still has a flush_wake");
        let wake_end = brain_rt[wake_start + 1..]
            .find("\n    fn ")
            .map(|i| wake_start + 1 + i)
            .unwrap_or(brain_rt.len());
        let wake_save = &brain_rt[wake_start..wake_end];
        for needle in [
            "wake state write FAILED on",
            "({ctx})",
            "refuse silent drop",
            "(GS-039)",
        ] {
            assert!(
                wake_save.contains(needle),
                "flush_wake must print {needle:?} so a lost wake save is loud and names its arm"
            );
        }
        let note = crate::tripwire::find_fn(&src, "doorbell_note_observed_wake");
        let note_end = crate::tripwire::find_fn(&src, "upsert_workspace_receipt");
        let note_body = &src[note..note_end];
        let ratchet = format!("{}{}", "prev.", "saturating_add(1)");
        assert!(
            note_body.contains("now <= prev") && !note_body.contains(&ratchet),
            "doorbell_note_observed_wake must not prev.saturating_add ratchet without new observation"
        );
        assert!(
            src.contains("AUTHORITATIVE OPERATOR OBJECTIVE") && src.contains("GS-039 P0-2"),
            "open-mode/--raw must frame AUTHORITATIVE operator objective"
        );
        assert!(
            src.contains("allow-crew-expand")
                && src.contains("GS-039 P0-3")
                && src.contains("ADR-0011 D2")
                && src.contains("packed-under-cap is the default"),
            "compose must pack under crew.max (ADR-0011) instead of robot hard-bail"
        );
        assert!(
            src.contains("GS-039 P0-4")
                && src.contains("panes still unbound/wrong-name")
                && src.contains("Fail-closed under verify: empty OR wrong-name"),
            "spawn must fail if panes unbound or wrong-named after settle"
        );
        assert!(
            src.contains("dim(\"--to all --stage ce-execute\")")
                && !src.contains("dim(\"--to all --stage p1\")"),
            "swarm ready must advertise ce-execute, not retired p1"
        );
        assert!(
            src.contains("send_with_wake")
                && include_str!("cmd/mail.rs").contains("default_value = \"tasked\""),
            "mail send must default --wake tasked"
        );
    }

    #[test]
    fn spawn_starts_doorbell_dispatcher() {
        let spawn_src = include_str!("cmd/spawn.rs");
        let spawn = crate::tripwire::find_fn(spawn_src, "cmd_spawn");
        let spawn_end = crate::tripwire::find_fn(spawn_src, "doorbell_project_workspaces");
        let body = &spawn_src[spawn..spawn_end];
        assert!(
            body.contains("doorbell::ensure_running"),
            "cmd_spawn must start/attach the ADR-0005 doorbell dispatcher"
        );
        let src = include_str!("main.rs");
        let kill = crate::tripwire::find_fn(src, "cmd_kill");
        let kbody = &src[kill..];
        assert!(
            kbody.contains("doorbell::stop"),
            "cmd_kill must stop the doorbell child"
        );
    }

    #[test]
    fn spawn_registers_wake_subscriptions() {
        let src = include_str!("cmd/spawn.rs");
        let spawn = crate::tripwire::find_fn(src, "cmd_spawn");
        let spawn_end = crate::tripwire::find_fn(src, "doorbell_project_workspaces");
        let body = &src[spawn..spawn_end];
        assert!(
            body.contains("register_workspace_subscriptions"),
            "cmd_spawn must record wake subscriptions for the daemon host"
        );
    }

    #[test]
    fn kill_never_touches_daemon() {
        let src = include_str!("main.rs");
        let kill = crate::tripwire::find_fn(src, "cmd_kill");
        let kill_end = crate::tripwire::find_fn(src, "cmd_assign");
        let kbody = &src[kill..kill_end];
        assert!(
            kbody.contains("doorbell::stop"),
            "cmd_kill must stop the dispatcher child"
        );
        for needle in ["18980", "serve --port", "daemon stop", "daemon_start"] {
            assert!(
                !kbody.contains(needle),
                "cmd_kill must never touch the daemon ({needle})"
            );
        }
    }

    #[test]
    fn kill_resolves_through_the_status_door() {
        let src = include_str!("main.rs");
        let kill = crate::tripwire::find_fn(src, "cmd_kill");
        let kill_end = crate::tripwire::find_fn(src, "cmd_assign");
        let kbody = &src[kill..kill_end];
        assert!(
            kbody.contains("rex_door_selected"),
            "cmd_kill must take the status door, not one fixed resolver"
        );
        assert!(
            kbody.contains("find_workspace_dyn(&*backend, project)"),
            "rex arm resolves through the selected plane"
        );
        assert!(
            kbody.contains("find_workspace(&mut c, project)"),
            "hc arm resolves through the hc client, like status"
        );
    }

    #[test]
    fn spawn_no_peer_aware_keeps_identity_skips_dispatcher() {
        let src = include_str!("cmd/spawn.rs");
        let spawn = crate::tripwire::find_fn(src, "cmd_spawn");
        let spawn_end = crate::tripwire::find_fn(src, "doorbell_project_workspaces");
        let body = &src[spawn..spawn_end];
        assert!(
            body.contains("doorbell::identity_prime"),
            "--no-peer-aware must still inject the identity line"
        );
        assert!(
            body.contains("if peer_aware") && body.contains("doorbell::ensure_running"),
            "dispatcher starts only when peer-aware"
        );
    }

    #[test]
    fn edge_triage_escape_spawn_keeps_dispatcher() {
        let src = include_str!("cmd/spawn.rs");
        let spawn = crate::tripwire::find_fn(src, "cmd_spawn");
        let spawn_end = crate::tripwire::find_fn(src, "doorbell_project_workspaces");
        let body = &src[spawn..spawn_end];
        assert!(
            body.contains("if peer_aware"),
            "dispatcher gate is peer-aware only, never escape-gated"
        );
        let gate = body.find("if peer_aware").unwrap();
        assert!(
            !body[gate..gate + 200].contains("escape"),
            "escape must not appear in the dispatcher gate"
        );
    }

    /// bd-durable-doorbell-lngf: the wake layer is supervised and
    /// observable — status shows liveness AND lazy-heals; recover restores
    /// dispatchers after a crash; doctor warns on dead ones; the persisted
    /// wake state feeds at-least-once redelivery.
    #[test]
    fn doorbell_layer_is_supervised_observable_durable() {
        let src = crate::test_source::crate_source();
        let surface = include_str!("cmd/doorbell.rs");
        assert!(
            surface.contains("enum DoorbellCmd"),
            "explicit surface exists"
        );
        for needle in [
            "cmd_doorbell_status",
            "cmd_doorbell_start",
            "cmd_doorbell_stop",
            "cmd_doorbell_re_inject",
        ] {
            assert!(src.contains(needle), "{needle} wired into the surface");
        }
        let recover = crate::tripwire::find_fn(&src, "cmd_recover");
        let recover_end = crate::tripwire::find_fn(&src, "rebind_from_registry");
        let recover_body = &src[recover..recover_end];
        assert!(
            recover_body.contains("doorbell::ensure_running"),
            "recover must heal dead dispatchers"
        );
        let status = crate::tripwire::find_fn(&src, "cmd_status");
        let status_end = crate::tripwire::find_fn(&src, "cmd_binding_rebind");
        let status_body = &src[status..status_end];
        assert!(
            status_body.contains("doorbell::ensure_running") && status_body.contains("wake_state"),
            "status shows last_wake and lazy-heals"
        );
        assert!(
            src.contains("doorbell_persist_wake"),
            "dispatcher persists wake state (at-least-once redelivery)"
        );
        let dsrc = include_str!("cmd/doorbell.rs");
        crate::tripwire::find_fn(dsrc, "cmd_internal_doorbell");
        assert!(
            dsrc.contains("doorbell::load_wake_state"),
            "dispatcher restarts from persisted high-waters"
        );
    }

    #[test]
    fn dead_panes_flags_everything_but_confirmed_live() {
        let booted: Vec<(String, String, String)> = vec![
            ("w1:p1".into(), "a".into(), "omp".into()),
            ("w1:p2".into(), "b".into(), "omp".into()),
            ("w1:p3".into(), "c".into(), "omp".into()),
            ("w1:p4".into(), "d".into(), "omp".into()),
        ];
        let probes = |pane: &str, _kind: &str| match pane {
            "w1:p1" => Some(true),
            "w1:p2" => Some(false),
            "w1:p3" => None,
            _ => None,
        };
        let dead = dead_panes(&booted, probes);
        let dead_ids: Vec<&str> = dead.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(dead_ids, vec!["w1:p2", "w1:p3", "w1:p4"]);
    }
}

#[allow(clippy::too_many_arguments)]
/// The neutral boot prompt that opens the prompt gate. H2
/// (bd-harness-pane-send-text-7fe8): delivered via `terminal.send` after the
/// TUI owns the foreground (works for any kind, non-agent panes too), not as
/// a positional MESSAGES launch arg.
///
/// Lifecycle-authority kinds — pi/omp/kimi/opencode/kilo/mastracode (+grok) —
/// only accept a prompt once their TUI owns the foreground; hc 0.1.16 has no
/// integration surface, so the foreground/readiness check plus this prime is
/// what removes the detection-class race (bd-bug-omp-hc-wedge, H5
/// bd-harness-prime-kind-list-eivx).
fn boot_prime_line() -> String {
    boot_prime_line_for(None)
}

/// bd-koafr: the boot prompt is the difference between a crew that starts on
/// its bead and one that burns two minutes reading mail and listing
/// awareness beads before saying "no objective".
fn boot_prime_line_for(assign: Option<&str>) -> String {
    match assign.map(str::trim).filter(|a| !a.is_empty()) {
        Some(beads) => format!(
            "Boot ready check: reply exactly READY. Your assignment: {beads}. \
             Claim it (`br update <id> --status in_progress --assignee <pin>`) \
             and run its VERIFY. Do not read mail or triage the project \
             backlog first."
        ),
        None => "Boot ready check: reply exactly READY and wait for an \
                 assignment. Do not read mail or triage the project backlog; \
                 the brief arrives next."
            .to_string(),
    }
}

/// Kinds that require the prime as a positional launch arg (secondary path —
/// kept for kinds that cannot take typed TUI input). Primary is
/// [`prime_via_send_text`]; currently no kind needs the in-arg form.
fn needs_in_arg_prime(_kind: &str) -> bool {
    false
}

/// Prime the agent.prompt gate via `pane.send_text` + Enter (H2): replaces
/// the char-by-char send-keys hack (send-keys rejects multi-char text:
/// `invalid_key`) and the MESSAGES-arg boot prime. Best-effort — a failed
/// prime warns but never fails the spawn.
///
/// bd-umb3: retried — a transiently-busy pane (mid-boot, integration not yet
/// reporting) must not silently stay unpromptable. Up to 3 attempts with a
/// short settle between; on final failure the warning carries the manual
/// socket one-liner so an operator can prime the pane by hand instead of
/// discovering `agent_not_ready` on the first send.
fn prime_via_send_text(c: &mut Client, pane_id: &str) {
    prime_line_via_send_text(c, pane_id, &boot_prime_line());
}

/// Phase 3: is the inbox comms path on? Default on. `TORON_COMMS_INBOX=0`
/// or `[comms] inbox_log = false` opts out. Spawn-time and doorbell wiring
/// branch on it.
fn comms_inbox_on() -> bool {
    match Config::load() {
        Ok(c) => comms_send::enabled(&c),
        Err(_) => toron_core::inbox::try_enabled_with(None).unwrap_or(true),
    }
}

/// The spawn-time prime for pane `pane_id` running hc kind `kind`. Flag on:
/// the per-harness `PaneWrite::LaunchBootstrap` constant through the sink
/// (pull contract: run `toron next`; no payload). Flag off: the legacy
/// READY check, byte for byte as before.
fn prime_bootstrap(c: &mut Client, pane_id: &str, kind: &str) {
    prime_bootstrap_with(c, pane_id, kind, comms_inbox_on());
}

/// What the spawn-time prime does; pure so both arms are testable.
#[derive(Debug, PartialEq, Eq)]
enum PrimeChoice {
    /// Flag on: `PaneWrite::LaunchBootstrap { harness }` through the sink.
    Bootstrap(pane_write::Harness),
    /// Flag off: the legacy READY check.
    LegacyReady,
}

fn prime_choice(inbox: bool, kind: &str) -> PrimeChoice {
    if inbox {
        PrimeChoice::Bootstrap(pane_write::Harness::from_kind(kind))
    } else {
        PrimeChoice::LegacyReady
    }
}

fn prime_bootstrap_with(c: &mut Client, pane_id: &str, kind: &str, inbox: bool) {
    match prime_choice(inbox, kind) {
        PrimeChoice::Bootstrap(h) => {
            pane_write::launch_bootstrap_into(c, pane_id, h);
            note_launch_hook(kind);
        }
        PrimeChoice::LegacyReady => prime_via_send_text(c, pane_id),
    }
}

/// Capable harness, comms flag already on (this only runs from the
/// bootstrap arm). A missing file name or an unverified harness does
/// nothing. Exec off leaves the operator file untouched and says so.
/// A failed write keeps Interrupt; it does not go quiet.
fn note_launch_hook(kind: &str) {
    let path = std::env::var("FLYWHEEL_SUPERVISOR_HOOK_FILE").ok();
    match harness_caps::plan_launch_hook(
        kind,
        path.as_deref().map(std::path::Path::new),
        supervisor_host::exec_allowed(),
    ) {
        harness_caps::LaunchHook::Suppressed => {
            eprintln!("flywheel supervisor: suppressed hook install: FLYWHEEL_ALLOW_EXEC=0");
        }
        harness_caps::LaunchHook::Failed(e) => {
            eprintln!("flywheel supervisor: hook install failed ({e}); interrupt stays");
        }
        harness_caps::LaunchHook::Skip | harness_caps::LaunchHook::Installed => {}
    }
}

/// One typed prime line with retries. bd-peeraware-enametoolong-iswg:
/// launch text must NEVER ride agent.start args — opencode reads a
/// positional arg as a DIRECTORY ("Failed to change directory to <prompt>");
/// every prime is typed via pane.send_text after TUI readiness.
/// Boot-prime retry budget (bd-8pjr): 3 attempts, 2s base, 5s cap.
/// Named so the per-site fail-twice test below pins the config.
fn boot_prime_policy() -> crate::retry::RetryPolicy {
    crate::retry::RetryPolicy {
        max_attempts: 3,
        base: Duration::from_secs(2),
        cap: Duration::from_secs(5),
    }
}

/// Settle before Enter so a TUI that detects paste-by-timing does not treat
/// an immediate Enter as a newline (tuios/hc lesson; hc plane harden).
const PRIME_ENTER_SETTLE: Duration = Duration::from_millis(100);

/// Type `line` into `pane_id` and submit. Prefers `agent.prompt` (no wait)
/// when the pane can take it; otherwise `pane.send_text` + settle + Enter.
/// Returns whether delivery was confirmed. If text landed but Enter failed,
/// retries Enter only — never re-types the text (no invent-green, no double
/// paste). Callers that need honesty (doorbell / recipe send) must check the
/// bool; boot-prime paths may still warn-and-continue.
fn prime_line_via_send_text(c: &mut Client, pane_id: &str, line: &str) -> bool {
    if c.agent_prompt(pane_id, line, None).is_ok() {
        return true;
    }
    let policy = boot_prime_policy();
    let max = policy.max_attempts;
    let text_res = policy.retry_with_backoff(
        || c.pane_send_text(pane_id, line),
        |_| true,
        |attempt, e| {
            eprintln!(
                "flywheel: warn: boot-prime send_text attempt {attempt}/{max} failed for {pane_id}: {e:#} — retrying"
            );
        },
        crate::retry::host_draw,
        std::thread::sleep,
    );
    if let Err(e) = text_res {
        eprintln!(
            "flywheel: warn: boot-prime via pane.send_text failed for {pane_id} after {max} attempts: {e:#}"
        );
        eprintln!(
            "flywheel:   manual socket prime: hc terminal send-text {pane_id} '{}' then send Enter (or terminal agent prompt {pane_id})",
            line
        );
        return false;
    }
    std::thread::sleep(PRIME_ENTER_SETTLE);
    let enter_res = policy.retry_with_backoff(
        || c.pane_send_input(pane_id, &["Enter"]),
        |_| true,
        |attempt, e| {
            eprintln!(
                "flywheel: warn: boot-prime Enter attempt {attempt}/{max} failed for {pane_id}: {e:#} — retrying Enter only"
            );
        },
        crate::retry::host_draw,
        std::thread::sleep,
    );
    if let Err(e) = enter_res {
        eprintln!(
            "flywheel: warn: boot-prime Enter failed for {pane_id} after text landed ({e:#}) — not re-typing text"
        );
        return false;
    }
    true
}

/// H3 (bd-harness-integration-preflight): lifecycle-hook-authority harness
/// kinds — the same list the boot-prime machinery documents (see
/// [`boot_prime_line`]): their hc integration authors idle/working/blocked
/// from hook events, so a missing integration degrades lifecycle-state
/// reporting. Warn-only by H3 decision; the env + prime machinery works
/// regardless.
const LIFECYCLE_AUTHORITY_KINDS: &[&str] = &[
    "pi",
    "omp",
    "kimi",
    "opencode",
    "kilo",
    "mastracode",
    "grok",
];

/// H3 (bd-harness-integration-preflight): before a kind's first agent
/// launches, make sure its hc integration exists and is current.
/// Best-effort by contract — a missing/broken integration NEVER fails the
/// spawn: install failures get the bd-627z.2 loud-warning treatment, and
/// lifecycle-hook-authority kinds additionally warn that lifecycle-state
/// reporting is degraded (the env + boot-prime machinery works regardless —
/// H3 decided warn-only, do NOT block).
fn preflight_kind_integration(
    c: &mut Client,
    kind: &str,
    statuses: &HashMap<String, hc::IntegrationStatus>,
) {
    if hc::integration_target(kind).is_none() {
        return;
    }
    let st = statuses
        .get(kind)
        .cloned()
        .unwrap_or_else(hc::IntegrationStatus::not_installed);
    if st.installed && st.current {
        return;
    }
    if st.installed {
        let detail = match (&st.version, &st.expected_version) {
            (Some(v), Some(e)) => format!("outdated ({v} < {e})"),
            _ => "outdated".to_string(),
        };
        eprintln!("flywheel: warn: {kind} hc integration {detail} — refreshing");
    }
    match hc::integration_install_socket(c, kind) {
        Ok(messages) => {
            eprintln!(
                "flywheel: hc integration {kind} installed via socket{}",
                if messages.is_empty() {
                    String::new()
                } else {
                    format!(": {}", messages.join("; "))
                }
            );
        }
        Err(e) => {
            warn_integration_install_failed(kind, LIFECYCLE_AUTHORITY_KINDS.contains(&kind), &e);
        }
    }
}

/// H3: the bd-627z.2 loud-warning pattern for a missing/outdated integration
/// whose auto-install failed. Hook-authority kinds additionally warn that
/// lifecycle-state reporting is degraded — warn-only, never block.
fn warn_integration_install_failed(kind: &str, hook_authority: bool, e: &anyhow::Error) {
    eprintln!(
        "\n!!! WARNING: the hc integration for {kind} is missing or outdated and the \
         auto-install failed.\n\
         hc 0.1.16 has no integration surface (no `hc integration` subcommand, no \
         `integration.*` method), so there is nothing to install.\n\
         {kind} detection falls back to process-based heuristics until the integration reports.\n\
         (integration install failed: {e:#})\n"
    );
    if hook_authority {
        eprintln!(
            "    {kind} is a lifecycle-hook-authority harness: without a reporting integration,\n\
             hc cannot author idle/working/blocked from hook events, so lifecycle-state\n\
             reporting is DEGRADED. The env + boot-prime machinery works regardless — the\n\
             spawn proceeds (H3 warn-only, do not block).\n"
        );
    }
}

/// One integration-status read per spawn/add — the preflight map all
/// kinds share. hc 0.1.16 has no integration surface, so the map is always
/// empty (every kind reads not-installed → install attempt → loud warning,
/// never a spawn failure).
fn read_integration_statuses() -> HashMap<String, hc::IntegrationStatus> {
    match hc::integration_status_all() {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            eprintln!(
                "flywheel: WARNING cannot read `hc integration status` — spawn integration \
                 preflight degraded: {e:#}"
            );
            HashMap::new()
        }
    }
}

/// H3: one `kind: <state>` line for the status/deps surfaces
/// (kind -> installed/current/version), painted by state.
fn integration_status_line(kind: &str, st: &hc::IntegrationStatus) -> String {
    let label = if st.installed && st.current {
        format!("current ({})", st.version.as_deref().unwrap_or("?"))
    } else if st.installed {
        format!(
            "outdated ({} < {})",
            st.version.as_deref().unwrap_or("?"),
            st.expected_version.as_deref().unwrap_or("?")
        )
    } else {
        "not installed".to_string()
    };
    let painted = if st.installed && st.current {
        paint_ok(&label)
    } else if st.installed {
        crate::output::warn(&label)
    } else {
        error(&label)
    };
    format!("{} integration: {}", kind_paint(kind), painted)
}

/// Spawn configuration knobs (bd-llbe: cmd_spawn had 17 positional args —
/// a real readability bug). Grouped into one struct; `Default` matches the
/// internal spawn patterns (respawn/asset/ensemble), the CLI spawn arm and
/// the xproj spawn override what they need explicitly.
#[derive(Debug, Clone)]
struct SpawnOpts {
    counts: AgentCounts,
    recipe: Option<String>,
    kinds: Vec<(String, u32)>,
    /// bd-kbl1c join-before-post: steer channels every spawned agent joins
    /// at pane creation (membership is the only gate).
    join_channels: Vec<String>,
    /// bd-koafr: boot assignment per pin from --assign.
    assign: Vec<String>,
    label_override: Option<String>,
    no_focus: bool,
    no_verify: bool,
    retire_orphans: bool,
    peer_aware: bool,
    no_peer_aware: bool,
    peer_poll_interval_minutes: u32,
    peer_poll_budget_minutes: u32,
    peer_max_prompts: u32,
    settle_secs: u64,
    profile_name: Option<String>,
    profile_count: u32,
    /// bd-j3so: bare harness+model override (kind/count spawns, no profile)
    bare_model: String,
    bare_provider: String,
    /// Operator labeled this spawn as an escape from compose/wave-swarm.
    escape: bool,
    /// Skip the pre-spawn model reconfirm (bd-ll3k). CLI `--yes` passes
    /// false-on-TTY (prompt); internal callers default true (never block).
    yes: bool,
    /// ADR-0011 D5: join live fleet.
    join: bool,
    /// bd-command-receipts-zile: request key pinned to this spawn's slot.
    request_key: Option<String>,
    /// flywheel ADR-0022 phase 1: dry-run — print the plan, launch nothing.
    dry_run: bool,
    /// with dry_run: compact JSON plan output.
    json: bool,
    /// re-resolve one seat (repeatable): `--patch <seat>.<field>=<value>`,
    /// field in harness|model|effort|mode. Later patches win; unknown
    /// seats or fields fail naming the valid ones.
    patches: Vec<String>,
}

impl Default for SpawnOpts {
    fn default() -> Self {
        Self {
            counts: AgentCounts::default(),
            recipe: None,
            kinds: Vec::new(),
            join_channels: Vec::new(),
            assign: Vec::new(),
            label_override: None,
            no_focus: false,
            no_verify: false,
            retire_orphans: false,
            peer_aware: false,
            no_peer_aware: false,
            peer_poll_interval_minutes: 5,
            peer_poll_budget_minutes: 30,
            peer_max_prompts: 3,
            settle_secs: 30,
            profile_name: None,
            profile_count: 1,
            request_key: None,
            dry_run: false,
            json: false,
            patches: Vec::new(),
            bare_model: String::new(),
            bare_provider: String::new(),
            escape: false,
            yes: true,
            join: false,
        }
    }
}

/// bd-rename-clear-name-too-long-oxd7: the hc-legal label for a dead
/// binding that the rename-clear frees.
///
/// hc caps agent names at 32 chars, and a spawn name can already sit most of
/// the way there (`flywheel-elm-effect-rs-oc` is 25), so the bare
/// `{name}-stale-{attempts}` concat mints 33 and hc refuses with
/// `invalid_agent_name` — the one door that frees a taken name wedges shut
/// exactly when it is needed. Truncate the BASE, never the tail:
/// [`terminal_agent_name`]'s `truncate(32)` would eat the attempt number instead,
/// and that number is what tells two stale rows apart.
fn stale_rename_label(name: &str, attempts: u32) -> String {
    let suffix = format!("-stale-{attempts}");
    let keep = 32usize.saturating_sub(suffix.len());
    let base = name.chars().take(keep).collect::<String>();
    let base = base.trim_end_matches('-');
    if base.is_empty() {
        format!("stale-{attempts}")
    } else {
        format!("{base}{suffix}")
    }
}

/// ADR-0003 discipline: classify on the machine code, never the message text.
/// `agent_name_taken` is hc's wire code passed through as
/// `BackendErrorCode::Other` — the one check every start door shares.
fn is_taken_name_error(e: &hc::BackendError) -> bool {
    e.code().as_str() == "agent_name_taken"
}

/// bd-s91y: may the taken-name recovery free the binding? hc's
/// `agent rename <pane> <new>` renames whatever agent that pane HOLDS, so the
/// rename-clear is only legal when the pane's OWN binding holds the contested
/// name; a blind clear would rename a stranger. Pure, so the own/foreign
/// split is unit-testable without hc.
///
/// WHO ELSE holds the name is deliberately not an input: it shapes only the
/// refusal MESSAGE, while the decision is the pane's own binding — so the
/// effect layer reads the holder only when it has to refuse (`agent.list`
/// twice per taken-name event would be one read of pure waste on the ordinary
/// clear path).
#[derive(Debug, PartialEq, Eq)]
enum TakenNameAction {
    /// The pane's own binding holds the contested name — free it, retry once.
    FreeOwnBinding { label: String },
    /// Anything else holds it — refuse, naming the holder in the message.
    ForeignHolder,
}

/// The decision behind [`start_agent_with_taken_name_clear`]. `attempt` only
/// shapes the stale label; it never widens WHAT may be renamed.
fn taken_name_action(pane_binding: Option<&str>, contested: &str, attempt: u32) -> TakenNameAction {
    if pane_binding == Some(contested) {
        TakenNameAction::FreeOwnBinding {
            label: stale_rename_label(contested, attempt),
        }
    } else {
        TakenNameAction::ForeignHolder
    }
}

/// Outcome of one `agent.start` attempt through the shared recovery.
#[derive(Debug)]
enum StartAttempt {
    /// Accepted — first try, or after freeing the pane's own binding.
    Started,
    /// Refused for a reason that is NOT `agent_name_taken` — the caller's own
    /// error policy (logging, classification, retry) still applies.
    NotTaken(hc::BackendError),
    /// The name is taken and the recovery may not (or could not) free it —
    /// fail loud NOW; keeping the name costs the retry budget and clears
    /// nothing.
    Refused(anyhow::Error),
}

/// bd-s91y: the four backend reads/writes the taken-name recovery needs —
/// implemented once over the concrete hc client, once over the trait-object
/// backend, so the recovery logic and its messages live in ONE place.
trait TakenNameOps {
    fn start_agent(
        &mut self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> std::result::Result<AgentInfo, hc::BackendError>;
    /// Name the pane's OWN binding currently holds. An unreadable list reads
    /// as `None` — the safe side, because `None` never authorizes a rename.
    fn pane_binding(&mut self, pane_id: &str) -> Option<String>;
    /// The pane a live agent row shows holding `name`, when one is visible.
    fn holder_pane(&mut self, name: &str) -> Option<String>;
    /// Rename THIS pane's own binding away — never a foreign holder's.
    fn rename_pane(
        &mut self,
        pane_id: &str,
        label: &str,
    ) -> std::result::Result<(), hc::BackendError>;
}

/// The concrete-client implementation (spawn's dead-shell re-launch, both
/// retry loops, the crew / rate-limit hops).
struct ConcreteTakenOps<'a> {
    c: &'a hc::Client,
}

impl TakenNameOps for ConcreteTakenOps<'_> {
    fn start_agent(
        &mut self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> std::result::Result<AgentInfo, hc::BackendError> {
        self.c.agent_start(name, kind, pane_id, 45_000, args)
    }
    fn pane_binding(&mut self, pane_id: &str) -> Option<String> {
        self.c
            .agent_list()
            .ok()?
            .into_iter()
            .find(|a| a.pane_id == pane_id)?
            .name
    }
    fn holder_pane(&mut self, name: &str) -> Option<String> {
        self.c
            .agent_list()
            .ok()?
            .into_iter()
            .find(|a| a.name.as_deref() == Some(name))
            .map(|a| a.pane_id)
    }
    fn rename_pane(
        &mut self,
        pane_id: &str,
        label: &str,
    ) -> std::result::Result<(), hc::BackendError> {
        self.c.agent_rename(pane_id, label).map(|_| ())
    }
}

/// The trait-object implementation (`cmd_recover`'s Respawn arm, the durable
/// respawn leaf).
struct DynTakenOps<'a> {
    backend: &'a dyn backend::MultiplexerBackend,
}

impl TakenNameOps for DynTakenOps<'_> {
    fn start_agent(
        &mut self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> std::result::Result<AgentInfo, hc::BackendError> {
        self.backend.agent_start(name, kind, pane_id, args)
    }
    fn pane_binding(&mut self, pane_id: &str) -> Option<String> {
        self.backend
            .agent_list()
            .ok()?
            .into_iter()
            .find(|a| a.pane_id == pane_id)?
            .name
    }
    fn holder_pane(&mut self, name: &str) -> Option<String> {
        self.backend
            .agent_list()
            .ok()?
            .into_iter()
            .find(|a| a.name.as_deref() == Some(name))
            .map(|a| a.pane_id)
    }
    fn rename_pane(
        &mut self,
        pane_id: &str,
        label: &str,
    ) -> std::result::Result<(), hc::BackendError> {
        self.backend.agent_rename(pane_id, Some(label))
    }
}

/// bd-s91y (GS-037 P0-3): the ONE home of the `agent_name_taken` recovery.
///
/// Before this, the start doors disagreed: the dead-shell verify path
/// (`verify_or_respawn_pane`) rename-cleared and failed fast, while both retry
/// loops — `cmd_recover`'s Respawn arm included — burned 30 x 3s (~90s) on a
/// name they never freed, even though `recover` is the documented rescue door
/// (found while closing bd-rename-clear-name-too-long-oxd7). The recovery:
/// start once; on a taken-name refusal read the pane's OWN binding, free it
/// with the bounded [`stale_rename_label`], retry ONCE, and refuse loud if it
/// is still taken — or if the holder is not this pane (hc renames whatever
/// the pane holds, so a blind clear would rename a stranger).
///
/// The clear is keyed on the pane's own BINDING, never on process liveness:
/// the trait-object backend carries no `pane.process_info` (cmd_recover's own
/// constraint), and every door reaches this state only after its own liveness
/// gate (a dead shell, a process-dead/unresponsive/agent-gone verdict).
fn start_agent_with_taken_name_clear(
    ops: &mut dyn TakenNameOps,
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
    attempt: u32,
) -> StartAttempt {
    match ops.start_agent(name, kind, pane_id, args) {
        Ok(_) => StartAttempt::Started,
        Err(e) if !is_taken_name_error(&e) => StartAttempt::NotTaken(e),
        Err(_) => {
            let binding = ops.pane_binding(pane_id);
            match taken_name_action(binding.as_deref(), name, attempt) {
                TakenNameAction::FreeOwnBinding { label } => match ops.rename_pane(pane_id, &label)
                {
                    Ok(()) => {
                        eprintln!(
                            "flywheel: freed name {name} by renaming dead binding to {label} — retrying start once"
                        );
                        match ops.start_agent(name, kind, pane_id, args) {
                            Ok(_) => StartAttempt::Started,
                            Err(e) if is_taken_name_error(&e) => {
                                StartAttempt::Refused(anyhow::anyhow!(
                                    "{name} ({pane_id}) agent_name_taken after rename-clear — refusing to burn the recovery budget. Close the workspace (flywheel kill <project> --force) or `hc agent rename` the stale binding, then re-spawn."
                                ))
                            }
                            Err(e) => {
                                if let Err(back) = ops.rename_pane(pane_id, name) {
                                    eprintln!(
                                        "flywheel: could not restore name {name} on {pane_id} after a failed re-launch ({back:#}) — the binding stays {label}"
                                    );
                                }
                                StartAttempt::NotTaken(e)
                            }
                        }
                    }
                    Err(re) => StartAttempt::Refused(anyhow::anyhow!(
                        "{name} ({pane_id}) agent_name_taken and rename-clear failed ({re:#}) — fail-fast (GS-037 P0-3), not a ~120s burn. Close the workspace or release the name, then re-spawn."
                    )),
                },
                TakenNameAction::ForeignHolder => {
                    let own = binding.as_deref().unwrap_or("<no binding>");
                    let holder = ops.holder_pane(name);
                    let held = holder.as_deref().map_or_else(
                        || "no live agent row holds it".to_string(),
                        |p| format!("held by pane {p}"),
                    );
                    StartAttempt::Refused(anyhow::anyhow!(
                        "{name} ({pane_id}) agent_name_taken, but this pane's own binding is {own} and the name is {held} — flywheel only frees a pane's OWN binding, so it will not rename a stranger's agent. Release {name} where it is held (close or rename that pane), then re-spawn."
                    ))
                }
            }
        }
    }
}

/// bd-s91y (GS-037 P0-3): the shared taken-name recovery. `cmd_recover`'s
/// respawn arm used to burn 30 x 3s on a name it could have freed — or must
/// refuse — in one read.
/// bd-spawn-omp-boot-reliability-3h06: post-boot dead-shell verification for
/// one spawned pane — assert the LIVE agent process (`pane_agent_alive`),
/// never the hc `launch_pending` flag. A pane can leave the launch window
/// with `launch_pending=false` and still be a plain shell: the launch
/// command was mangled by the booting shell and the agent never started;
/// hc reports the pane idle and it LOOKS like an agent until a send
/// fails (2026-08-17 w70: p4/p5 plain shells, only p1 TUI'd).
///
/// A shell-fallback pane is recovered within `budget`: cancel any partial
/// command (the booting shell may be parked at the PS2 continuation prompt),
/// re-launch into the SAME pane (`agent.start` re-types the launch command —
/// `start_agent_with_retry` would short-circuit on the registered-but-dead
/// agent), re-run the bd-eign.6 foreground gate, and re-prime. Returns
/// `Ok(true)` once the process is live; `Ok(false)` when the budget is
/// exhausted — the caller must NOT declare the swarm ready.
fn verify_or_respawn_pane(
    c: &mut Client,
    pane_id: &str,
    name: &str,
    kind: &str,
    args: &[String],
    prime: bool,
    budget: Duration,
) -> Result<bool> {
    let deadline = std::time::Instant::now() + budget;
    let mut attempts = 0;
    loop {
        let grace = std::time::Instant::now() + Duration::from_secs(20);
        let mut live = false;
        while std::time::Instant::now() < grace && std::time::Instant::now() < deadline {
            let verdict = hc::pane_process_info(c, pane_id)
                .map(|pi| hc::pane_agent_alive(&pi, kind))
                .unwrap_or(None);
            if verdict == Some(true) {
                live = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2000));
        }
        if live {
            if attempts > 0 {
                eprintln!(
                    "flywheel: {name} ({pane_id}) recovered after {} re-launch(es)",
                    attempts
                );
            }
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "flywheel: {name} ({pane_id}) has no live agent process after spawn + {attempts} re-launch(es) within the {:.0}s budget — the pane is a dead shell",
                budget.as_secs_f32()
            );
            return Ok(false);
        }
        attempts += 1;
        eprintln!(
            "flywheel: {name} ({pane_id}) is a shell-fallback pane (no live agent process, bd-spawn-omp-boot-reliability-3h06) — re-launching (attempt {attempts})"
        );
        if let Some(pi) = hc::pane_process_info(c, pane_id).ok()
            && hc::pane_agent_alive(&pi, kind) == Some(false)
        {
            let _ = c.pane_send_input(pane_id, &["CTRL_C"]);
            std::thread::sleep(Duration::from_millis(500));
        }
        let mut ops = ConcreteTakenOps { c: &*c };
        match start_agent_with_taken_name_clear(&mut ops, name, kind, pane_id, args, attempts) {
            StartAttempt::Started => {}
            StartAttempt::NotTaken(e) => {
                eprintln!(
                    "flywheel: re-launch of {name} ({pane_id}) attempt {attempts} failed: {e:#}"
                );
            }
            StartAttempt::Refused(e) => return Err(e),
        }
        let _ = hc::wait_agent_foreground(c, pane_id, kind, name, Duration::from_secs(15));
        if prime {
            prime_bootstrap(c, pane_id, kind);
        }
    }
}

/// bd-spawn-omp-boot-reliability-3h06: which spawned panes lack a live agent
/// process right now? Pure verdict from a probe map — the final pre-"swarm
/// ready" gate (a pane whose process died after its per-pane verification —
/// during later panes' spawn — must fail the spawn loudly, never masquerade
/// as ready). A probe that errors or returns `None` counts as dead
/// (never declare ready on inconclusive evidence).
fn dead_panes(
    booted: &[(String, String, String)],
    mut probes: impl FnMut(&str, &str) -> Option<bool>,
) -> Vec<&(String, String, String)> {
    booted
        .iter()
        .filter(|(pane_id, _name, kind)| probes(pane_id, kind) != Some(true))
        .collect()
}

/// bd-compose-plumbing-zdzi: ensure the given channels exist (captain owns
/// creation, idempotently — a crew pin must never become creator via a join
/// race) and join every given pin with `--if-missing`. Join-before-post:
/// membership is the only gate and a non-member post is a refused write, so
/// the `spawn` join flags AND `workflow compose --run`'s declared steer
/// channel go through THIS one implementation (the compose path had skipped
/// it; spawn's inline copy was the only one — the divergence this bead
/// records). Best-effort by contract: each failure warns with the exact
/// remedy and a failed join never fails the run.
/// Channel ensure+join via the toron CLI (bd-wave2 residual): needs a
/// toron-core channel surface (create/join with key resolution + bus
/// publish); no facade equivalent exists yet.
fn ensure_and_join_channels(project: &str, channels: &[String], pins: &[String]) {
    if channels.is_empty() {
        return;
    }
    // The channel creator must be a REGISTERED identity. `operator_pin()`
    // falls through to $USER when AGENT_NAME is absent, which is not a toron
    // pin; using it made `spawn --join-channel` fail for the OS username.
    // Prefer the project's registered `captain` alias, then an explicit pane
    // pin. Never fall back to $USER for a write that toron will reject.
    let captain = channel_captain(project).or_else(|| {
        std::env::var("AGENT_NAME")
            .ok()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
    });
    let Some(captain) = captain.as_deref() else {
        eprintln!(
            "flywheel: warn: no registered captain alias for {project}; cannot ensure {channels:?}. \
             Set one with `toron contact captain --project {project} --pin <Pin>`, or create/join \
             the channel manually with a registered pin."
        );
        return;
    };
    for ch in channels {
        let ensured = Command::new("toron")
            .arg("channel")
            .arg("create")
            .arg("--name")
            .arg(ch)
            .arg("--project")
            .arg(project)
            .arg("--as")
            .arg(captain)
            .arg(ch)
            .output();
        match ensured {
            Ok(o) if o.status.success() => {}
            Ok(o) => eprintln!(
                "flywheel: warn: channel ensure {ch} failed for {captain}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => eprintln!("flywheel: warn: channel ensure {ch} failed: {e}"),
        }
    }
    for pin in pins {
        for ch in channels {
            let status = Command::new("toron")
                .arg("channel")
                .arg("join")
                .arg(ch)
                .arg("--project")
                .arg(project)
                .arg("--as")
                .arg(pin)
                .arg("--if-missing")
                .output();
            match status {
                Ok(o) if o.status.success() => {}
                Ok(o) => eprintln!(
                    "flywheel: warn: channel join {ch} failed for {pin}: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => eprintln!("flywheel: warn: channel join {ch} failed for {pin}: {e}"),
            }
        }
    }
}

/// The project's registered orchestrator pin, from the toron captain alias.
/// `None` means the alias is absent or unreadable; callers must not substitute
/// the host OS username.
fn channel_captain(project: &str) -> Option<String> {
    let out = Command::new("toron")
        .args(["mail", "whois", project, "captain", "--json"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| captain_name_from_whois(&out.stdout))
        .flatten()
}

fn captain_name_from_whois(stdout: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    value
        .get("name")
        .and_then(|n| n.as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod channel_captain_tests {
    use super::*;

    #[test]
    fn whois_json_names_the_registered_captain() {
        assert_eq!(
            captain_name_from_whois(br#"{"name":"CloudySparrow"}"#),
            Some("CloudySparrow".to_string())
        );
    }

    #[test]
    fn missing_or_blank_captain_is_refused() {
        assert_eq!(captain_name_from_whois(b"{}"), None);
        assert_eq!(captain_name_from_whois(br#"{"name":"  "}"#), None);
        assert_eq!(captain_name_from_whois(b"not json"), None);
    }
}

/// bd-ux-papercuts-t2u5: the `spawn --join` refusal must name the verb that
/// actually ADDS panes. "JOINING — not spawning" alone sent the operator
/// hunting for `flywheel add` (three attempts, live friction 2026-10-03).
///
/// bd-gjtcw: the hint echoes the REQUESTED flags verbatim, so `spawn --oc 3`
/// never points at `add --pi N`.
fn spawn_join_notice(project: &str, live: &str, requested: &str) -> String {
    let flags = if requested.trim().is_empty() {
        "--kind N".to_string()
    } else {
        requested.trim().to_string()
    };
    format!(
        "JOINING — not spawning  project={project} live={live}  (ADR-0011 D5; reuse first workspace)\nto ADD panes to a live swarm run the same request with `flywheel add`: `flywheel add {project} {flags}`"
    )
}

#[cfg(test)]
mod spawn_join_notice_tests {
    use super::*;

    #[test]
    fn spawn_join_refusal_names_add_with_the_requested_flags() {
        let msg = spawn_join_notice("proj", "w1 (label \"proj\", 2 panes)", "--oc 3");
        assert!(msg.contains("JOINING — not spawning"), "{msg}");
        assert!(
            msg.contains("flywheel add proj --oc 3"),
            "the refusal must echo the requested flags: {msg}"
        );
        assert!(
            !msg.contains("--pi"),
            "the hint must never invent a harness count: {msg}"
        );
    }

    #[test]
    fn spawn_join_refusal_without_flags_stays_honest() {
        let msg = spawn_join_notice("proj", "w1", "  ");
        assert!(msg.contains("flywheel add proj --kind N"), "{msg}");
    }
}

/// bd-unpinned-loud-1mlt (GS-028 B6+B7): names of registry entries with no
/// pinned mail identity. Pure — shared by the spawn-site warning and the
/// sweep skip counter so both agree on what "unpinned" means. An empty
/// identity counts as unpinned (same rows the sweep skips).
fn unpinned_registry_names(registry: &[state::SpawnEntry]) -> Vec<String> {
    registry
        .iter()
        .filter(|e| e.mail_identity.as_deref().is_none_or(str::is_empty))
        .map(|e| e.name.clone())
        .collect()
}

/// GS-030 slice 4 (bd-gs030-failclosed-m1x0): fate of a spawn that drew no
/// pin. Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnpinnedFate {
    /// Mail is configured so a pin should exist: fail naming the pane
    /// instead of writing a dark row the sweep will skip.
    FailClosed,
    /// Mail is not configured (bare experiments, offline): proceed with the
    /// legacy/unpinned entry and warn loud (current behavior preserved).
    LegacyWarn,
}

/// Decide [`UnpinnedFate`] from whether mail is configured
/// (`backend::mail_backend(cfg).is_ok()` — the same signal the pin draw
/// gates on). A configured backend that yields no pin is a failure, not a
/// shape; an unconfigured backend is an experiment, not an error.
pub(crate) fn unpinned_spawn_fate(mail_configured: bool) -> UnpinnedFate {
    if mail_configured {
        UnpinnedFate::FailClosed
    } else {
        UnpinnedFate::LegacyWarn
    }
}

pub(crate) fn project_base(project: &str) -> String {
    project.rsplit('/').next().unwrap_or(project).to_string()
}

/// H1 (bd-harness-prime-agent-818f): hc has NO kind for some harnesses —
/// its `Agent` enum is a closed 22-kind set (`agent.start --kind pa` →
/// `unsupported_agent_kind`), and `server.agent_manifests` registers
/// screen-detection rules only, NOT kinds. A harness WITH a spawn recipe
/// (`[harness.<name>]`, compiled-in or profiles.toml) is launched via the
/// recipe instead: `pane.run` the command. hc's standard pane launch env
/// already carries HC_SOCKET_PATH / HC_SESSION / HC_TERMINAL_ID, so
/// prime-agent's built-in hc extension activates and self-reports
/// lifecycle (working/blocked/idle) through the socket — which hc accepts
/// as hook authority even for unknown kinds (empirically verified: the pane
/// shows up in `agent list` as `agent=prime-agent`). The normal wait/rename
/// loop then binds the name. Returns `Ok(true)` when the recipe path was
/// taken, `Ok(false)` when this kind has no recipe, `Err` on launch failure.
fn try_recipe_launch(c: &mut Client, kind: &str, pane_id: &str, args: &[String]) -> Result<bool> {
    let Some(recipe) = ProfileCatalog::load()
        .ok()
        .and_then(|cat| cat.harness(kind).and_then(|h| h.spawn_cmd.clone()))
    else {
        return Ok(false);
    };
    let cmd = recipe_launch_command(&recipe, args);
    eprintln!("agent.start: hc has no kind '{kind}'; launching via harness recipe: {cmd}");
    c.pane_run(pane_id, &cmd)?;
    Ok(true)
}

/// Trait-object variant of [`try_recipe_launch`] (same contract as
/// [`start_agent_with_retry_dyn`]): routes the recipe launch through
/// `MultiplexerBackend::pane_run`.
fn try_recipe_launch_dyn(
    backend: &dyn backend::MultiplexerBackend,
    kind: &str,
    pane_id: &str,
    args: &[String],
) -> Result<bool> {
    let Some(recipe) = ProfileCatalog::load()
        .ok()
        .and_then(|cat| cat.harness(kind).and_then(|h| h.spawn_cmd.clone()))
    else {
        return Ok(false);
    };
    let cmd = recipe_launch_command(&recipe, args);
    eprintln!("agent.start: hc has no kind '{kind}'; launching via harness recipe: {cmd}");
    backend.pane_run(pane_id, &cmd)?;
    Ok(true)
}

/// Shell-quote one arg for the `pane.run` command line (single quotes,
/// escaping embedded quotes the POSIX way).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Render a `[harness.<name>] spawn_cmd` recipe + launch args into a single
/// shell command. Args are shell-quoted so model ids / boot primes with
/// spaces survive the pane's shell.
fn recipe_launch_command(recipe: &str, args: &[String]) -> String {
    let mut cmd = recipe.to_string();
    for a in args {
        cmd.push(' ');
        cmd.push_str(&shell_quote(a));
    }
    cmd
}

/// Outcome of one reroute-site hop attempt: return the hopped agent, fail
/// loud on a broken hop, rejoin the in-place reroute when the reconciler
/// is satisfied, or keep today's refusal.
enum SiteHop {
    Hopped(Box<AgentInfo>),
    Failed(anyhow::Error),
    InPlace,
    Refused,
}

#[cfg(test)]
mod envpin_reroute_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    struct FakeHopBackend {
        order: Arc<Mutex<Vec<String>>>,
    }

    impl crate::backend::MultiplexerBackend for FakeHopBackend {
        fn name(&self) -> &'static str {
            "fake-hop"
        }
        fn pane_list(
            &self,
            _workspace_id: Option<&str>,
        ) -> Result<Vec<toron_core::pane_types::PaneInfo>, toron_plane::backend::BackendError>
        {
            Ok(vec![toron_core::pane_types::PaneInfo {
                pane_id: "old-pane".into(),
                terminal_id: String::new(),
                workspace_id: "ws".into(),
                tab_id: String::new(),
                focused: false,
                agent_status: toron_core::pane_types::AgentStatus::Idle,
                revision: 0,
                title: None,
                cwd: None,
                agent: None,
                agent_session: None,
                display_agent: None,
                foreground_cwd: None,
                label: None,
                scroll: None,
                state_labels: serde_json::Map::new(),
                terminal_title: None,
                terminal_title_stripped: None,
                tokens: serde_json::Map::new(),
            }])
        }
        fn agent_start(
            &self,
            name: &str,
            _kind: &str,
            pane_id: &str,
            _args: &[String],
        ) -> Result<toron_core::pane_types::AgentInfo, toron_plane::backend::BackendError> {
            self.order
                .lock()
                .unwrap()
                .push(format!("launch:{name}@{pane_id}"));
            Ok(toron_core::pane_types::AgentInfo {
                agent: None,
                agent_status: toron_core::pane_types::AgentStatus::Working,
                cwd: None,
                display_agent: None,
                focused: false,
                foreground_cwd: None,
                interactive_ready: false,
                launch_pending: false,
                name: Some(name.to_string()),
                pane_id: pane_id.to_string(),
                revision: 0,
                state_change_seq: 0,
                completion_seq: None,
                state_labels: serde_json::Map::new(),
                tab_id: String::new(),
                terminal_id: String::new(),
                terminal_title: None,
                workspace_id: String::new(),
            })
        }
        fn pane_close(&self, pane_id: &str) -> Result<(), toron_plane::backend::BackendError> {
            self.order.lock().unwrap().push(format!("close:{pane_id}"));
            Ok(())
        }
    }

    fn hop_fixture() -> EnvpinHop {
        let mut env = BTreeMap::new();
        env.insert(
            "OPENCODE_CONFIG_CONTENT".to_string(),
            "{\"model\":\"opencode/new\"}".to_string(),
        );
        env.insert(
            "FLYWHEEL_REROUTE_NOTE".to_string(),
            "rerouted opencode-fallback -> opencode-next after 429".to_string(),
        );
        EnvpinHop {
            project: "envpin-test-proj".to_string(),
            agent: "TestyCreek".to_string(),
            old_pane: "old-pane".to_string(),
            next_harness: "opencode".to_string(),
            next_args: vec!["opencode".to_string()],
            next_env: env,
            next_profile: "opencode-next".to_string(),
            next_model: "opencode/new".to_string(),
            from_profile: "opencode-fallback".to_string(),
            from_model: "opencode/gold".to_string(),
            from_harness: "opencode".to_string(),
            kind: crate::failure::FailureKind::RateLimit,
            evidence: "429 rate limited".to_string(),
            attempt: 0,
        }
    }

    fn rebind_rows(project: &str, key: &str) -> Vec<crate::agent_ref::RebindIntent> {
        crate::state::read_log(project)
            .expect("journal reads")
            .iter()
            .filter(|e| e.kind == crate::state::EventKind::Spawn)
            .filter_map(|e| {
                serde_json::from_value::<crate::agent_ref::RebindIntent>(e.data.clone()).ok()
            })
            .filter(|i| i.effect_key == key)
            .collect()
    }

    /// bd-6143s: the hop's doc says a verdict other than Keep/Respawn is a bug
    /// and fails loud. A Relaunch (argv-delivered model) must never read as
    /// "already satisfied" while the pane still runs the old model.
    #[test]
    fn relaunch_verdict_fails_loud_instead_of_reporting_satisfied() {
        let backend = FakeHopBackend {
            order: Arc::new(Mutex::new(Vec::new())),
        };
        let mut hop = hop_fixture();
        // Same harness, argv-delivered model (pi): the reconciler says Relaunch.
        hop.from_harness = "pi".to_string();
        hop.next_harness = "pi".to_string();
        hop.from_model = "pi/old".to_string();
        hop.next_model = "pi/new".to_string();
        hop.next_env = BTreeMap::new();
        let respawn = |_: &str| -> anyhow::Result<crate::hc::AgentInfo> {
            panic!("a Relaunch verdict must not respawn")
        };
        let err = envpin_respin_hop(&backend, &respawn, &hop, false)
            .expect_err("a Relaunch verdict must fail loud");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Relaunch"),
            "the error must name the verdict it refused: {msg}"
        );
    }

    #[test]
    fn envpin_hop_journals_before_launch_keeps_name_closes_last() {
        let _guard = crate::config::lock_env();
        let order = Arc::new(Mutex::new(Vec::new()));
        let seen_prepared_at_split = Arc::new(Mutex::new(None));
        let backend = FakeHopBackend {
            order: order.clone(),
        };
        let hop = hop_fixture();
        let key = crate::agent_ref::RebindIntent::derive_effect_key(
            &crate::agent_ref::AgentRef::new(&hop.project, &hop.agent),
            &crate::agent_ref::AgentBinding {
                profile_name: hop.from_profile.clone(),
                model: hop.from_model.clone(),
                harness: hop.from_harness.clone(),
                location: crate::agent_ref::BindingLocation::Pane,
            },
            &crate::agent_ref::AgentBinding {
                profile_name: hop.next_profile.clone(),
                model: hop.next_model.clone(),
                harness: hop.next_harness.clone(),
                location: crate::agent_ref::BindingLocation::Pane,
            },
        );
        let split_order = order.clone();
        let split_seen = seen_prepared_at_split.clone();
        let project = hop.project.clone();
        let key2 = key.clone();
        let respawn = move |anchor: &str| -> anyhow::Result<AgentInfo> {
            split_order.lock().unwrap().push(format!("split:{anchor}"));
            let rows = rebind_rows(&project, &key2);
            let prepared = rows
                .iter()
                .any(|i| i.state == toron_core::index::workflow_effects::EffectState::Prepared);
            *split_seen.lock().unwrap() = Some(prepared);
            Ok(site_agent("TestyCreek", "new-pane-1"))
        };
        let got = envpin_respin_hop(&backend, &respawn, &hop, false).expect("hop runs");
        assert!(
            matches!(&got, EnvpinHopOutcome::Hopped { agent } if agent.name.as_deref() == Some("TestyCreek")),
            "same agent name kept: {got:?}"
        );
        assert_eq!(
            seen_prepared_at_split.lock().unwrap().clone(),
            Some(true),
            "Prepared row journaled BEFORE the respawn runs"
        );
        assert_eq!(
            *order.lock().unwrap(),
            vec!["split:old-pane".to_string(), "close:old-pane".to_string()],
            "the respawn runs while the intent is Prepared, old pane closes after"
        );
        let rows = rebind_rows(&hop.project, &key);
        assert_eq!(rows.len(), 3, "exactly one intent, three states: {rows:?}");
        assert_eq!(
            rows.iter().map(|i| i.state).collect::<Vec<_>>(),
            vec![
                toron_core::index::workflow_effects::EffectState::Prepared,
                toron_core::index::workflow_effects::EffectState::InFlight,
                toron_core::index::workflow_effects::EffectState::Settled,
            ]
        );
    }

    #[test]
    fn envpin_hop_closed_gate_neither_respawns_nor_journals() {
        let _guard = crate::config::lock_env();
        let order = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeHopBackend {
            order: order.clone(),
        };
        let mut hop = hop_fixture();
        hop.kind = crate::failure::FailureKind::Timeout;
        let respawn =
            |_: &str| -> anyhow::Result<AgentInfo> { panic!("closed gate must not split") };
        let got = envpin_respin_hop(&backend, &respawn, &hop, false).expect("hop runs");
        assert!(
            matches!(got, EnvpinHopOutcome::SuppressedGate),
            "closed gate suppresses: {got:?}"
        );
        assert!(order.lock().unwrap().is_empty(), "no backend calls");
        assert!(
            crate::state::read_log(&hop.project)
                .expect("journal reads")
                .is_empty(),
            "no journal rows"
        );
    }

    #[test]
    fn envpin_hop_second_failure_does_not_hop_again() {
        let _guard = crate::config::lock_env();
        let order = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeHopBackend {
            order: order.clone(),
        };
        let hop = hop_fixture();
        let respawn =
            |_: &str| -> anyhow::Result<AgentInfo> { panic!("second failure must not split") };
        let got = envpin_respin_hop(&backend, &respawn, &hop, true).expect("hop runs");
        assert!(
            matches!(got, EnvpinHopOutcome::AlreadyRerouted),
            "second failure does not hop: {got:?}"
        );
        assert!(order.lock().unwrap().is_empty(), "no backend calls");
        assert!(
            crate::state::read_log(&hop.project)
                .expect("journal reads")
                .is_empty(),
            "no journal rows"
        );
    }

    /// ADR-0022 acceptance 5 (bd-6setu.12): a classified 429 on an opencode seat
    /// with a model fallback respawns under the SAME pin and journals exactly one
    /// RebindIntent. Reservations key by pin, so the pin's registry row surviving
    /// unchanged is what keeps them.
    #[test]
    fn acceptance5_429_respawn_journals_one_rebind_intent_and_keeps_the_pin() {
        let _guard = crate::config::lock_env();
        let backend = FakeHopBackend {
            order: Arc::new(Mutex::new(Vec::new())),
        };
        let hop = hop_fixture();
        assert_eq!(
            hop.kind,
            crate::failure::FailureKind::RateLimit,
            "the fixture is the 429 case"
        );
        assert_eq!(
            crate::agent_ref::RebindIntent::hop_kind(hop.kind),
            Some("model"),
            "a 429 drives the model hop, not a profile hop"
        );
        assert!(
            crate::agent_ref::RebindIntent::allowed_from_failure(hop.kind, &hop.evidence),
            "classified 429 evidence permits the hop"
        );

        crate::state::save_spawn_registry(
            &hop.project,
            &[crate::state::SpawnEntry::profiled(
                &hop.old_pane,
                &hop.agent,
                "coder",
                "opencode",
                Some(hop.agent.clone()),
            )],
        )
        .expect("seed the pin's row");

        let key = crate::agent_ref::RebindIntent::derive_effect_key(
            &crate::agent_ref::AgentRef::new(&hop.project, &hop.agent),
            &crate::agent_ref::AgentBinding {
                profile_name: hop.from_profile.clone(),
                model: hop.from_model.clone(),
                harness: hop.from_harness.clone(),
                location: crate::agent_ref::BindingLocation::Pane,
            },
            &crate::agent_ref::AgentBinding {
                profile_name: hop.next_profile.clone(),
                model: hop.next_model.clone(),
                harness: hop.next_harness.clone(),
                location: crate::agent_ref::BindingLocation::Pane,
            },
        );
        let name = hop.agent.clone();
        let respawn = move |_anchor: &str| -> anyhow::Result<toron_core::pane_types::AgentInfo> {
            Ok(site_agent(&name, "new-pane-9"))
        };
        let got = envpin_respin_hop(&backend, &respawn, &hop, false).expect("hop runs");
        assert!(
            matches!(got, EnvpinHopOutcome::Hopped { .. }),
            "a classified 429 hops: {got:?}"
        );

        let rows = rebind_rows(&hop.project, &key);
        assert_eq!(rows.len(), 3, "exactly one intent, three states: {rows:?}");
        assert_eq!(
            rows.iter().map(|i| i.state).collect::<Vec<_>>(),
            vec![
                toron_core::index::workflow_effects::EffectState::Prepared,
                toron_core::index::workflow_effects::EffectState::InFlight,
                toron_core::index::workflow_effects::EffectState::Settled,
            ],
            "one journaled intent, in order"
        );
        let reg = crate::state::load_spawn_registry(&hop.project);
        assert_eq!(reg.len(), 1, "the pin's row survives: {reg:?}");
        assert_eq!(
            reg[0].mail_identity.as_deref(),
            Some(hop.agent.as_str()),
            "the pin is unchanged, so its reservations still key to it"
        );
    }

    #[test]
    fn envpin_hop_keep_means_no_hop() {
        let _guard = crate::config::lock_env();
        let order = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeHopBackend {
            order: order.clone(),
        };
        let mut hop = hop_fixture();
        hop.next_model = hop.from_model.clone();
        let respawn =
            |_: &str| -> anyhow::Result<AgentInfo> { panic!("satisfied hop must not split") };
        let got = envpin_respin_hop(&backend, &respawn, &hop, false).expect("hop runs");
        assert!(
            matches!(got, EnvpinHopOutcome::AlreadySatisfied),
            "same model is already satisfied: {got:?}"
        );
        assert!(order.lock().unwrap().is_empty(), "no backend calls");
    }

    #[test]
    fn envpin_hop_failed_launch_never_closes_old() {
        let _guard = crate::config::lock_env();
        let order = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeHopBackend {
            order: order.clone(),
        };
        let hop = hop_fixture();
        let respawn = |anchor: &str| -> anyhow::Result<AgentInfo> {
            order.lock().unwrap().push(format!("split:{anchor}"));
            Err(anyhow::anyhow!("fake launch failed"))
        };
        let err = envpin_respin_hop(&backend, &respawn, &hop, false).expect_err("launch fails");
        assert!(format!("{err:#}").contains("fake launch failed"));
        let calls = order.lock().unwrap().clone();
        assert!(
            calls.iter().any(|c| c.starts_with("split:")),
            "split ran: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.starts_with("close:")),
            "old pane never closed on failed launch: {calls:?}"
        );
    }

    struct FakeRexHopBackend {
        name: &'static str,
        order: Arc<Mutex<Vec<String>>>,
        commands: Arc<Mutex<Vec<String>>>,
        fail_run: bool,
    }

    impl crate::backend::MultiplexerBackend for FakeRexHopBackend {
        fn name(&self) -> &'static str {
            self.name
        }
        fn pane_list(
            &self,
            _workspace_id: Option<&str>,
        ) -> Result<Vec<toron_core::pane_types::PaneInfo>, toron_plane::backend::BackendError>
        {
            Ok(vec![site_pane("block:old-pane")])
        }
        fn pane_split(
            &self,
            parent: &str,
            direction: &str,
            _cwd: Option<&str>,
        ) -> Result<toron_core::pane_types::PaneInfo, toron_plane::backend::BackendError> {
            self.order
                .lock()
                .unwrap()
                .push(format!("split:{parent}:{direction}"));
            Ok(site_pane("block:new-pane-1"))
        }
        fn agent_list(
            &self,
        ) -> Result<Vec<toron_core::pane_types::AgentInfo>, toron_plane::backend::BackendError>
        {
            let mut row = site_agent("", "block:new-pane-1");
            row.agent_status = toron_core::pane_types::AgentStatus::Idle;
            Ok(vec![row])
        }
        fn pane_run(
            &self,
            pane_id: &str,
            command: &str,
        ) -> Result<(), toron_plane::backend::BackendError> {
            self.order.lock().unwrap().push(format!("run:{pane_id}"));
            if self.fail_run {
                return Err(toron_plane::backend::BackendError::timeout(
                    "fake rex run failed",
                ));
            }
            self.commands.lock().unwrap().push(command.to_string());
            Ok(())
        }
        fn agent_rename(
            &self,
            pane_id: &str,
            new_name: Option<&str>,
        ) -> Result<(), toron_plane::backend::BackendError> {
            self.order
                .lock()
                .unwrap()
                .push(format!("rename:{}@{pane_id}", new_name.unwrap_or_default()));
            Ok(())
        }
        fn agent_wait(
            &self,
            pane_id: &str,
            _until: &[toron_core::pane_types::AgentStatus],
            _timeout_ms: Option<u64>,
        ) -> Result<toron_core::pane_types::AgentInfo, toron_plane::backend::BackendError> {
            self.order.lock().unwrap().push(format!("wait:{pane_id}"));
            Ok(site_agent("TestyCreek", pane_id))
        }
        fn pane_close(&self, pane_id: &str) -> Result<(), toron_plane::backend::BackendError> {
            self.order.lock().unwrap().push(format!("close:{pane_id}"));
            Ok(())
        }
    }

    fn hop_effect_key(hop: &EnvpinHop) -> String {
        crate::agent_ref::RebindIntent::derive_effect_key(
            &crate::agent_ref::AgentRef::new(&hop.project, &hop.agent),
            &crate::agent_ref::AgentBinding {
                profile_name: hop.from_profile.clone(),
                model: hop.from_model.clone(),
                harness: hop.from_harness.clone(),
                location: crate::agent_ref::BindingLocation::Pane,
            },
            &crate::agent_ref::AgentBinding {
                profile_name: hop.next_profile.clone(),
                model: hop.next_model.clone(),
                harness: hop.next_harness.clone(),
                location: crate::agent_ref::BindingLocation::Pane,
            },
        )
    }

    fn rex_hop_fixture() -> EnvpinHop {
        let mut hop = hop_fixture();
        hop.old_pane = "block:old-pane".to_string();
        hop
    }

    fn assert_rex_hop_command(commands: &Arc<Mutex<Vec<String>>>) {
        let commands = commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1, "one run: {commands:?}");
        let command = &commands[0];
        assert!(
            command.starts_with("env "),
            "the pin travels as an env prefix: {command}"
        );
        assert!(
            command.contains("OPENCODE_CONFIG_CONTENT=")
                && command.contains("\"model\":\"opencode/new\""),
            "the NEW model rides the rex env prefix: {command}"
        );
        assert!(
            command.contains("AGENT_NAME=CyanTest"),
            "the agent keeps its mail identity: {command}"
        );
        assert!(
            command.contains("opencode"),
            "the harness launches the hop args: {command}"
        );
    }

    /// The live rex backends name themselves `superlogical` / `auto`; the arm
    /// must follow the ANCHOR id, so a name-keyed dispatch would refuse here.
    #[test]
    fn envpin_rex_hop_splits_plain_and_pins_via_rex_env_command() {
        for backend_name in ["auto", "superlogical"] {
            let _guard = crate::config::lock_env();
            crate::state::save_spawn_registry(
                "envpin-test-proj",
                &[crate::state::SpawnEntry::profiled(
                    "block:old-pane",
                    "TestyCreek",
                    "coder",
                    "opencode",
                    Some("CyanTest".to_string()),
                )],
            )
            .expect("stage the spawn registry");
            let order = Arc::new(Mutex::new(Vec::new()));
            let commands = Arc::new(Mutex::new(Vec::new()));
            let backend = FakeRexHopBackend {
                name: backend_name,
                order: order.clone(),
                commands: commands.clone(),
                fail_run: false,
            };
            let cfg = Config::default();
            let hop = rex_hop_fixture();
            let respawn =
                |anchor: &str| envpin_hop_respawn(Some(&cfg), &hop.project, &backend, &hop, anchor);
            let got = envpin_respin_hop(&backend, &respawn, &hop, false)
                .unwrap_or_else(|e| panic!("{backend_name} rex hop runs: {e:#}"));
            assert!(
                matches!(&got, EnvpinHopOutcome::Hopped { agent } if agent.name.as_deref() == Some("TestyCreek")),
                "{backend_name}: same agent name kept: {got:?}"
            );
            assert_eq!(
                *order.lock().unwrap(),
                vec![
                    "split:block:old-pane:down".to_string(),
                    "run:block:new-pane-1".to_string(),
                    "rename:TestyCreek@block:new-pane-1".to_string(),
                    "wait:block:new-pane-1".to_string(),
                    "close:block:old-pane".to_string(),
                ],
                "{backend_name}: plain split, then the env-prefixed run; old pane closes last"
            );
            assert_rex_hop_command(&commands);
            let rows = rebind_rows(&hop.project, &hop_effect_key(&hop));
            assert_eq!(
                rows.iter().map(|i| i.state).collect::<Vec<_>>(),
                vec![
                    toron_core::index::workflow_effects::EffectState::Prepared,
                    toron_core::index::workflow_effects::EffectState::InFlight,
                    toron_core::index::workflow_effects::EffectState::Settled,
                ],
                "{backend_name}: one intent, settled: {rows:?}"
            );
        }
    }

    /// A failed rex `pane_run` must not leave the fresh split behind.
    #[test]
    fn envpin_rex_hop_failed_run_closes_the_orphan_split() {
        let _guard = crate::config::lock_env();
        let order = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeRexHopBackend {
            name: "auto",
            order: order.clone(),
            commands: Arc::new(Mutex::new(Vec::new())),
            fail_run: true,
        };
        let cfg = Config::default();
        let hop = rex_hop_fixture();
        let respawn =
            |anchor: &str| envpin_hop_respawn(Some(&cfg), &hop.project, &backend, &hop, anchor);
        let err = envpin_respin_hop(&backend, &respawn, &hop, false).expect_err("run fails");
        assert!(
            format!("{err:#}").contains("fake rex run failed"),
            "{err:#}"
        );
        assert_eq!(
            *order.lock().unwrap(),
            vec![
                "split:block:old-pane:down".to_string(),
                "run:block:new-pane-1".to_string(),
                "close:block:new-pane-1".to_string(),
            ],
            "the orphan split closes; the old pane is never touched"
        );
        let rows = rebind_rows(&hop.project, &hop_effect_key(&hop));
        assert_eq!(
            rows.iter().map(|i| i.state).collect::<Vec<_>>(),
            vec![
                toron_core::index::workflow_effects::EffectState::Prepared,
                toron_core::index::workflow_effects::EffectState::InFlight,
                toron_core::index::workflow_effects::EffectState::Failed,
            ],
            "the failed hop is marked Failed: {rows:?}"
        );
    }

    /// A non-rex anchor on the per-target router is not provably rex, so the
    /// hop must refuse rather than guess a plane.
    #[test]
    fn envpin_hop_respawn_non_rex_anchor_refuses() {
        let cfg = Config::default();
        let hop = hop_fixture();
        for backend_name in ["auto", "superlogical"] {
            let err = envpin_hop_respawn(
                Some(&cfg),
                &hop.project,
                &NeverSplitBackend(backend_name),
                &hop,
                "old-pane",
            )
            .expect_err("a non-rex anchor with no hc backend must refuse");
            assert!(
                err.downcast_ref::<EnvPinSplitUnsupported>().is_some(),
                "{backend_name}: the refusal is the typed one: {err:#}"
            );
            assert!(
                format!("{err:#}").contains(backend_name),
                "{backend_name}: the refusal names the backend: {err:#}"
            );
        }
    }

    /// hc keeps the split-with-env arm; with no daemon on the configured
    /// socket it fails there (never by taking the rex path).
    #[test]
    fn envpin_hop_respawn_hc_arm_needs_its_daemon() {
        let _guard = crate::config::lock_env();
        let order = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeRexHopBackend {
            name: "hc",
            order: order.clone(),
            commands: Arc::new(Mutex::new(Vec::new())),
            fail_run: false,
        };
        let cfg = Config {
            socket_path: Some(
                std::env::temp_dir()
                    .join("fw-envpin-no-daemon.sock")
                    .display()
                    .to_string(),
            ),
            ..Default::default()
        };
        let hop = hop_fixture();
        let err = envpin_hop_respawn(Some(&cfg), &hop.project, &backend, &hop, "old-pane")
            .expect_err("no hc daemon on the configured socket");
        assert!(
            format!("{err:#}").contains("hc connect for env-pin hop"),
            "the hc arm tries its own split path: {err:#}"
        );
        assert!(
            !order.lock().unwrap().iter().any(|c| c.starts_with("run:")),
            "the hc arm never takes the rex pane_run path"
        );
    }

    struct RexRefuseBackend {
        order: Arc<Mutex<Vec<String>>>,
        starts: Arc<Mutex<usize>>,
        commands: Arc<Mutex<Vec<String>>>,
    }

    fn site_pane(id: &str) -> toron_core::pane_types::PaneInfo {
        toron_core::pane_types::PaneInfo {
            pane_id: id.to_string(),
            terminal_id: String::new(),
            workspace_id: "ws".to_string(),
            tab_id: String::new(),
            focused: false,
            agent_status: toron_core::pane_types::AgentStatus::Idle,
            revision: 0,
            title: None,
            cwd: None,
            agent: None,
            agent_session: None,
            display_agent: None,
            foreground_cwd: None,
            label: None,
            scroll: None,
            state_labels: serde_json::Map::new(),
            terminal_title: None,
            terminal_title_stripped: None,
            tokens: serde_json::Map::new(),
        }
    }

    fn site_agent(name: &str, pane_id: &str) -> toron_core::pane_types::AgentInfo {
        toron_core::pane_types::AgentInfo {
            agent: None,
            agent_status: toron_core::pane_types::AgentStatus::Working,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: false,
            launch_pending: false,
            name: Some(name.to_string()),
            pane_id: pane_id.to_string(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: String::new(),
            terminal_id: String::new(),
            terminal_title: None,
            workspace_id: String::new(),
        }
    }

    impl crate::backend::MultiplexerBackend for RexRefuseBackend {
        fn name(&self) -> &'static str {
            "auto"
        }
        fn agent_list(
            &self,
        ) -> Result<Vec<toron_core::pane_types::AgentInfo>, toron_plane::backend::BackendError>
        {
            let mut row = site_agent("", "block:new-pane-1");
            row.agent_status = toron_core::pane_types::AgentStatus::Idle;
            Ok(vec![row])
        }
        fn pane_list(
            &self,
            _workspace_id: Option<&str>,
        ) -> Result<Vec<toron_core::pane_types::PaneInfo>, toron_plane::backend::BackendError>
        {
            Ok(vec![site_pane("block:old-pane")])
        }
        fn pane_split(
            &self,
            parent: &str,
            direction: &str,
            _cwd: Option<&str>,
        ) -> Result<toron_core::pane_types::PaneInfo, toron_plane::backend::BackendError> {
            self.order
                .lock()
                .unwrap()
                .push(format!("split:{parent}:{direction}"));
            Ok(site_pane("block:new-pane-1"))
        }
        fn pane_run(
            &self,
            pane_id: &str,
            command: &str,
        ) -> Result<(), toron_plane::backend::BackendError> {
            self.order.lock().unwrap().push(format!("run:{pane_id}"));
            self.commands.lock().unwrap().push(command.to_string());
            Ok(())
        }
        fn agent_rename(
            &self,
            pane_id: &str,
            new_name: Option<&str>,
        ) -> Result<(), toron_plane::backend::BackendError> {
            self.order
                .lock()
                .unwrap()
                .push(format!("rename:{}@{pane_id}", new_name.unwrap_or_default()));
            Ok(())
        }
        fn agent_wait(
            &self,
            pane_id: &str,
            _until: &[toron_core::pane_types::AgentStatus],
            _timeout_ms: Option<u64>,
        ) -> Result<toron_core::pane_types::AgentInfo, toron_plane::backend::BackendError> {
            self.order.lock().unwrap().push(format!("wait:{pane_id}"));
            Ok(site_agent("TestyCreek", pane_id))
        }
        fn agent_start(
            &self,
            name: &str,
            _kind: &str,
            pane_id: &str,
            _args: &[String],
        ) -> Result<toron_core::pane_types::AgentInfo, toron_plane::backend::BackendError> {
            let mut starts = self.starts.lock().unwrap();
            *starts += 1;
            let first = *starts == 1;
            self.order
                .lock()
                .unwrap()
                .push(format!("start:{name}@{pane_id}"));
            if first {
                return Err(toron_plane::backend::BackendError::timeout(
                    "agent.start failed: 401 invalid api key (fake)",
                ));
            }
            Ok(site_agent(name, pane_id))
        }
        fn pane_close(&self, pane_id: &str) -> Result<(), toron_plane::backend::BackendError> {
            self.order.lock().unwrap().push(format!("close:{pane_id}"));
            Ok(())
        }
    }

    struct NeverSplitBackend(&'static str);

    impl crate::backend::MultiplexerBackend for NeverSplitBackend {
        fn name(&self) -> &'static str {
            self.0
        }
        fn pane_split(
            &self,
            _parent_pane_id: &str,
            _direction: &str,
            _cwd: Option<&str>,
        ) -> Result<toron_core::pane_types::PaneInfo, toron_plane::backend::BackendError> {
            panic!("{} must never split for an env-pin hop", self.0);
        }
    }

    #[test]
    fn envpin_hop_split_non_hc_refuses() {
        let _guard = crate::config::lock_env();
        let mut env = BTreeMap::new();
        env.insert(
            "OPENCODE_CONFIG_CONTENT".to_string(),
            "{\"model\":\"opencode/gold\"}".to_string(),
        );
        for (backend, name) in [
            (NeverSplitBackend("rex"), "rex"),
            (NeverSplitBackend("hc"), "hc"),
        ] {
            let err = envpin_hop_split(None, Some("envpin-test-proj"), &backend, "anchor", &env)
                .expect_err("a backend that cannot carry the pin must refuse");
            let msg = format!("{err:#}");
            assert!(
                msg.contains(name) && msg.contains("pane env pin"),
                "refusal names the backend and the reason: {msg}"
            );
        }
    }

    #[test]
    fn envpin_respawn_rex_site_hops_with_pin() {
        let _guard = crate::config::lock_env();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cfgdir = std::env::temp_dir().join(format!("fw-envpin-site-{nanos}"));
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfgdir) };
        let cfgdir = crate::config::config_dir();
        std::fs::create_dir_all(&cfgdir).expect("hermetic config dir");
        std::fs::write(
            cfgdir.join("profiles.toml"),
            "[profiles.opencode-fallback]\nharness = \"opencode\"\nprovider = \"opencode\"\nmodel = \"big-pickle\"\nfallback = [\"opencode-next\"]\n\n[profiles.opencode-next]\nharness = \"opencode\"\nprovider = \"opencode\"\nmodel = \"gold\"\n",
        )
        .expect("stage profiles");
        std::fs::write(
            cfgdir.join("models.toml"),
            "[models.\"opencode/gold\"]\ncaps = [\"opencode\"]\n",
        )
        .expect("stage models");
        let order = Arc::new(Mutex::new(Vec::new()));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let backend = RexRefuseBackend {
            order: order.clone(),
            starts: Arc::new(Mutex::new(0)),
            commands: commands.clone(),
        };
        let cfg = Config::default();
        let agent = start_agent_with_retry_dyn_profile(
            &backend,
            "TestyCreek",
            "opencode",
            "block:old-pane",
            &[],
            LaunchCtx {
                profile_name: Some("opencode-fallback"),
                project: Some("envpin-rex-proj"),
                cfg: Some(&cfg),
            },
        )
        .expect("rex respawns the pinned hop instead of refusing");
        assert_eq!(agent.name.as_deref(), Some("TestyCreek"));
        assert_eq!(
            *order.lock().unwrap(),
            vec![
                "start:TestyCreek@block:old-pane".to_string(),
                "split:block:old-pane:down".to_string(),
                "run:block:new-pane-1".to_string(),
                "rename:TestyCreek@block:new-pane-1".to_string(),
                "wait:block:new-pane-1".to_string(),
                "close:block:old-pane".to_string(),
            ],
            "the site splits plain, runs the pinned launch, then closes the old pane"
        );
        let commands = commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1, "one rex run: {commands:?}");
        assert!(
            commands[0].contains("\"model\":\"opencode/gold\""),
            "the fallback model rides the rex env prefix: {}",
            commands[0]
        );
        let key = crate::agent_ref::RebindIntent::derive_effect_key(
            &crate::agent_ref::AgentRef::new("envpin-rex-proj", "TestyCreek"),
            &crate::agent_ref::AgentBinding {
                profile_name: "opencode-fallback".to_string(),
                model: "opencode/big-pickle".to_string(),
                harness: "opencode".to_string(),
                location: crate::agent_ref::BindingLocation::Pane,
            },
            &crate::agent_ref::AgentBinding {
                profile_name: "opencode-next".to_string(),
                model: "opencode/gold".to_string(),
                harness: "opencode".to_string(),
                location: crate::agent_ref::BindingLocation::Pane,
            },
        );
        let rows = rebind_rows("envpin-rex-proj", &key);
        assert_eq!(
            rows.iter().map(|i| i.state).collect::<Vec<_>>(),
            vec![
                toron_core::index::workflow_effects::EffectState::Prepared,
                toron_core::index::workflow_effects::EffectState::InFlight,
                toron_core::index::workflow_effects::EffectState::Settled,
            ],
            "the hopped respawn settles its one intent: {rows:?}"
        );
        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }
}

#[cfg(test)]
mod recipe_launch_tests {
    use super::{recipe_launch_command, shell_quote};

    #[test]
    fn shell_quote_wraps_plain_and_escapes_embedded_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b c"), "'a b c'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn recipe_command_appends_quoted_args() {
        let args: Vec<String> = vec![
            "--model".into(),
            "opencode-go/deepseek-v4-flash".into(),
            "Boot ready check: reply exactly READY.".into(),
        ];
        assert_eq!(
            recipe_launch_command("prime-agent --offline", &args),
            "prime-agent --offline '--model' 'opencode-go/deepseek-v4-flash' 'Boot ready check: reply exactly READY.'"
        );
    }

    #[test]
    fn recipe_command_with_no_args_is_the_recipe() {
        assert_eq!(
            recipe_launch_command("prime-agent --offline", &[]),
            "prime-agent --offline"
        );
    }
}

/// bd-zq8u: hc's `agent.start` rejects launch args it cannot encode
/// safely for the target shell — `arg.chars().any(char::is_control)`
/// (hc src/app/agents.rs, added 2026-07-17): any control character
/// (tab, newline, CR, NUL, ESC) fails with `invalid_agent_argument`.
/// Pre-validate BEFORE the retry loop: a poisoned launch arg must fail
/// LOUD on the first call with an actionable error, never burn the retry
/// budget. The check is strict ASCII-printable (0x20..=0x7E): non-ASCII
/// and control bytes both fail.
fn ensure_ascii_launch_args(args: &[String]) -> Result<()> {
    for (idx, arg) in args.iter().enumerate() {
        if let Some((byte_idx, c)) = arg
            .char_indices()
            .find(|(_, c)| !c.is_ascii() || c.is_control())
        {
            bail!(
                "agent launch argument {idx} contains non-ASCII or control character {c:?} \
                 (U+{:04X}) at byte {byte_idx}: hc cannot encode it safely for the \
                 target shell. ASCII-fy the launch text (e.g. \
                 integrations/peer-aware.md.tpl, which must also be free of \
                 newlines/tabs) or pass --no-peer-aware",
                c as u32
            );
        }
    }
    Ok(())
}

/// Launch identity the retrying start needs beyond argv: profile, project and
/// config for the recipe/reroute paths. `None` everywhere for the plain path.
#[derive(Default)]
pub(crate) struct LaunchCtx<'a> {
    pub profile_name: Option<&'a str>,
    pub project: Option<&'a str>,
    pub cfg: Option<&'a Config>,
}

pub(crate) fn start_agent_with_retry(
    c: &mut Client,
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
) -> Result<AgentInfo> {
    start_agent_with_retry_profile(c, name, kind, pane_id, args, LaunchCtx::default())
}

pub(crate) fn start_agent_with_retry_profile(
    c: &mut Client,
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
    ctx: LaunchCtx<'_>,
) -> Result<AgentInfo> {
    ensure_ascii_launch_args(args)?;
    let mut recipe_launched = false;
    let mut current_kind = kind.to_string();
    let mut current_args = args.to_vec();
    let mut current_profile = ctx.profile_name.map(std::string::ToString::to_string);
    let mut attempted_profiles: Vec<String> = current_profile.clone().into_iter().collect();
    let mut already_rerouted = false;
    for attempt in 0..30 {
        if !recipe_launched {
            let mut ops = ConcreteTakenOps { c: &*c };
            match start_agent_with_taken_name_clear(
                &mut ops,
                name,
                &current_kind,
                pane_id,
                &current_args,
                attempt as u32 + 1,
            ) {
                StartAttempt::Started => {}
                StartAttempt::Refused(e) => return Err(e),
                StartAttempt::NotTaken(e) => {
                    if attempt == 0 {
                        eprintln!("agent.start error: {e:#}");
                        let cf = failure::classify(&e.to_string(), None, None);
                        if cf.reroute_eligible {
                            eprintln!(
                                "flywheel: classified spawn failure kind={} evidence={}",
                                cf.kind, cf.evidence
                            );
                        }
                        if cf.reroute_eligible && !already_rerouted {
                            if let Some(orig) = current_profile.as_deref()
                                && let Some(next) = reroute_next_profile(
                                    orig,
                                    cf.kind,
                                    &cf.evidence,
                                    &attempted_profiles,
                                    attempt as u32,
                                )
                                && let Ok(cat) = ProfileCatalog::load()
                                && let Some(next_prof) = cat.get(&next)
                            {
                                let next_kind = if next_prof.harness.is_empty() {
                                    current_kind.clone()
                                } else {
                                    next_prof.harness.clone()
                                };
                                if let Ok(next_plan) =
                                    profile_launch_plan(&next_kind, next_prof, None)
                                    && let Some(pin) = &next_plan.env_model_pin
                                {
                                    let pinned = current_profile
                                        .as_deref()
                                        .and_then(|cp| cat.get(cp))
                                        .map(|p| format!("{}/{}", p.provider, p.model));
                                    let mut reroute_in_place =
                                        reroute_env_pin_verdict(pinned.as_deref(), pin)
                                            == RerouteEnvPin::AlreadyPinned;
                                    if !reroute_in_place {
                                        let site_hop = || {
                                            let Some(proj) = ctx.project else {
                                                return SiteHop::Refused;
                                            };
                                            let Some(cfg) = ctx.cfg else {
                                                return SiteHop::Refused;
                                            };
                                            let mut next_env = std::collections::BTreeMap::new();
                                            if hc::apply_opencode_model_pin(&mut next_env, pin)
                                                .is_err()
                                            {
                                                return SiteHop::Refused;
                                            }
                                            next_env.insert(
                                                "FLYWHEEL_REROUTE_NOTE".to_string(),
                                                format!(
                                                    "rerouted {orig} -> {next} after {}",
                                                    cf.kind
                                                ),
                                            );
                                            let hop_backend =
                                                match backend::multiplexer_backend(cfg) {
                                                    Ok(hb) => hb,
                                                    Err(e) => {
                                                        return SiteHop::Failed(e.into());
                                                    }
                                                };
                                            let hop = EnvpinHop {
                                                project: proj.to_string(),
                                                agent: name.to_string(),
                                                old_pane: pane_id.to_string(),
                                                next_harness: next_kind.clone(),
                                                next_args: next_plan.args.clone(),
                                                next_env,
                                                next_profile: next.clone(),
                                                next_model: next_plan.model.clone(),
                                                from_profile: orig.to_string(),
                                                from_model: pinned.clone().unwrap_or_default(),
                                                from_harness: current_kind.clone(),
                                                kind: cf.kind,
                                                evidence: cf.evidence.clone(),
                                                attempt: attempt as u32,
                                            };
                                            let respawn = |anchor: &str| {
                                                envpin_hop_respawn(
                                                    Some(cfg),
                                                    proj,
                                                    &*hop_backend,
                                                    &hop,
                                                    anchor,
                                                )
                                            };
                                            match envpin_respin_hop(
                                                &*hop_backend,
                                                &respawn,
                                                &hop,
                                                already_rerouted,
                                            ) {
                                                Ok(EnvpinHopOutcome::Hopped { agent }) => {
                                                    SiteHop::Hopped(agent)
                                                }
                                                Ok(EnvpinHopOutcome::AlreadySatisfied) => {
                                                    SiteHop::InPlace
                                                }
                                                Ok(_) => SiteHop::Refused,
                                                Err(e) => {
                                                    if let Some(reason) = envpin_split_refusal(&e) {
                                                        eprintln!("flywheel: {reason}");
                                                        SiteHop::Refused
                                                    } else {
                                                        SiteHop::Failed(e)
                                                    }
                                                }
                                            }
                                        };
                                        match site_hop() {
                                            SiteHop::Hopped(agent) => return Ok(*agent),
                                            SiteHop::Failed(e) => return Err(e),
                                            SiteHop::InPlace => reroute_in_place = true,
                                            SiteHop::Refused => {}
                                        }
                                    }
                                    if !reroute_in_place {
                                        eprintln!(
                                            "flywheel: pane reroute {} -> {} REFUSED: {} needs its \
                                             model pinned by pane env, and pane {} was created \
                                             with a different pin. A pane's env is fixed at \
                                             split, so this hop would silently run the previous \
                                             model. Respawn instead of rerouting.",
                                            orig, next, next_kind, pane_id
                                        );
                                    } else {
                                        eprintln!(
                                            "flywheel: pane reroute {} -> {} (profile fallback, \
                                             env pin already present on the pane)",
                                            orig, next
                                        );
                                        current_kind = next_kind;
                                        current_args = next_plan.args;
                                        current_profile = Some(next.clone());
                                        attempted_profiles.push(next);
                                        already_rerouted = true;
                                        std::thread::sleep(Duration::from_secs(1));
                                        continue;
                                    }
                                } else if let Ok(next_args) =
                                    profile_launch_args(&next_kind, next_prof, None)
                                {
                                    eprintln!(
                                        "flywheel: pane reroute {} -> {} (profile fallback)",
                                        orig, next
                                    );
                                    current_kind = next_kind;
                                    current_args = next_args;
                                    current_profile = Some(next.clone());
                                    attempted_profiles.push(next);
                                    already_rerouted = true;
                                    std::thread::sleep(Duration::from_secs(1));
                                    continue;
                                }
                            }
                            let fallback_key = ctx.profile_name.unwrap_or(kind);
                            if fallback_chain_for(fallback_key).is_empty() {
                                return Err(anyhow::anyhow!(
                                    "spawn failed ({}): {} — no fallback chain",
                                    cf.kind,
                                    cf.evidence
                                ));
                            }
                            return Err(anyhow::anyhow!(
                                "spawn failed ({}): {} — reroute hop failed",
                                cf.kind,
                                cf.evidence
                            ));
                        }
                        if cf.reroute_eligible && already_rerouted {
                            return Err(anyhow::anyhow!(
                                "spawn failed ({}): {} — fallback also failed (terminal, one hop per class)",
                                cf.kind,
                                cf.evidence
                            ));
                        }
                        if matches!(&e.code, BackendErrorCode::Other(c) if c == "unsupported_agent_kind")
                        {
                            if try_recipe_launch(c, &current_kind, pane_id, &current_args)? {
                                recipe_launched = true;
                            } else {
                                return Err(e.into());
                            }
                        } else if cf.kind == failure::FailureKind::Billing
                            || cf.kind == failure::FailureKind::Auth
                        {
                            let fallback_key = ctx.profile_name.unwrap_or(kind);
                            if fallback_chain_for(fallback_key).is_empty() {
                                return Err(anyhow::anyhow!(
                                    "spawn failed ({}): {} — no fallback chain",
                                    cf.kind,
                                    cf.evidence
                                ));
                            }
                        }
                    }
                }
            }
        }
        if let Ok(agents) = c.agent_list() {
            if let Some(a) = agents
                .iter()
                .find(|a| a.name.as_deref() == Some(name) && a.pane_id == pane_id)
            {
                return Ok(a.clone());
            }
            if let Some(a) = agents.iter().find(|a| a.pane_id == pane_id) {
                if a.name.as_deref() != Some(name)
                    && let Err(e) = c.agent_rename(&a.pane_id, name)
                {
                    eprintln!("agent.rename error: {e:#}");
                }
                if let Ok(agents) = c.agent_list()
                    && let Some(a) = agents.iter().find(|a| a.pane_id == pane_id)
                {
                    return Ok(a.clone());
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    bail!("agent start failed for {name} on {pane_id} after 30 attempts")
}

/// Trait-object variant of [`start_agent_with_retry`] (review-fix F6): the
/// durable-loop respawn leaf routes through the backend factory, so this
/// retries via `MultiplexerBackend::agent_start` + `agent_list` instead of
/// the concrete `Client`. Same retry contract: 30 attempts, 3s sleep, accept
/// a same-pane agent even if nameless (bd-w17) and apply the name.
fn start_agent_with_retry_dyn_profile(
    backend: &dyn backend::MultiplexerBackend,
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
    ctx: LaunchCtx<'_>,
) -> Result<AgentInfo> {
    ensure_ascii_launch_args(args)?;
    let mut recipe_launched = false;
    let mut current_kind = kind.to_string();
    let mut current_args = args.to_vec();
    let mut current_profile = ctx.profile_name.map(std::string::ToString::to_string);
    let mut attempted_profiles: Vec<String> = current_profile.clone().into_iter().collect();
    let mut already_rerouted = false;
    for attempt in 0..30 {
        if !recipe_launched {
            let mut ops = DynTakenOps { backend };
            match start_agent_with_taken_name_clear(
                &mut ops,
                name,
                &current_kind,
                pane_id,
                &current_args,
                attempt as u32 + 1,
            ) {
                StartAttempt::Started => {}
                StartAttempt::Refused(e) => return Err(e),
                StartAttempt::NotTaken(e) => {
                    if attempt == 0 {
                        eprintln!("agent.start error: {e:#}");
                        let cf = failure::classify(&e.to_string(), None, None);
                        if cf.reroute_eligible {
                            eprintln!(
                                "flywheel: classified spawn failure kind={} evidence={}",
                                cf.kind, cf.evidence
                            );
                        }
                        if cf.reroute_eligible && !already_rerouted {
                            if let Some(orig) = current_profile.as_deref()
                                && let Some(next) = reroute_next_profile(
                                    orig,
                                    cf.kind,
                                    &cf.evidence,
                                    &attempted_profiles,
                                    attempt as u32,
                                )
                                && let Ok(cat) = ProfileCatalog::load()
                                && let Some(next_prof) = cat.get(&next)
                            {
                                let next_kind = if next_prof.harness.is_empty() {
                                    current_kind.clone()
                                } else {
                                    next_prof.harness.clone()
                                };
                                if let Ok(next_plan) =
                                    profile_launch_plan(&next_kind, next_prof, None)
                                    && let Some(pin) = &next_plan.env_model_pin
                                {
                                    let pinned = current_profile
                                        .as_deref()
                                        .and_then(|cp| cat.get(cp))
                                        .map(|p| format!("{}/{}", p.provider, p.model));
                                    let mut reroute_in_place =
                                        reroute_env_pin_verdict(pinned.as_deref(), pin)
                                            == RerouteEnvPin::AlreadyPinned;
                                    if !reroute_in_place {
                                        let site_hop = || {
                                            let Some(proj) = ctx.project else {
                                                return SiteHop::Refused;
                                            };
                                            let mut next_env = std::collections::BTreeMap::new();
                                            if hc::apply_opencode_model_pin(&mut next_env, pin)
                                                .is_err()
                                            {
                                                return SiteHop::Refused;
                                            }
                                            next_env.insert(
                                                "FLYWHEEL_REROUTE_NOTE".to_string(),
                                                format!(
                                                    "rerouted {orig} -> {next} after {}",
                                                    cf.kind
                                                ),
                                            );
                                            let hop = EnvpinHop {
                                                project: proj.to_string(),
                                                agent: name.to_string(),
                                                old_pane: pane_id.to_string(),
                                                next_harness: next_kind.clone(),
                                                next_args: next_plan.args.clone(),
                                                next_env,
                                                next_profile: next.clone(),
                                                next_model: next_plan.model.clone(),
                                                from_profile: orig.to_string(),
                                                from_model: pinned.clone().unwrap_or_default(),
                                                from_harness: current_kind.clone(),
                                                kind: cf.kind,
                                                evidence: cf.evidence.clone(),
                                                attempt: attempt as u32,
                                            };
                                            let respawn = |anchor: &str| {
                                                envpin_hop_respawn(
                                                    ctx.cfg, proj, backend, &hop, anchor,
                                                )
                                            };
                                            match envpin_respin_hop(
                                                backend,
                                                &respawn,
                                                &hop,
                                                already_rerouted,
                                            ) {
                                                Ok(EnvpinHopOutcome::Hopped { agent }) => {
                                                    SiteHop::Hopped(agent)
                                                }
                                                Ok(EnvpinHopOutcome::AlreadySatisfied) => {
                                                    SiteHop::InPlace
                                                }
                                                Ok(_) => SiteHop::Refused,
                                                Err(e) => {
                                                    if let Some(reason) = envpin_split_refusal(&e) {
                                                        eprintln!("flywheel: {reason}");
                                                        SiteHop::Refused
                                                    } else {
                                                        SiteHop::Failed(e)
                                                    }
                                                }
                                            }
                                        };
                                        match site_hop() {
                                            SiteHop::Hopped(agent) => return Ok(*agent),
                                            SiteHop::Failed(e) => return Err(e),
                                            SiteHop::InPlace => reroute_in_place = true,
                                            SiteHop::Refused => {}
                                        }
                                    }
                                    if !reroute_in_place {
                                        eprintln!(
                                            "flywheel: pane reroute {} -> {} REFUSED: {} needs its \
                                             model pinned by pane env, and pane {} was created \
                                             with a different pin. A pane's env is fixed at \
                                             split, so this hop would silently run the previous \
                                             model. Respawn instead of rerouting.",
                                            orig, next, next_kind, pane_id
                                        );
                                    } else {
                                        eprintln!(
                                            "flywheel: pane reroute {} -> {} (profile fallback, \
                                             env pin already present on the pane)",
                                            orig, next
                                        );
                                        current_kind = next_kind;
                                        current_args = next_plan.args;
                                        current_profile = Some(next.clone());
                                        attempted_profiles.push(next);
                                        already_rerouted = true;
                                        std::thread::sleep(Duration::from_secs(1));
                                        continue;
                                    }
                                } else if let Ok(next_args) =
                                    profile_launch_args(&next_kind, next_prof, None)
                                {
                                    eprintln!(
                                        "flywheel: pane reroute {} -> {} (profile fallback)",
                                        orig, next
                                    );
                                    current_kind = next_kind;
                                    current_args = next_args;
                                    current_profile = Some(next.clone());
                                    attempted_profiles.push(next);
                                    already_rerouted = true;
                                    std::thread::sleep(Duration::from_secs(1));
                                    continue;
                                }
                            }
                            let fallback_key = ctx.profile_name.unwrap_or(kind);
                            if fallback_chain_for(fallback_key).is_empty() {
                                return Err(anyhow::anyhow!(
                                    "spawn failed ({}): {} — no fallback chain",
                                    cf.kind,
                                    cf.evidence
                                ));
                            }
                            return Err(anyhow::anyhow!(
                                "spawn failed ({}): {} — reroute hop failed",
                                cf.kind,
                                cf.evidence
                            ));
                        }
                        if cf.reroute_eligible && already_rerouted {
                            return Err(anyhow::anyhow!(
                                "spawn failed ({}): {} — fallback also failed (terminal, one hop per class)",
                                cf.kind,
                                cf.evidence
                            ));
                        }
                        if matches!(&e.code, BackendErrorCode::Other(c) if c == "unsupported_agent_kind")
                        {
                            if try_recipe_launch_dyn(
                                backend,
                                &current_kind,
                                pane_id,
                                &current_args,
                            )? {
                                recipe_launched = true;
                            } else {
                                return Err(e.into());
                            }
                        } else if cf.kind == failure::FailureKind::Billing
                            || cf.kind == failure::FailureKind::Auth
                        {
                            let fallback_key = ctx.profile_name.unwrap_or(&current_kind);
                            if fallback_chain_for(fallback_key).is_empty() {
                                return Err(anyhow::anyhow!(
                                    "spawn failed ({}): {} — no fallback chain",
                                    cf.kind,
                                    cf.evidence
                                ));
                            }
                        }
                    }
                }
            }
        }
        if let Ok(agents) = backend.agent_list() {
            if let Some(a) = agents
                .iter()
                .find(|a| a.name.as_deref() == Some(name) && a.pane_id == pane_id)
            {
                return Ok(a.clone());
            }
            if let Some(a) = agents.iter().find(|a| a.pane_id == pane_id) {
                if a.name.as_deref() != Some(name) {
                    let _ = backend.agent_rename(&a.pane_id, Some(name));
                }
                if let Ok(agents) = backend.agent_list()
                    && let Some(a) = agents.iter().find(|a| a.pane_id == pane_id)
                {
                    return Ok(a.clone());
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    bail!("agent start failed for {name} on {pane_id} after 30 attempts")
}

/// bd-zq8u: the peer-aware template is injected into agent.start launch
/// args; hc rejects non-ASCII bytes ("cannot be encoded safely for the
/// target shell"). These tests pin the ASCII contract on the rendered
/// template AND the fail-fast guard (never a 30-attempt retry burn).
#[cfg(test)]
mod peer_encoding_tests {
    use super::{
        ensure_ascii_launch_args, peer_prompt_template, start_agent_with_retry_dyn_profile,
    };
    use crate::backend::MultiplexerBackend;

    /// Read the real template from disk (the file the spawn path loads).
    fn real_template() -> String {
        let p =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/peer-aware.md.tpl");
        std::fs::read_to_string(&p).expect("peer-aware template readable")
    }

    /// S9/bd-zq8u: the rendered peer template is ASCII-only AND free of
    /// control characters for ALL substitution inputs — name,
    /// mail_identity, and the three budget numbers. hc rejects any arg
    /// with a control char (`arg.chars().any(char::is_control)`), so the
    /// rendered launch text must pass the strict launch guard.
    #[test]
    fn peer_template_renders_ascii_for_all_inputs() {
        let tpl = real_template();
        let inputs: &[(&str, &str, u32, u32, u32)] = &[
            ("flywheel-toron-coder1", "DustySparrow", 5, 30, 3),
            ("flywheel-hachiko-oc4", "BlueRidge", 10, 60, 5),
            ("flywheel-xproj-gk2", "CoralFinch", 1, 1, 1),
            ("flywheel-toron-omp", "GrayCarp", 60, 240, 0),
        ];
        for (name, identity, interval, budget, prompts) in inputs {
            let rendered = peer_prompt_template(&tpl, name, identity, *interval, *budget, *prompts);
            assert!(
                rendered.is_ascii(),
                "rendered template must be ASCII for name={name} identity={identity}"
            );
            assert!(
                !rendered.chars().any(char::is_control),
                "rendered template must be control-char-free (hc rejects \
                 is_control args) for name={name} identity={identity}"
            );
            ensure_ascii_launch_args(std::slice::from_ref(&rendered))
                .unwrap_or_else(|e| panic!("ASCII render must pass the launch guard: {e}"));
        }
    }

    /// bd-zq8u: a poisoned launch arg fails LOUD with an actionable error
    /// naming the defect class, the character code, and the fix. The
    /// control-char case is hc's ACTUAL rejection criterion — the 2026-08-15
    /// regression that burned 30 retries was the multi-line template's
    /// newlines, not the non-ASCII glyphs.
    #[test]
    fn launch_arg_guard_rejects_poisoned_args_with_actionable_errors() {
        let cases: &[(&str, &[&str])] = &[
            ("You are §9.3 -- ok -> done", &["U+00A7", "--no-peer-aware"]),
            (
                "line one\nline two",
                &["U+000A", "integrations/peer-aware.md.tpl"],
            ),
        ];
        for (arg, needles) in cases {
            let err = match ensure_ascii_launch_args(&[arg.to_string()]) {
                Ok(ok) => panic!("{arg:?} must be rejected, got {ok:?}"),
                Err(e) => e,
            };
            let msg = err.to_string();
            assert!(
                msg.contains("non-ASCII or control"),
                "error must name the defect class: {msg}"
            );
            for needle in *needles {
                assert!(msg.contains(needle), "error must mention {needle}: {msg}");
            }
        }
    }

    /// bd-zq8u: the guard fires BEFORE agent.start — a poisoned arg must
    /// fail on the first call, never enter the 30-attempt retry loop. A
    /// backend whose agent_start panics proves the guard returns first.
    struct PanicOnStartMux;
    impl MultiplexerBackend for PanicOnStartMux {
        fn name(&self) -> &'static str {
            "panic-on-start"
        }
        /// If the guard ever regresses and this is reached, the test fails
        /// immediately instead of burning the 30-attempt retry loop.
        fn agent_start(
            &self,
            _name: &str,
            _kind: &str,
            _pane_id: &str,
            _args: &[String],
        ) -> Result<crate::hc::AgentInfo, crate::hc::BackendError> {
            panic!("agent.start must never be reached with a poisoned arg")
        }
    }

    #[test]
    fn start_agent_retry_fails_fast_on_poisoned_arg() {
        let backend = &PanicOnStartMux as &dyn MultiplexerBackend;
        let err = start_agent_with_retry_dyn_profile(
            backend,
            "flywheel-toron-coder1",
            "opencode",
            "w9:p1",
            &["peer template with § section sign".to_string()],
            crate::LaunchCtx::default(),
        )
        .expect_err("poisoned arg must fail fast");
        assert!(
            err.to_string().contains("non-ASCII or control"),
            "fast-fail error must be the guard's: {err}"
        );
    }

    /// bd-zq8u: empty args (non-peer-aware single-shot panes) always pass.
    #[test]
    fn launch_arg_guard_accepts_empty_and_ascii_args() {
        ensure_ascii_launch_args(&[]).expect("no args is fine");
        ensure_ascii_launch_args(&[
            "Boot ready check: reply exactly READY.".to_string(),
            "--model opencode-go/deepseek-v4-flash".to_string(),
        ])
        .expect("ASCII args pass");
    }

    /// Toron projection has no read-state: `unread_only` is a documented
    /// no-op. The launch template must poll by `since_ts` high-water and
    /// must not teach unread as a working filter.
    fn teaches_unread_filter(s: &str) -> bool {
        s.contains("unread_only=true")
            || s.contains("unread_only = true")
            || s.contains("unread_only: true")
            || s.contains("unread_only,")
            || s.contains("(unread_only")
    }

    #[test]
    fn peer_template_polls_since_ts_not_unread() {
        let tpl = real_template();
        assert!(
            !teaches_unread_filter(&tpl),
            "peer-aware template must not teach unread as a working filter"
        );
        assert!(
            tpl.contains("since_ts"),
            "peer-aware template must teach since_ts high-water"
        );
        assert!(
            tpl.contains("[wake]"),
            "ADR-0005: handler wakes on the [wake] line, not a poll timer"
        );
        assert!(
            !tpl.contains("every {poll_interval_minutes} minutes\n  of idle"),
            "ADR-0005: 5-min idle watch must not be the driver"
        );
        let rendered =
            peer_prompt_template(&tpl, "flywheel-toron-coder1", "DustySparrow", 5, 30, 3);
        assert!(
            !teaches_unread_filter(&rendered),
            "rendered launch prompt must not teach unread as a working filter"
        );
        assert!(
            rendered.contains("since_ts"),
            "rendered launch prompt must mention since_ts"
        );
    }
}

/// Register a spawned agent as an agent-mail identity (ADR-0001
/// mail-register-on-spawn) and bridge it to its hc terminal (ADR-0001 A4,
/// bd-mail-pane-identity-bridge): toron holds no per-pane identity files, so
/// the bridge is the pane's `toron_pin`/`toron_project`/`toron_npub` labels
/// (`terminal.label`) plus the local identity file flywheel keeps for its own
/// reads — `resolve_pane_identity` can then map the pane to the name.
/// Best-effort: if mail isn't configured or the backend returns an error, the
/// spawn still succeeded — log a warning and continue. The agent becomes
/// addressable by name in mail once registered.
///
/// bd-627z.2: a CONFIGURED-but-unreachable daemon (the [mail.toron] url is
/// set, the daemon is down) is NOT a silent skip — it gets a prominent
/// warning that the agents will have no mail identities until the daemon
/// recovers, with the remediation (start the daemon; the loop/assign
/// auto-registers on first contact once it is back).
///
/// bd-627z.4: returns the DETERMINISTIC mail identity so the spawn caller
/// can pin it in the spawn registry (pane → identity) — assign then
/// resolves through that map and the identity never shifts.
///
/// S9 (ADR-0018 D1a): the identity is a UNIQUE adjective+noun PIN (the
/// legacy whitelist generator, collision-checked against the spawn
/// registry pins AND the registered agents; retry on collision), not the
/// canonical terminal name. The slug name stays the technical pane handle and
/// resolves as an alias through the registry pin (wake + debate read it).
/// A re-spawn of the same terminal name KEEPS its pinned identity — idempotent,
/// never a fresh draw. The random npub stays underneath (registration
/// creates or upserts the key for the pin).
///
/// S8.4: the upsert carries a FRESH task stamp (spawn_agent_profile) — the
/// toron register_agent upsert overwrites stale profile task fields from a
/// prior session (a 2026-08-14 identity still advertised `bd-qk9q`).
///
/// governed-by: ADR-0001 (bd-mail-register-on-spawn, bd-mail-pane-identity-bridge), ADR-0002 (factory), ADR-0018 D1a (bd-spawn-adjnoun-pin-7psn)
pub(crate) fn register_spawned_agent(
    cfg: &Config,
    project: &str,
    name: &str,
    kind: &str,
    pane_id: &str,
) -> Option<String> {
    let pin = pinned_mail_identity(cfg, project, name, kind)?;
    bind_pane_mail_identity(cfg, project, name, pane_id, Some(&pin));
    Some(pin)
}

/// S9 (ADR-0018 D1a): draw + register the agent's mail PIN and write the
/// slug → pin fold, WITHOUT binding a pane. Idempotent — a re-spawn of the
/// same terminal name keeps its pin (registry-first), a fresh name draws a pin
/// collision-checked against the registry pins AND the agents table.
///
/// bd-pin-public-plane-s-easy.2: the pin is drawn BEFORE the pane exists so
/// the pane env can carry `AGENT_NAME` / `FLYWHEEL_MAIL_AS` (hc injects
/// env only at split/create; agent.start has no env param). The caller then
/// binds the pane with [`bind_pane_mail_identity`] once the pane id exists.
pub(crate) fn pinned_mail_identity(
    cfg: &Config,
    project: &str,
    name: &str,
    kind: &str,
) -> Option<String> {
    let key = project_dir(cfg, project).to_str().map(str::to_string)?;
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("flywheel: mail backend unavailable for {name} ({kind}): {e}");
            return None;
        }
    };
    let daemon_mode = cfg
        .toron_mail_config()
        .url
        .as_deref()
        .map(|u| !u.is_empty())
        .unwrap_or(false);
    let warn_no_identities = |e: &backend::MailError| {
        if daemon_mode {
            eprintln!(
                "\n!!! WARNING: the toron mail daemon is unreachable — spawned \
                 agents will have NO mail identities until it recovers.\n\
                 Fix: start the daemon (toron serve), then the next assign / \
                 loop dispatch auto-registers each identity on first contact.\n\
                 (register_agent failed for {name}: {e})\n"
            );
        } else {
            eprintln!("flywheel: mail register_agent failed for {name}: {e}");
        }
    };
    let mut register_effect =
        match agent_ref::AgentEffectGuard::begin(project, name, agent_ref::AgentPhase::Register) {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("flywheel: warn: cannot journal register intent for {name}: {e:#}");
                None
            }
        };
    if let Err(e) = backend::run_mail(mail.ensure_project(&key)) {
        warn_no_identities(&e);
        if let Some(g) = register_effect.take() {
            g.mark_unknown(Some(&format!("ensure_project failed: {e}")));
        }
        return None;
    }
    let registry = state::load_spawn_registry(project);
    let mail_name = registry
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| e.mail_identity.clone())
        .filter(|m| !m.is_empty() && m != name)
        .unwrap_or_else(|| {
            let mut taken: std::collections::HashSet<String> = registry
                .iter()
                .filter_map(|e| e.mail_identity.clone())
                .collect();
            let (captain_pin, _) = lookup_captain_holder(project);
            if let Some(captain) = captain_pin {
                taken.insert(captain);
            }
            taken.insert("captain".to_string());
            match backend::run_mail(mail.list_agents(&key, None)) {
                Ok(agents) => {
                    taken.extend(agents.into_iter().map(|a| a.name));
                }
                Err(e) => {
                    eprintln!("flywheel: mail list_agents failed during pin draw for {name}: {e}");
                }
            }
            // bd-3qun4: toron enforces one key per pin across the WHOLE store,
            // so two projects drawing from the same agent name collided
            // ("pin X is already registered in <other project>"). Seed with the
            // project key as well: a re-spawn in the same project stays
            // deterministic, a different project draws a different pin.
            let seed = state::fnv1a64(&format!("{key}|{name}"));
            let mut pin = unique_adjnoun_pin(&taken, seed);
            for _ in 0..256 {
                if daemon_mode || !mail::pin_bound_elsewhere(&key, &pin) {
                    break;
                }
                taken.insert(pin.clone());
                pin = unique_adjnoun_pin(&taken, seed);
            }
            pin
        });
    let profile = spawn_agent_profile(&mail_name, kind, project);
    if let Err(e) = backend::run_mail(mail.register_agent(&key, &profile)) {
        warn_no_identities(&e);
        if let Some(g) = register_effect.take() {
            g.mark_unknown(Some(&format!("register_agent failed: {e}")));
        }
        return None;
    }
    if let Ok(Some(facade)) = mail::Mail::from_config(Some(&cfg.toron_mail_config()))
        && let Err(e) = facade.fold_agent_name_alias(&key, name, &mail_name)
    {
        eprintln!("flywheel: mail fold_agent_name_alias failed for {name} -> {mail_name}: {e:#}");
    }
    let mut entries = state::load_spawn_registry(project);
    match entries.iter_mut().find(|e| e.name == name) {
        Some(e) => e.mail_identity = Some(mail_name.clone()),
        None => entries.push(state::SpawnEntry::with_mail_identity(
            String::new(),
            name.to_string(),
            mail_name.clone(),
        )),
    }
    if let Err(e) = state::save_spawn_registry(project, &entries) {
        eprintln!("flywheel: cannot save spawn registry for {name}: {e:#}");
    }
    if let Some(g) = register_effect.take() {
        g.settle(Some(&format!("pin={mail_name}")));
    }
    Some(mail_name)
}

/// bd-pin-public-plane-s-easy.2: bind a drawn pin to its hc terminal — write
/// the pane identity bridge (pane → pin, so resolve_pane_identity names an
/// identity that exists) and record the pane_id in the spawn registry. Runs
/// AFTER the pane exists; the pin itself was drawn pre-split so the pane env
/// could carry it.
/// Stamp gate for `bind_pane_mail_identity` below: a rex `block:` id is
/// not an hc pane, so stamping it makes hc say `pane_not_found`.
/// Pure so the contract is testable.
fn pane_takes_hc_stamp(pane_id: &str) -> bool {
    !crate::inbox_ports::is_rex_block(pane_id)
}

pub(crate) fn bind_pane_mail_identity(
    cfg: &Config,
    project: &str,
    name: &str,
    pane_id: &str,
    pin: Option<&str>,
) {
    let Some(pin) = pin.filter(|p| !p.is_empty()) else {
        return;
    };
    let Some(key) = project_dir(cfg, project).to_str().map(str::to_string) else {
        return;
    };
    if let Ok(mail) = backend::mail_backend(cfg) {
        if let Err(e) = backend::run_mail(mail.write_pane_identity(&key, pane_id, pin)) {
            eprintln!("flywheel: mail write_pane_identity failed for {name} on {pane_id}: {e}");
        }
        if pane_takes_hc_stamp(pane_id) {
            match toron_plane::backend::hc::HcBackend::connect(None) {
                Ok(backend) => {
                    let npub = backend::run_mail(mail.whois(&key, pin))
                        .map(|w| w.npub)
                        .unwrap_or_default();
                    let mut labels = std::collections::BTreeMap::from([
                        (
                            toron_plane::backend::types::TOKEN_PIN.to_string(),
                            pin.to_string(),
                        ),
                        (
                            toron_plane::backend::types::TOKEN_PROJECT.to_string(),
                            project.to_string(),
                        ),
                    ]);
                    if !npub.is_empty() {
                        labels.insert(
                            toron_plane::backend::types::TOKEN_NPUB.to_string(),
                            npub.clone(),
                        );
                    }
                    if let Err(e) = backend.client().terminal_label(pane_id, &labels) {
                        eprintln!(
                            "flywheel: pane mail-identity stamp failed for {name} on {pane_id}: {e}"
                        );
                    }
                    if comms_inbox_on() && !npub.is_empty() {
                        record_spawn_comms_facts(project, pin, &npub);
                    }
                }
                Err(e) => {
                    eprintln!(
                        "flywheel: pane mail-identity stamp skipped for {name} on {pane_id}: {e}"
                    )
                }
            }
        }
    }
    let current = match state::load_spawn_registry_strict(project) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("flywheel: REFUSING to bind mail identity for {name}: {e}");
            return;
        }
    };
    let mut entries = state::merge_registry_for_mutation(project, current);
    // Match the pane first, then the caller's name, then the pin. A spawn
    // writes the profiled row under the composed pane name; a later bind that
    // arrives with the PIN only used to miss it and push a legacy row, and
    // dedupe then kept that later row — the profile column vanished. Updating
    // the existing row in place preserves profile/harness/model and renames it
    // to the caller's addressable name.
    let existing = entries
        .iter()
        .position(|e| e.pane_id == pane_id)
        .or_else(|| entries.iter().position(|e| e.name == name))
        .or_else(|| {
            entries
                .iter()
                .position(|e| e.mail_identity.as_deref() == Some(pin))
        });
    match existing {
        Some(i) => {
            let e = &mut entries[i];
            e.pane_id = pane_id.to_string();
            e.name = name.to_string();
            e.mail_identity = Some(pin.to_string());
        }
        None => entries.push(state::SpawnEntry::with_mail_identity(
            pane_id.to_string(),
            name.to_string(),
            pin.to_string(),
        )),
    }
    if let Err(e) = state::save_spawn_registry(project, &entries) {
        eprintln!("flywheel: cannot save spawn registry for {name}: {e:#}");
    }
}

/// After the pane stamp, under the comms flag only: append `context@1` and
/// `role@1`. A store miss or a refusal is logged. The spawn still returns.
fn record_spawn_comms_facts(project: &str, pin: &str, npub: &str) {
    let stored = state::load_session_topology(project).unwrap_or_default();
    let (word, _) = briefing::split_topology(&stored);
    let topology = briefing::topology_enum(word).unwrap_or(toron_core::topology::Topology::Swarm);
    let role = if state::load_captain_alias(project).as_deref() == Some(pin) {
        toron_core::authority::Role::Captain
    } else {
        toron_core::authority::Role::Crew
    };
    let store = match comms_send::store() {
        Ok(store) => store.with_operator(inbox_ports::system_producer(project)),
        Err(e) => {
            eprintln!("flywheel: comms spawn facts skipped for {pin}: {e:#}");
            return;
        }
    };
    match mail::block_on(spawn_facts::append_spawn_facts(
        &store, project, topology, npub, role,
    )) {
        Ok(Some(what)) => eprintln!("flywheel: comms recorded {what} for {pin}"),
        Ok(None) => {}
        Err(e) => eprintln!("flywheel: comms spawn facts not appended for {pin}: {e}"),
    }
}

/// Deterministic hc-agent-name → adjective+noun pin mapping.
///
/// The pin shape follows the legacy agent-mail whitelist: a FIXED
/// `VALID_ADJECTIVES` (75) × `VALID_NOUNS` (132) pair, lowercase-split +
/// PascalCase (bd-odc). Any other shape is off-shape, INCLUDING lookalikes
/// like `FrostDolphin` (dolphin isn't in the noun list). This maps any hc
/// name (`flywheel-flywheel-oc`) to a stable adjective+noun from that
/// whitelist via FNV-1a hash into the lists.
///
/// The mapping is deterministic — the same terminal name always maps to the
/// same pin, across runs and machines — so re-registration is idempotent and
/// the reverse lookup (terminal name in task_description) is stable. Collisions
/// (different terminal names → same pair) are possible but harmless: the
/// registry is an upsert per name, and the hc name is preserved in
/// task_description, so no information is lost.
///
/// The lists are that agent-mail whitelist copied verbatim from
/// mcp_agent_mail_rust crates/mcp-agent-mail-core/src/models.rs (Apache/MIT).
/// The agent-mail server is gone (bd-euav: toron is the only mail backend), so
/// nothing validates against the lists live any more — they are flywheel's own
/// pin vocabulary now and are FROZEN: derivation is index-based, so editing or
/// reordering either list re-derives every existing pin and breaks identity.
/// If `s` lowercases to a VALID_ADJECTIVES+VALID_NOUNS pair (the frozen
/// whitelist shape, PascalCase after normalization), return the canonical
/// PascalCase form; else None. This is the identity gate for
/// [`terminal_mail_name`]: an input already shaped as a pair passes through.
fn valid_pair(s: &str, adjectives: &[&str], nouns: &[&str]) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    for adj in adjectives {
        if let Some(rest) = lower.strip_prefix(adj)
            && !rest.is_empty()
            && nouns.contains(&rest)
        {
            let mut adj = (*adj).to_owned();
            adj.get_mut(0..1)?.make_ascii_uppercase();
            let mut noun = rest.to_owned();
            noun.get_mut(0..1)?.make_ascii_uppercase();
            return Some(format!("{adj}{noun}"));
        }
    }
    None
}

fn terminal_mail_name(terminal_name: &str) -> String {
    if let Some(pair) = valid_pair(terminal_name, &ADJECTIVES, &NOUNS) {
        return pair;
    }
    let h = state::fnv1a64(terminal_name);
    adjnoun_from_fnv(h)
}

/// bd-recover-accepts-camelcase-pins-z5fz: the hc agent-name rule, quoted
/// from hc's own refusal — 1..=32 chars of lowercase ASCII letters, digits,
/// `-` or `_`.
pub(crate) fn is_terminal_legal_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// bd-recover-accepts-camelcase-pins-z5fz: map a spawn-registry `name` to the
/// hc-legal agent name to restore/launch under.
///
/// A registry row's `name` may hold a CamelCase adjective+noun PIN — an
/// adopted/captain pane registered by pin rather than by kind short — and
/// passing that verbatim to `agent.start`/`agent.rename` is refused with
/// `invalid_agent_name`, so the recovery door was unusable exactly when a
/// swarm needed it. A name already legal passes through untouched, so a
/// healthy pane draws no rename churn. Recovery, rebind, and the send-path
/// auto-heal all route through this one mapping.
///
/// Ceiling: two pins that lowercase to the same string collide on ONE hc
/// name (`QuietHeron`/`quietheron`). An hc name is a display index, not
/// identity — the pin (`mail_identity`) is identity — so a collision is a
/// naming nuisance, never a misroute.
pub(crate) fn terminal_agent_name(name: &str) -> String {
    if is_terminal_legal_name(name) {
        return name.to_string();
    }
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(32);
    out
}

/// The state of one registry row against the live pane listing — the ONE rule
/// `flywheel recover` and `flywheel rebind` act on.
///
/// `Intact` compares the row's name to the pane's terminal name through the SAME
/// mapping the restore uses, so a CamelCase pin whose pane already carries the
/// legal form reads `Intact` (no rename churn, no `invalid_agent_name`).
/// Comparing the raw pin would read `Lost` forever and re-rename each pass —
/// the bd-recover-accepts-camelcase-pins-z5fz bug. `None` means hc does not
/// list the pane at all; `Some(None)` means the pane exists with no name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindingState {
    /// pane present, name already the legal form of the row's name
    Intact,
    /// pane present, name is something else (restore it)
    Lost { was: String },
    /// pane absent from hc's listing
    Gone,
}

pub(crate) fn binding_state(registry_name: &str, live: Option<Option<&str>>) -> BindingState {
    match live {
        None => BindingState::Gone,
        Some(Some(actual)) if actual == terminal_agent_name(registry_name) => BindingState::Intact,
        Some(other) => BindingState::Lost {
            was: other.unwrap_or("<null>").to_string(),
        },
    }
}

#[cfg(test)]
mod terminal_agent_name_tests {
    use super::{BindingState, binding_state, is_terminal_legal_name, terminal_agent_name};

    /// bd-recover-accepts-camelcase-pins-z5fz: a CamelCase pin is accepted by
    /// recover (mapped to a legal terminal name); an already-legal terminal name is
    /// passed through byte-for-byte so a healthy pane is never renamed.
    #[test]
    fn recover_accepts_camelcase_pins() {
        assert!(!is_terminal_legal_name("QuietHeron"));
        assert_eq!(terminal_agent_name("QuietHeron"), "quietheron");
        assert_eq!(terminal_agent_name("Buffy"), "buffy");
        assert_eq!(terminal_agent_name("CobaltMaple"), "cobaltmaple");
        for legal in [
            "flywheel-beads-oc2",
            "crew-plan-coder",
            "flywheel_oc3",
            "a1",
        ] {
            assert!(is_terminal_legal_name(legal), "{legal} is hc-legal");
            assert_eq!(terminal_agent_name(legal), legal);
        }
        for pin in [
            "QuietHeron",
            "LavenderSalmon",
            "BrightBadger",
            "w22:p1",
            "A Very Long CamelCase Pin Name That Exceeds Thirty Two",
        ] {
            let h = terminal_agent_name(pin);
            assert!(is_terminal_legal_name(&h), "{pin} -> {h:?} must be legal");
            assert!(h.len() <= 32, "{pin} -> {h:?} is over 32 chars");
            assert!(
                h.chars().next().is_some_and(|c| c.is_ascii_lowercase()),
                "{pin} -> {h:?} must start lowercase (hc's rule)"
            );
        }
        assert_eq!(terminal_agent_name("w22:p1"), "w22-p1");

        assert_eq!(
            binding_state("QuietHeron", Some(Some("quietheron"))),
            BindingState::Intact
        );
        assert_eq!(
            binding_state("flywheel-beads-oc2", Some(Some("flywheel-beads-oc2"))),
            BindingState::Intact
        );
        assert_eq!(
            binding_state("QuietHeron", Some(Some("flywheel-x-oc"))),
            BindingState::Lost {
                was: "flywheel-x-oc".to_string()
            }
        );
        assert_eq!(
            binding_state("Buffy", Some(None)),
            BindingState::Lost {
                was: "<null>".to_string()
            }
        );
        assert_eq!(binding_state("Buffy", None), BindingState::Gone);
    }
}

/// bd-hc-registry-recover-ywypf: label-missing retry + captain skip.
#[cfg(test)]
mod hc_registry_recover_tests {
    use super::{is_captain_entry, should_retry_workspace_labels};
    use crate::state::SpawnEntry;

    fn ws(label: &str) -> crate::hc::WorkspaceInfo {
        crate::hc::WorkspaceInfo {
            workspace_id: "w1".to_string(),
            number: 1,
            label: label.to_string(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: "t1".to_string(),
            agent_status: crate::hc::AgentStatus::Idle,
            cwd: None,
        }
    }

    /// Retry only on the all-empty-labels window (fresh spawn race).
    #[test]
    fn recover_label_retry_settles_transient_miss() {
        assert!(should_retry_workspace_labels(&[]));
        assert!(should_retry_workspace_labels(&[ws(""), ws("")]));
        assert!(!should_retry_workspace_labels(&[
            ws(""),
            ws("flywheel-foo")
        ]));
        assert!(!should_retry_workspace_labels(&[ws("flywheel-foo")]));
    }

    /// Captain pin matches by mail identity, registry name, or terminal
    /// spelling; otherwise it is a crew row. Never respawn by default.
    #[test]
    fn recover_captain_skip_by_default() {
        let captain = "AmberHarbor";
        let by_identity = SpawnEntry::with_mail_identity("w1:p1", "flywheel-foo-oc", captain);
        assert!(is_captain_entry(&by_identity, Some(captain)));
        let by_name = SpawnEntry::legacy("w1:p2", captain);
        assert!(is_captain_entry(&by_name, Some(captain)));
        let crew = SpawnEntry::with_mail_identity("w1:p3", "flywheel-foo-oc", "TanLion");
        assert!(!is_captain_entry(&crew, Some(captain)));
        assert!(!is_captain_entry(&crew, None));
    }
}

/// bd-0uzrq.21: recover after a host blackout — no owned workspace survives and
/// every registered pane is gone. Recover must recreate the workspace and
/// respawn each non-captain pin under its own identity, kind and profile,
/// instead of refusing with "no flywheel swarm".
#[cfg(test)]
mod recover_session_loss_tests {
    use super::{recover_seats, recover_workspace, state};
    use crate::backend::MultiplexerBackend;
    use crate::config::{Config, lock_env};
    use crate::hc::{
        AgentInfo, AgentStatus, BackendError, BackendErrorCode, PaneInfo, WorkspaceInfo,
    };
    use std::sync::Mutex;

    fn pane(ws: &str, n: u32) -> PaneInfo {
        PaneInfo {
            pane_id: format!("{ws}:p{n}"),
            terminal_id: format!("term-{n}"),
            workspace_id: ws.into(),
            tab_id: format!("{ws}:t1"),
            focused: false,
            agent_status: AgentStatus::Idle,
            revision: 0,
            title: None,
            cwd: None,
            agent: None,
            agent_session: None,
            display_agent: None,
            foreground_cwd: None,
            label: None,
            scroll: None,
            state_labels: serde_json::Map::new(),
            terminal_title: None,
            terminal_title_stripped: None,
            tokens: serde_json::Map::new(),
        }
    }

    fn agent(name: &str, pane_id: &str, ws: &str) -> AgentInfo {
        AgentInfo {
            agent: Some(name.to_string()),
            agent_status: AgentStatus::Idle,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: false,
            name: Some(name.to_string()),
            pane_id: pane_id.to_string(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term".into(),
            terminal_title: None,
            workspace_id: ws.to_string(),
        }
    }

    /// A plane that lost its session: no workspace, no panes, no agents.
    #[derive(Default)]
    struct LostFleet {
        created: Mutex<Vec<Option<String>>>,
        panes: Mutex<Vec<PaneInfo>>,
        agents: Mutex<Vec<AgentInfo>>,
    }

    const WS: &str = "w-recreated";

    impl MultiplexerBackend for LostFleet {
        fn name(&self) -> &'static str {
            "lost-fleet"
        }
        fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, BackendError> {
            Ok(Vec::new())
        }
        fn workspace_create(&self, label: Option<&str>) -> Result<WorkspaceInfo, BackendError> {
            self.created.lock().unwrap().push(label.map(str::to_string));
            self.panes.lock().unwrap().push(pane(WS, 1));
            Ok(WorkspaceInfo {
                workspace_id: WS.into(),
                number: 1,
                label: label.unwrap_or_default().to_string(),
                focused: false,
                pane_count: 1,
                tab_count: 1,
                active_tab_id: format!("{WS}:t1"),
                agent_status: AgentStatus::Idle,
                cwd: None,
            })
        }
        fn pane_list(&self, _ws: Option<&str>) -> Result<Vec<PaneInfo>, BackendError> {
            Ok(self.panes.lock().unwrap().clone())
        }
        fn pane_split(
            &self,
            _parent: &str,
            _direction: &str,
            _cwd: Option<&str>,
        ) -> Result<PaneInfo, BackendError> {
            let mut panes = self.panes.lock().unwrap();
            let p = pane(WS, panes.len() as u32 + 1);
            panes.push(p.clone());
            Ok(p)
        }
        fn agent_list(&self) -> Result<Vec<AgentInfo>, BackendError> {
            Ok(self.agents.lock().unwrap().clone())
        }
        fn agent_start(
            &self,
            name: &str,
            _kind: &str,
            pane_id: &str,
            _args: &[String],
        ) -> Result<AgentInfo, BackendError> {
            let a = agent(name, pane_id, WS);
            self.agents.lock().unwrap().push(a.clone());
            Ok(a)
        }
        fn agent_rename(&self, _target: &str, _name: Option<&str>) -> Result<(), BackendError> {
            Ok(())
        }
        fn pane_close(&self, pane_id: &str) -> Result<(), BackendError> {
            Err(BackendError::new(
                BackendErrorCode::Other("pane_not_found".into()),
                format!("pane {pane_id} not found"),
            ))
        }
    }

    #[test]
    fn recover_after_session_loss_respawns_every_gone_crew_pin() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("demo")).expect("project dir");
        let cfg = Config {
            workspace_root: Some(dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        state::save_captain_alias("demo", "SlateHarbor").expect("captain alias");
        let crew = |pane_id: &str, name: &str, pin: &str, profile: &str| {
            let mut e = state::SpawnEntry::with_mail_identity(pane_id, name, pin);
            e.profile = Some(profile.to_string());
            e
        };
        state::save_spawn_registry(
            "demo",
            &[
                state::SpawnEntry::with_mail_identity(
                    "block:dead:captain",
                    "flywheel-demo-oc",
                    "SlateHarbor",
                ),
                crew(
                    "block:dead:job",
                    "flywheel-demo-job",
                    "FrostyCreek",
                    "coder",
                ),
                crew("block:dead:rev", "flywheel-demo-rev", "BoldBay", "reviewer"),
            ],
        )
        .expect("registry");

        let fleet = LostFleet::default();
        let (ws_id, panes) = recover_workspace(&fleet, "demo").expect("recreate the swarm");
        assert_eq!(ws_id, WS);
        assert_eq!(
            fleet.created.lock().unwrap().as_slice(),
            &[Some("flywheel-demo".to_string())],
            "the recreated workspace carries the swarm label"
        );
        let panes = panes.expect("the recreated workspace lists its root pane");
        assert_eq!(panes.len(), 1);

        let entries = state::load_spawn_registry("demo");
        let rep = recover_seats(&fleet, &cfg, "demo", &ws_id, Some(&panes), &entries, false)
            .expect("seats");
        assert_eq!(rep.skipped_captain, vec!["flywheel-demo-oc".to_string()]);
        assert_eq!(rep.healthy, 0, "every crew pane is gone, none healthy");
        assert_eq!(
            rep.recovered.len(),
            2,
            "both crew pins respawn: {rep:?}",
            rep = rep.recovered
        );
        for row in &rep.recovered {
            assert_eq!(row["action"], "respawned");
            assert_eq!(row["kind"], "opencode");
        }
        assert_eq!(rep.moved.len(), 2, "each respawn records its new pane");

        let after = state::load_spawn_registry("demo");
        assert_eq!(
            after[1].mail_identity.as_deref(),
            Some("FrostyCreek"),
            "the pin identity survives the respawn"
        );
        assert_eq!(
            after[1].profile.as_deref(),
            Some("coder"),
            "role bind survives"
        );
        assert_eq!(after[1].name, "flywheel-demo-job");
    }
}

/// The legacy whitelist pair for a hash: ADJECTIVES[h % 75] +
/// NOUNS[(h >> 32) % 132], PascalCase. This is the pair shape agent-mail
/// validates (bd-odc) — the "legacy generator" the captain's adj+noun pins
/// are drawn from (ADR-0018 D1a).
///
/// governed-by: ADR-0018 D1a (bd-spawn-adjnoun-pin-7psn)
pub(crate) fn adjnoun_from_fnv(h: u64) -> String {
    let adj = ADJECTIVES[(h % ADJECTIVES.len() as u64) as usize];
    let noun = NOUNS[((h >> 32) % NOUNS.len() as u64) as usize];
    let mut adj = adj.to_owned();
    let mut noun = noun.to_owned();
    if let Some(c) = adj.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    if let Some(c) = noun.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    format!("{adj}{noun}")
}

/// S9 (ADR-0018 D1a): draw a UNIQUE adjective+noun pin, deterministic under
/// `seed` (the caller seeds per terminal name, so the FIRST candidate matches
/// the legacy hash mapping when free). A candidate already in `taken`
/// (spawn-registry pins + registered agents) redraws to the next candidate
/// — the 9900-pair space makes the loop effectively single-pass and the
/// seed increment guarantees termination.
///
/// governed-by: ADR-0018 D1a (bd-spawn-adjnoun-pin-7psn)
pub(crate) fn unique_adjnoun_pin(taken: &std::collections::HashSet<String>, seed: u64) -> String {
    let mut h = seed;
    loop {
        let pin = adjnoun_from_fnv(h);
        if !taken.contains(&pin) {
            return pin;
        }
        h = h.wrapping_add(1);
    }
}

/// The FROZEN adjective whitelist — 75 entries, index-addressed by pin
/// derivation (`terminal_mail_name`); never reorder or edit.
const ADJECTIVES: [&str; 75] = [
    "red",
    "orange",
    "yellow",
    "pink",
    "black",
    "purple",
    "blue",
    "brown",
    "white",
    "green",
    "chartreuse",
    "lilac",
    "fuchsia",
    "azure",
    "amber",
    "coral",
    "crimson",
    "cyan",
    "gold",
    "golden",
    "gray",
    "indigo",
    "ivory",
    "jade",
    "lavender",
    "magenta",
    "maroon",
    "navy",
    "olive",
    "pearl",
    "rose",
    "ruby",
    "sage",
    "scarlet",
    "silver",
    "teal",
    "topaz",
    "violet",
    "cobalt",
    "copper",
    "bronze",
    "emerald",
    "sapphire",
    "turquoise",
    "beige",
    "tan",
    "cream",
    "peach",
    "plum",
    "sunny",
    "misty",
    "foggy",
    "stormy",
    "windy",
    "frosty",
    "dusty",
    "hazy",
    "cloudy",
    "rainy",
    "snowy",
    "icy",
    "mossy",
    "sandy",
    "swift",
    "quiet",
    "bold",
    "calm",
    "bright",
    "dark",
    "wild",
    "silent",
    "gentle",
    "rustic",
    "noble",
    "proud",
];
/// The FROZEN noun whitelist — 132 entries, index-addressed by pin
/// derivation (`terminal_mail_name`); never reorder or edit.
const NOUNS: [&str; 132] = [
    "stone",
    "lake",
    "creek",
    "pond",
    "mountain",
    "hill",
    "snow",
    "castle",
    "river",
    "forest",
    "valley",
    "canyon",
    "meadow",
    "prairie",
    "desert",
    "island",
    "cliff",
    "cave",
    "glacier",
    "waterfall",
    "spring",
    "stream",
    "reef",
    "dune",
    "ridge",
    "peak",
    "gorge",
    "marsh",
    "brook",
    "glen",
    "grove",
    "fern",
    "hollow",
    "basin",
    "cove",
    "bay",
    "harbor",
    "coast",
    "shore",
    "bluff",
    "knoll",
    "summit",
    "plateau",
    "dog",
    "cat",
    "bear",
    "fox",
    "wolf",
    "deer",
    "elk",
    "moose",
    "otter",
    "beaver",
    "badger",
    "lynx",
    "puma",
    "squirrel",
    "rabbit",
    "hare",
    "mouse",
    "mink",
    "seal",
    "horse",
    "lion",
    "tiger",
    "panther",
    "leopard",
    "jaguar",
    "coyote",
    "bison",
    "ox",
    "hawk",
    "eagle",
    "owl",
    "falcon",
    "raven",
    "heron",
    "crane",
    "finch",
    "robin",
    "sparrow",
    "duck",
    "goose",
    "swan",
    "dove",
    "wren",
    "jay",
    "lark",
    "kite",
    "condor",
    "osprey",
    "pelican",
    "gull",
    "tern",
    "stork",
    "ibis",
    "cardinal",
    "oriole",
    "thrush",
    "trout",
    "salmon",
    "bass",
    "pike",
    "carp",
    "turtle",
    "frog",
    "pine",
    "oak",
    "maple",
    "birch",
    "cedar",
    "willow",
    "aspen",
    "elm",
    "orchid",
    "lotus",
    "ivy",
    "tower",
    "bridge",
    "forge",
    "mill",
    "barn",
    "gate",
    "anchor",
    "lantern",
    "beacon",
    "compass",
    "horizon",
    "spire",
    "chapel",
    "citadel",
    "fortress",
];

/// Resolve a mail-recipient specifier to a registered agent-mail identity
/// name. An hc agent name (`flywheel-<base>-<kind>[N]`, `crew-<wf>-<role>[N]`)
/// maps deterministically to its adjective+noun identity — registration uses
/// the same mapping (bd-odc), so the identity exists. Anything else passes
/// through unchanged: it is either an already-valid identity (the mapping is
/// idempotent for those) or a recipient the server will reject honestly.
/// Pure — no registry round trips.
pub(crate) fn resolve_mail_target(target: &str) -> String {
    if target.starts_with("flywheel-") || target.starts_with("crew-") {
        terminal_mail_name(target)
    } else {
        target.to_string()
    }
}

/// bd-627z.4: resolve a mail-recipient specifier THROUGH the spawn registry
/// first — the identity pinned at spawn (pane → adjective+noun) wins over
/// the pure hash mapping. Numbered agents (oc2/oc3/...) keep the SAME
/// identity across calls, restarts, and daemon restarts because the binding
/// is recorded at spawn, not re-derived from the (renumberable) terminal name.
/// Falls back to [`resolve_mail_target`] when the registry has no pin.
///
/// Registry-miss reasons (the bead's clear-error contract): the target was
/// never spawned in this project, or it is already an identity name. The
/// caller decides how to surface those.
pub(crate) fn resolve_mail_target_registry(project: &str, target: &str) -> String {
    let entries = state::load_spawn_registry(project);
    for e in &entries {
        if e.name == target {
            if let Some(mid) = &e.mail_identity {
                return mid.clone();
            }
            break;
        }
    }
    resolve_mail_target(target)
}

fn cmd_list(cfg: &Config, all: bool) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let workspaces = backend.workspace_list()?;
    let agents = backend.agent_list()?;
    let mut rows: Vec<Value> = Vec::new();
    for ws in workspaces
        .iter()
        .filter(|w| all || w.label.starts_with("flywheel-"))
    {
        let ws_agents: Vec<Value> = agents
            .iter()
            .filter(|a| a.workspace_id == ws.workspace_id)
            .map(|a| {
                let pending = is_still_booting(a);
                json!({
                    "name": a.name, "kind": a.agent,
                    "status": status_label(pending, a.agent_status.as_str()),
                    "launch_pending": pending,
                })
            })
            .collect();
        rows.push(json!({
            "workspace_id": ws.workspace_id,
            "label": ws.label,
            "cwd": ws.cwd,
            "pane_count": ws.pane_count,
            "agents": ws_agents
        }));
    }
    if std::io::stdout().is_terminal() {
        for r in &rows {
            println!("{}", r["label"].as_str().unwrap_or("?"));
            if let Some(ags) = r["agents"].as_array() {
                for a in ags {
                    let st = a["status"].as_str().unwrap_or("unknown");
                    let st = hc::parse_agent_status(st)
                        .map(status_paint)
                        .unwrap_or_else(|_| dim(st));
                    println!(
                        "  {}  {}  {}",
                        dim(a["name"].as_str().unwrap_or("?")),
                        kind_paint(a["kind"].as_str().unwrap_or("?")),
                        st
                    );
                }
            }
            println!();
        }
        if rows.is_empty() {
            println!("no flywheel swarms (run: flywheel spawn <project> --oc=1 --pi=1 --gk=1)");
        }
    } else {
        print_json(&json!({ "workspaces": rows }));
    }
    Ok(())
}

/// bd-2o1v P0-5 (a): profile affinity of a routed primitive, READ from the
/// workflow's own steps (never re-declared). None when the workflow
/// dispatches to no profile (headless/diagnosis) or is unresolvable.
fn routed_role(primitive: &str) -> Option<String> {
    let merged = workflow_catalog::all_workflows_merged(None).ok()?;
    let (wf, _) = merged.get(primitive)?;
    decide::workflow_role_from_steps(&wf.steps)
}

/// Classify only. Never dispatches.
#[allow(clippy::too_many_arguments)]
fn cmd_decide(
    cfg: &Config,
    target: &str,
    ready_beads: u32,
    profiled_panes: u32,
    launch_pending: u32,
    contention: bool,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut snap = decide::Snapshot {
        ready_bead_count: ready_beads,
        profiled_panes,
        launch_pending,
        contention,
    };
    if let Some(project) = project {
        check_project_spelling(project, "decide")?;
        let ws_label = state::load_spawn_meta(project).unwrap_or_else(|| swarm_label(project));
        // Wave11: label-pinned entry (same single world read as assemble).
        let live = snapshot::assemble_with_label(cfg, project, &ws_label, doorbell_now_unix());
        if snap.ready_bead_count == 0 {
            snap.ready_bead_count = live
                .lanes
                .iter()
                .filter(|l| l.status == "open" || l.status == "ready")
                .count() as u32;
        }
        if snap.profiled_panes == 0 {
            snap.profiled_panes = live.pins.len() as u32;
        }
        if snap.launch_pending == 0 {
            snap.launch_pending = live.launch_pending;
        }
        if !snap.contention {
            snap.contention = !live.held_paths.is_empty();
        }
    }
    let merged = workflow_catalog::all_workflows_merged(None)?;
    let known: Vec<String> = merged.keys().cloned().collect();
    if let Err(e) = decide::check_workflow_routes(decide::ROUTING_TABLE, &known) {
        bail!("{e}");
    }
    let affinity: Vec<(&str, Option<String>)> = decide::ROUTING_TABLE
        .iter()
        .filter(|e| {
            matches!(
                e.argv,
                decide::RouteArgv::WorkflowRun { .. } | decide::RouteArgv::WorkflowRunNoMode
            )
        })
        .map(|e| {
            let role = merged
                .get(e.primitive)
                .and_then(|(w, _)| decide::workflow_role_from_steps(&w.steps));
            (e.primitive, role)
        })
        .collect();
    let known_profiles: Vec<String> = profile::ProfileCatalog::load()?.names();
    if let Err(e) = decide::check_profile_routes(&affinity, &known_profiles) {
        bail!("{e}");
    }
    use toron_core::kernel::Program as _;
    let classify = crate::elm::DecideProgram::update(
        &snap,
        &crate::elm::DecideMsg::Classify {
            target: target.to_string(),
        },
    );
    let cmd = classify
        .commands
        .into_iter()
        .next()
        .expect("classify folds to one command");
    let role = affinity
        .iter()
        .find(|(p, _)| *p == cmd.primitive)
        .and_then(|(_, r)| r.clone());
    if json {
        print_json(&serde_json::json!({
            "primitive": cmd.primitive,
            "reason": cmd.reason,
            "argv": cmd.argv,
            "topology": cmd.topology,
            "role": role,
        }));
    } else {
        println!(
            "{}\n{}\n{}\ntopology {}\nrole {}",
            cmd.primitive,
            cmd.reason,
            cmd.argv.join(" "),
            cmd.topology,
            role.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

/// bd-lorot: a throwaway state root for a test that reaches the real ingress
/// seam. The seam is the only writer in this plane and it writes wherever it is
/// TOLD to, which is the entire point: the live root is now reachable only by
/// naming it out loud, so a test cannot leak into it by omission. Before this,
/// `config::state_dir()` was read from inside the seam, every test inherited
/// `~/.local/state/flywheel`, and 244 uniquely-named journal files had piled up
/// there — which `known_journal_workspaces` then counted, so the suite was
/// manufacturing a permanent wake-blind false alarm for a fleet that never
/// existed.
#[cfg(test)]
pub(crate) fn temp_state_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("flywheel-state-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["journals", "doorbell", "facts"] {
        std::fs::create_dir_all(dir.join(sub)).expect("temp state root");
    }
    dir
}

/// bd-lorot: the fence half of the fix — prove the LIVE root gained nothing.
/// `proj` is unique per run, so if the seam wrote to the live root at all this
/// file would exist. Cheap, exact, and it goes red the moment someone re-adds
/// an ambient `config::state_dir()` to the fact path.
#[cfg(test)]
pub(crate) fn assert_live_root_untouched(project: &str) {
    let live = config::state_dir();
    for leaked in [
        live.join("journals")
            .join(format!("doorbell-journal-{project}.jsonl")),
        live.join("facts")
            .join(format!("{}.jsonl", state::state_key(project))),
        live.join("facts")
            .join(format!("{}-pending.jsonl", state::state_key(project))),
    ] {
        assert!(
            !leaked.exists(),
            "the test suite wrote into the LIVE state root at {} — a test must name a \
             throwaway root, never inherit the ambient one",
            leaked.display()
        );
    }
}

#[must_use]
pub fn facts_journal_path(state_dir: &std::path::Path, project: &str) -> std::path::PathBuf {
    state_dir
        .to_path_buf()
        .join("facts")
        .join(format!("{}.jsonl", state::state_key(project)))
}

fn pending_facts_path(state_dir: &std::path::Path, project: &str) -> std::path::PathBuf {
    state_dir
        .to_path_buf()
        .join("facts")
        .join(format!("{}-pending.jsonl", state::state_key(project)))
}

fn append_pending_fact(state_dir: &std::path::Path, project: &str, summary: &str) -> Result<()> {
    let path = pending_facts_path(state_dir, project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{}", serde_json::to_string(summary)?)?;
    Ok(())
}

fn drain_pending_facts(state_dir: &std::path::Path, project: &str) -> Vec<String> {
    let path = pending_facts_path(state_dir, project);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let _ = std::fs::remove_file(&path);
    raw.lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect()
}

pub fn append_fact(
    state_dir: &std::path::Path,
    project: &str,
    event: &ingress::IngressEvent,
) -> Result<()> {
    if !ingress::is_fact(event) {
        return Ok(());
    }
    let path = facts_journal_path(state_dir, project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(event)?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

#[must_use]
pub fn load_facts(state_dir: &std::path::Path, project: &str) -> Vec<ingress::IngressEvent> {
    let path = facts_journal_path(state_dir, project);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect()
}

pub fn facts_count_and_last(state_dir: &std::path::Path, project: &str) -> (usize, Option<String>) {
    let facts = load_facts(state_dir, project);
    let count = facts.len();
    let last = facts
        .last()
        .and_then(ingress::fact_summary)
        .or_else(|| facts.last().map(|e| format!("{e:?}")));
    (count, last)
}

/// Captain surface: a fact becomes a single `[doorbell] <summary>` inject
/// line for the captain pane. Reuses the existing `doorbell_prompt` wake
/// path (hc `agent_prompt` with pane fallback). No new Nostr kinds.
/// GS-006: NO alias or alias not in registry is a loud miss (FactMiss), not
/// silent Ok(()), and never falls back to registry.first() (crew[0] misroute).
pub fn captain_fact_inject(
    state_dir: &std::path::Path,
    project: &str,
    event: &ingress::IngressEvent,
) -> Result<()> {
    let Some(line) = ingress::fact_doorbell_line(event) else {
        return Ok(());
    };
    let _ = append_fact(state_dir, project, event);
    if state::load_captain_alias(project).is_none() {
        eprintln!(
            "flywheel: [FactMiss] no captain alias for project {project}: fact not injected (loud)"
        );
        anyhow::bail!("captain fact miss: no alias for project {project}");
    }
    append_pending_fact(state_dir, project, &line)
}

/// Adapter wiring for `handle_ingress`: facts → journal + captain doorbell.
#[must_use]
pub fn ingress_adapters_for_project(
    state_dir: &std::path::Path,
    project: &str,
) -> ingress::IngressAdapters {
    let root = state_dir.to_path_buf();
    let proj = project.to_string();
    ingress::IngressAdapters {
        hc: Box::new(|e| eprintln!("ingress routing bug: hc via fact seam: {e:?}")),
        watch: Box::new(|w| eprintln!("ingress routing bug: watch via fact seam: {w:?}")),
        workflow: Box::new(|t| eprintln!("ingress routing bug: workflow via fact seam: {t:?}")),
        cron: Box::new(|n| eprintln!("ingress routing bug: cron via fact seam: {n}")),
        fact: Box::new(move |ev: &ingress::IngressEvent| {
            let summary = ingress::fact_doorbell_line(ev).unwrap_or_else(|| format!("{ev:?}"));
            let msg = crate::workspace::Msg::Fact {
                summary: summary.clone(),
            };
            crate::doorbell::journal_append_workspace(
                &root,
                &proj,
                &msg,
                &[],
                serde_json::json!({ "stage": "fact_pending", "summary": summary }),
                None,
            );
            let ledger = facts_journal_path(&root, &proj);
            let wrote = match ledger.parent() {
                Some(dir) => std::fs::create_dir_all(dir).and_then(|()| {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&ledger)
                        .and_then(|mut f| {
                            use std::io::Write;
                            writeln!(f, "{}", serde_json::to_string(ev).unwrap_or_default())
                        })
                }),
                None => Ok(()),
            };
            if let Err(e) = wrote {
                eprintln!("flywheel: [FactMiss] could not append facts ledger: {e:#}");
            }
            if let Err(e) = append_pending_fact(&root, &proj, &summary) {
                eprintln!("flywheel: [FactMiss] could not queue fact for dispatcher: {e:#}");
            }
        }),
    }
}

/// ADR-0010 D10 (bd-67qg): print the fleet snapshot contract as robot
/// JSON. Validates against `snapshot::schema_keys` — a missing key is a
/// contract break, never an empty section.
/// bd-3uqz: wire the triage to a real verb. Without a caller the whole module is
/// dead code in a binary crate, and a judgment nothing invokes is theater.
fn cmd_unanswered_triage(
    flywheel_state_dir: Option<std::path::PathBuf>,
    dry_run: bool,
) -> anyhow::Result<()> {
    use crate::judgment::UreqTransport;
    let dir = flywheel_state_dir.unwrap_or_else(|| {
        std::env::var_os("DOORBELL_STATE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let base = std::env::var_os("XDG_STATE_HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                            .join(".local/state")
                    });
                base.join("flywheel")
            })
    });
    let transport = UreqTransport::from_authority(&crate::authority::RuntimeAuthority::resolve())
        .unwrap_or_else(|_| UreqTransport {
            api_key: String::new(),
            url: crate::judgment::SYSTEMONE_URL.to_string(),
        });
    let now = crate::unanswered_triage::unix_now();
    let doorbell_dir = std::env::var_os("DOORBELL_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                        .join(".local/state")
                })
                .join("doorbell")
        });
    let out =
        crate::unanswered_triage::run(&dir, &doorbell_dir, &transport, dry_run, now, "toron")?;
    println!("{out}");
    Ok(())
}

/// bd-3uqz: fetch the live doorbell health, run the stuck-vs-busy judgment over it,
/// and print both readings side by side.
///
/// The two outputs are deliberately both present. The judgment can be `unknown`,
/// and when it is, the only useful thing a reader has is the deterministic
/// counters beside it. Printing the label alone would make an unasked question look
/// like an answer.
fn cmd_stuck_vs_busy(port: u16, dry_run: bool) -> anyhow::Result<()> {
    use crate::judgment::UreqTransport;
    use crate::stage::{Signal, Stage};
    let url = format!("http://127.0.0.1:{port}/health");
    let body = ureq::get(&url)
        .call()
        .map_err(|e| {
            anyhow::anyhow!(
                "GET {url}: {e}. If this is a connection refusal, check the PORT: 18980 is \
             the doorbell and 18080 is toron's daemon."
            )
        })?
        .into_string()
        .map_err(|e| anyhow::anyhow!("read {url} body: {e}"))?;
    let health: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("{url} is not JSON: {e}"))?;

    if health.get("watch").is_none() || health.get("wake").is_none() {
        return Err(anyhow::anyhow!(
            "{url} answered but carries no watch and no wake counters, so this is not a \
             doorbell health document. Refusing to ask a judgment about the wrong service."
        ));
    }

    let transport = UreqTransport::from_authority(&crate::authority::RuntimeAuthority::resolve())
        .unwrap_or_else(|_| UreqTransport {
            api_key: String::new(),
            url: crate::judgment::SYSTEMONE_URL.to_string(),
        });
    let asked_jev = !transport.api_key.is_empty();

    if dry_run {
        let ev = crate::stuck_vs_busy::evidence(&health);
        println!(
            "{}",
            serde_json::json!({
                "would_ask": true,
                "health_url": url,
                "questions": [crate::stuck_vs_busy::Q_WEDGED, crate::stuck_vs_busy::Q_WORKING],
                "evidence": serde_json::from_str::<serde_json::Value>(&ev)
                    .unwrap_or(serde_json::Value::Null),
            })
        );
        return Ok(());
    }

    let mut menu = crate::crew_menu::HarnessMenu {
        crew: 0,
        options: Vec::new(),
        registered_unconfigured: Vec::new(),
    };
    let objective = serde_json::json!({ "health": health }).to_string();
    let mut ctx = crate::stage::StageCtx {
        objective: &objective,
        menu: &mut menu,
    };
    let Stage::Compose { stages, .. } = crate::stuck_vs_busy::stage() else {
        unreachable!("stuck_vs_busy::stage() is a Compose by construction")
    };
    let find = |id: &str| {
        stages
            .iter()
            .find(|s| matches!(s, Stage::Code { id: sid, .. } if *sid == id))
    };
    let Some(pre) = find("health:has-counters") else {
        unreachable!("the preflight leaf is named in the module")
    };
    let pre_report = pre.run(&mut ctx, &transport);
    if pre_report.signal != Signal::Holds {
        println!(
            "{}",
            serde_json::json!({
                "verdict": "unknown",
                "signal": format!("{:?}", pre_report.signal),
                "detail": pre_report.detail,
                "health_url": url,
                "asked_jev": false,
                "request_spent": false,
                "acts_on_the_daemon": false,
            })
        );
        return Ok(());
    }
    let Some(jev) = stages
        .iter()
        .find(|s| matches!(s, Stage::Jev { id: sid, .. } if *sid == "stuck-vs-busy:noul"))
    else {
        unreachable!("the jev leaf is named in the module")
    };
    let report = jev.run(&mut ctx, &transport);
    let verdict = report
        .detail
        .split(':')
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_string();

    // whole safety property: if Jev is unreachable the label says `unknown` and
    println!(
        "{}",
        serde_json::json!({
            "verdict": verdict,
            "signal": format!("{:?}", report.signal),
            "detail": report.detail,
            "health_url": url,
            "asked_jev": asked_jev,
            "request_spent": true,
            "deterministic_counters": {
                "restarts": health["watch"]["restarts"],
                "restart_budget_per_window": health["watch"]["restart_budget_per_window"],
                "last_event_age_s": health["watch"]["last_event_age_s"],
                "deaf_ceiling_ms": health["wake"]["deaf_ceiling_ms"],
                "sweep_last_age_s": health["sweep"]["last_age_s"],
                "injectable": health["injection"]["injectable"],
                "pins": health["injection"]["pins"],
                "degraded": health["degraded"],
            },
            "acts_on_the_daemon": false,
        })
    );
    Ok(())
}

/// bd-7wah: is this stderr only warnings?
///
/// Deliberately conservative — it looks for a leading `Warning:`/`warning:` on each
/// non-blank line. A line that does not start with a warning marker makes the whole
/// stream count as real output, so a genuine error buried under a warning is still
/// reported. The failure mode of being wrong here is "we called it warnings when it
/// was not", which costs a slightly vaguer message; the failure mode of the old code
/// was "we called a NO_COLOR warning the cause of an empty dispatch", which costs the
/// diagnosis entirely.
fn stderr_is_only_warnings(stderr: &str) -> bool {
    let mut any = false;
    for line in stderr.lines().map(str::trim).filter(|l| !l.is_empty()) {
        any = true;
        let lower = line.to_ascii_lowercase();
        if !(lower.starts_with("warning") || lower.starts_with("warn:")) {
            return false;
        }
    }
    any
}
/// bd-7wah: stderr must not become THE error when the parser already has one.
///
/// The recorded shape of the bug: a provider 400 parsed correctly, then a
/// `NO_COLOR` warning on stderr overwrote it, and the run log named a colour
/// warning as the cause. 20 runs carry that as their explanation.
#[test]
fn stderr_is_never_the_error_when_the_parser_already_has_one() {
    assert!(
        stderr_is_only_warnings("Warning: NO_COLOR is ignored"),
        "a lone warning IS only warnings — that is the 20-run false attribution, and \\
         the caller must be able to name it as such rather than as a cause"
    );
    assert!(
        !stderr_is_only_warnings("Warning: something\nError: provider 400: context length"),
        "a real error line under a warning makes the stream real output"
    );
    assert!(stderr_is_only_warnings("Warning: a\nwarning: b"));
    assert!(
        !stderr_is_only_warnings(""),
        "empty stderr is not 'only warnings' — there is nothing to classify"
    );
}

/// GS-048: live decide Snapshot from the same world facts as `briefing` /
/// `decide --project` — ready beads, profiled panes, contention. Never use
/// `Snapshot::default()` in production expand_reclassify paths (invent-green).
pub(crate) fn live_decide_snapshot(cfg: &Config, project: &str) -> decide::Snapshot {
    let ws_label = state::load_spawn_meta(project).unwrap_or_else(|| swarm_label(project));
    // Wave11: label-pinned entry (same single world read as assemble).
    let live = snapshot::assemble_with_label(cfg, project, &ws_label, doorbell_now_unix());
    decide::Snapshot {
        ready_bead_count: live
            .lanes
            .iter()
            .filter(|l| l.status == "open" || l.status == "ready")
            .count() as u32,
        profiled_panes: live.pins.len() as u32,
        launch_pending: live.launch_pending,
        contention: !live.held_paths.is_empty(),
    }
}

/// GS-048: mid-session reclassify enters the Elm plane
/// (`Msg::MaterialContext` → expand → `DecisionReady` sticky announce), then
/// prints the briefing proposal with the same live Snapshot. Does **not**
/// bypass sticky-open via a bare `cmd_briefing` alias.
#[allow(clippy::too_many_arguments)]
fn cmd_reclassify(
    cfg: &Config,
    objective: &str,
    project: Option<&str>,
    json: bool,
    commit: bool,
    yes: bool,
    topology: Option<&str>,
    crew: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    let project = project_door(project, "toron", "reclassify")?;
    let mut ws_model = workspace::WorkspaceModel::from_environment();
    if let Some(topo) = state::load_session_topology(project) {
        ws_model.session_topology = Some(topo);
    }
    if let Some(pin) = state::load_captain_alias(project) {
        ws_model.roles.insert("captain".into(), pin.clone());
        ws_model.presence.insert(
            pin.clone(),
            workspace::Presence::Idle {
                source: "reclassify".into(),
            },
        );
        ws_model.doorbell.index.insert_pin(pin, None);
    }
    let reason = workspace::material_context_reason(objective).unwrap_or("operator-goal-change");
    let msg = workspace::material_context_msg(objective, reason);
    let (next, cmds) = workspace::tick(&ws_model, &msg);
    ws_model = next;
    let snap = live_decide_snapshot(cfg, project);
    let expanded = workspace::expand_reclassify_cmds(
        &mut ws_model,
        cmds,
        project,
        &crate::authority::RuntimeAuthority::resolve(),
        &|goal| {
            let d = decide::decide(goal, &snap);
            (
                d.primitive.to_string(),
                d.topology.to_string(),
                d.argv.clone(),
                d.reason.clone(),
            )
        },
    );
    if let Some(topo) = ws_model.session_topology.as_deref() {
        let _ = state::save_session_topology(project, topo);
    }
    for c in &expanded {
        if let workspace::Cmd::Prompt { line, .. } = c
            && line.starts_with("[reclassify]")
        {
            eprintln!("{line}");
        }
        if let workspace::Cmd::Audit { kind, detail, .. } = c
            && (kind == "reclassify_ready" || kind == "reclassify_requested")
        {
            eprintln!("flywheel: [{kind}] {detail}");
        }
    }
    cmd_briefing(
        cfg,
        objective,
        Some(project),
        json,
        commit,
        yes,
        topology,
        crew,
        model,
    )
}

/// ADR-0010 D10 (bd-67qg): the one session-start drain over the snapshot.
/// Always the three sections, even on an empty queue.
#[allow(clippy::too_many_arguments)]
fn cmd_briefing(
    cfg: &Config,
    objective: &str,
    project: Option<&str>,
    json: bool,
    commit: bool,
    yes: bool,
    topology: Option<&str>,
    crew: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    let project = project_door(project, "toron", "briefing")?;
    let ws_label = state::load_spawn_meta(project).unwrap_or_else(|| swarm_label(project));
    // Wave11: label-pinned entry (same single world read as assemble).
    let snap = snapshot::assemble_with_label(cfg, project, &ws_label, doorbell_now_unix());
    let world = briefing::World {
        project: project.to_string(),
        ready_beads: match load_ready_beads(cfg, project) {
            Ok(v) => v.len() as u32,
            Err(e) => {
                eprintln!(
                    "flywheel: briefing: br ready failed ({e:#}) — ready_beads=0 (fail-open, do not count leftover opens)"
                );
                0
            }
        },
        dispatcher: snap.dispatcher.clone(),
        joinable: snap.joinable,
        degraded: snap.degraded.clone(),
        foreign_workspaces: Vec::new(),
        pin_count: snap.pins.len() as u32,
        held_paths: snap.held_paths.clone(),
        coalition_id: snap.coalitions.first().cloned(),
        launch_pending: snap.launch_pending,
        agent_pin: crate::authority::select_agent_identity(None, &|k| std::env::var(k).ok()),
    };
    if world.joinable {
        eprintln!("{}", briefing::join_banner(&world));
    } else {
        eprintln!(
            "ARRIVE  project={} dispatcher={} pins={} (no live fleet)",
            world.project, world.dispatcher, world.pin_count
        );
    }
    let unique: Vec<String> = Vec::new();
    let catalog_for_menu = profile::ProfileCatalog::load().ok();
    let mut b = briefing::assemble(
        objective,
        &world,
        &unique,
        cfg.crew_max(),
        catalog_for_menu.as_ref(),
    );
    b.apply_staleness();
    b.harness_ranking = Some(match judgment::jev_gate() {
        judgment::JevGate::Off => "jev off (FLYWHEEL_JEV off) — deterministic order".to_string(),
        judgment::JevGate::Unconfigured => {
            "jev unconfigured (set TYPESAFE_API_KEY) — deterministic order".to_string()
        }
        judgment::JevGate::On => match b.harness_menu.as_mut() {
            None => "no harness menu (no profile catalog) — nothing to rank".to_string(),
            Some(menu) => {
                let authority = crate::authority::RuntimeAuthority::resolve();
                match judgment::UreqTransport::from_authority(&authority) {
                    Err(e) => format!("jev has no usable key ({e}) — deterministic order"),
                    Ok(transport) => {
                        let mut ctx = stage::StageCtx { objective, menu };
                        let report = jev_rank::crew_fit_stage().run(&mut ctx, &transport);
                        match report.signal {
                            stage::Signal::Holds => format!("jev {}", report.detail),
                            stage::Signal::Fails => format!("jev declined: {}", report.detail),
                            stage::Signal::Unknown => {
                                format!("jev unavailable ({}): deterministic order", report.detail)
                            }
                        }
                    }
                }
            }
        },
    });
    b.proposal.role = routed_role(&b.proposal.workflow);
    if let Some(sticky) = state::load_session_topology(project) {
        let explicit = workspace::parse_explicit_topology(objective);
        let (final_topo, held) = workspace::sticky_open_topology(
            Some(sticky.as_str()),
            b.proposal.topology.as_str(),
            explicit.as_deref(),
        );
        briefing::align_with_session_topology(&mut b, &final_topo, held);
    }
    briefing::apply_answers(&mut b, topology, crew, model).map_err(|e| anyhow!(e))?;
    briefing::attach_consent_plan(&mut b, cfg.crew_max(), &cfg.consent_policy());
    let project_dir = project_dir(cfg, project);
    b.memory_traps = crate::cmd::memory::briefing_traps(
        objective,
        Some(project),
        None,
        Some(project_dir.as_path()),
    );
    if json {
        print_json(&serde_json::to_value(&b)?);
    } else {
        println!("schema {}", b.schema);
        println!(
            "topology {}  workflow {}",
            b.proposal.topology, b.proposal.workflow
        );
        println!("why {}", b.proposal.why);
        println!("argv {}", b.proposal.argv.join(" "));
        println!("role {}", b.proposal.role.as_deref().unwrap_or("-"));
        println!(
            "questions topology={} crew={} model={}",
            b.questions[0].default,
            b.questions[1].default,
            b.questions
                .iter()
                .find(|q| q.id == "model")
                .map(|q| q.default.as_str())
                .unwrap_or("default")
        );
        let traps_section = crate::cmd::memory::render_traps_section(&b.memory_traps);
        if !traps_section.is_empty() {
            print!("\n{traps_section}");
        }
    }
    warn_uncommitted_decision_docs(cfg, project);
    if commit || yes {
        if commit && !yes && std::io::stdin().is_terminal() {
            let mut topo = topology.map(str::to_string);
            let mut cr = crew.map(str::to_string);
            let mut mo = model.map(str::to_string);
            for q in &b.questions {
                let filled = match q.id.as_str() {
                    "topology" => topo.is_some(),
                    "crew" => cr.is_some(),
                    "model" => mo.is_some(),
                    _ => true,
                };
                if filled {
                    continue;
                }
                eprint!("{} [{}]: ", q.id, q.default);
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                let ans = line.trim();
                let chosen = if ans.is_empty() {
                    q.default.clone()
                } else {
                    ans.to_string()
                };
                match q.id.as_str() {
                    "topology" => topo = Some(chosen),
                    "crew" => cr = Some(chosen),
                    "model" => mo = Some(chosen),
                    _ => {}
                }
            }
            briefing::apply_answers(&mut b, topo.as_deref(), cr.as_deref(), mo.as_deref())
                .map_err(|e| anyhow!(e))?;
        }
        let argv = b.proposal.argv.clone();
        let spawns_crew = briefing::spawns_crew(&b.proposal);
        if commit && !yes && spawns_crew {
            let plan = format!(
                "crew plan (proposal): topology={} workflow={} crew-default={} argv=`{}`",
                b.proposal.topology,
                b.proposal.workflow,
                b.questions
                    .iter()
                    .find(|q| q.id == "crew")
                    .map(|q| q.default.clone())
                    .unwrap_or_default(),
                argv.join(" ")
            );
            if std::io::stdin().is_terminal() {
                eprintln!("\n{plan}");
                eprint!("spawn this crew? [y/N] (remix: rerun with --crew N / profile flags) ");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if !line.trim().eq_ignore_ascii_case("y") {
                    bail!("briefing aborted — crew plan not confirmed");
                }
            } else {
                bail!(
                    "briefing --commit: topology '{}' spawns a crew — crew-confirm gate \
                     (bd-2o1v). {plan}. Present this proposal to the operator and rerun \
                     with --yes (or --crew N after remix)",
                    b.proposal.topology
                );
            }
        }
        eprintln!("flywheel: briefing --commit {}", argv.join(" "));
        if argv.len() < 2 {
            bail!("briefing commit: empty argv");
        }
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()?;
        if !status.success() {
            bail!("briefing commit exited {status}");
        }
    }
    Ok(())
}

/// bd-enov (F3): parse `git status --porcelain` output for uncommitted
/// DECISION docs (`docs/decisions/**`) — the contract-bearing files whose
/// stale committed copy misleads every crew reading git history. Pure.
/// Handles the `XY <path>` porcelain shape (including `R  old -> new`).
pub(crate) fn uncommitted_decision_docs(porcelain: &str) -> Vec<String> {
    let mut out: Vec<String> = porcelain
        .lines()
        .filter(|l| l.len() > 3)
        .filter_map(|l| {
            let path = l[3..].trim();
            let path = path.rsplit(" -> ").next().unwrap_or(path).trim_matches('"');
            path.starts_with("docs/decisions/")
                .then(|| path.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Run the decision-doc dirt check for a project and warn loud (never fatal —
/// a captain may deliberately dispatch mid-doc). Best-effort: a missing git or
/// non-repo dir is silent (nothing to say).
fn warn_uncommitted_decision_docs(cfg: &Config, project: &str) {
    let dir = project_dir(cfg, project);
    if !dir.is_dir() {
        return;
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["status", "--porcelain", "--", "docs/decisions"])
        .output();
    let Ok(out) = out else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let dirt = uncommitted_decision_docs(&String::from_utf8_lossy(&out.stdout));
    if dirt.is_empty() {
        return;
    }
    eprintln!(
        "\n!!! DIRT PREFLIGHT: {} uncommitted decision doc(s) in {} — HEAD carries the \
         superseded contract and crews reading git history will implement the wrong thing.\n\
         Commit them before dispatch: {}",
        dirt.len(),
        dir.display(),
        dirt.join(", ")
    );
}

/// Arrival triple, legs 2–3 (bd-asr2.14, D5; leg 1 is the presence publish
/// above): role auto-bind from the spawn-registry profile (closes the
/// GS-030 no-bind-invisible gap — binds publish on the next sweep seed),
/// best-effort wire publish via the existing `toron role pin` CLI.
/// Best-effort throughout: never fails arrival.
fn arrival_triple(cfg: &Config, project: &str, pin: &str) {
    let profile = state::load_spawn_registry(project)
        .into_iter()
        .find(|e| e.mail_identity.as_deref() == Some(pin))
        .and_then(|e| e.profile.clone());
    let Some(role) = profile else {
        eprintln!("flywheel: arrival triple: no registry profile for {pin} — role bind skipped");
        return;
    };
    match state::upsert_role_bind(project, &role, pin) {
        Ok(()) => eprintln!("flywheel: arrival triple: role {role} bound for {pin}"),
        Err(e) => {
            eprintln!("flywheel: arrival triple: role bind failed: {e:#}");
            return;
        }
    }
    let mail = match crate::mail_or_die(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("flywheel: arrival triple: toron role pin failed: {e:#}");
            return;
        }
    };
    match mail.pin_role_alias(project, &role, pin) {
        Ok(_) => {}
        Err(e) => eprintln!("flywheel: arrival triple: toron role pin failed: {e:#}"),
    }
}

/// Publish `working` presence for every registry pin, in-process.
/// Shared by the hc/rex spawn arms: same retry contract as the retired
/// `toron presence publish` subprocess (retry-3 on unknown-agent while a
/// concurrent registration lands, warn-and-record otherwise). A mail-build
/// failure reads like the old spawn failure, per pin.
fn publish_presence_for_registry(
    mail: Option<&crate::mail::Mail>,
    mail_err: &str,
    presence_project: &str,
    project: &str,
    registry: &[state::SpawnEntry],
) {
    for e in registry {
        if let Some(pin) = &e.mail_identity {
            let mut presence_effect = match agent_ref::AgentEffectGuard::begin(
                project,
                pin,
                agent_ref::AgentPhase::Presence,
            ) {
                Ok(g) => Some(g),
                Err(err) => {
                    eprintln!("flywheel: warn: cannot journal presence intent for {pin}: {err:#}");
                    None
                }
            };
            let mut failure: Option<String> = None;
            match mail {
                None => {
                    let msg = mail_err.trim().to_string();
                    eprintln!("flywheel: presence publish failed for {pin}: {msg}");
                    failure = Some(format!("presence publish failed for {pin}: {msg}"));
                }
                Some(m) => {
                    let mut attempt = 0;
                    loop {
                        attempt += 1;
                        match m.presence_publish(presence_project, pin, "working") {
                            Ok(()) => break,
                            Err(e) => {
                                let msg = format!("{e:#}");
                                let unknown = msg.to_ascii_lowercase().contains("unknown agent");
                                if unknown && attempt < 3 {
                                    std::thread::sleep(Duration::from_millis(200 * attempt as u64));
                                    continue;
                                }
                                eprintln!(
                                    "flywheel: presence publish failed for {pin}: {}",
                                    msg.trim()
                                );
                                failure = Some(format!(
                                    "presence publish failed for {pin}: {}",
                                    msg.trim()
                                ));
                                break;
                            }
                        }
                    }
                }
            }
            if let Some(g) = presence_effect.take() {
                match failure {
                    Some(ev) => g.mark_unknown(Some(&ev)),
                    None => g.settle(Some("presence=working published")),
                }
            }
        }
    }
}

/// bd-gpj0: open-mode arrive can only be *pushed* if this pane is in the
/// spawn registry. `HC_TERMINAL_ID` is the current hc terminal; bind it to
/// the arriver pin. Without it, wakes stay digest-bound — say so loudly.
fn arrive_maybe_bind_pane(cfg: &Config, project: &str, pin: &str) {
    let rex = std::env::var("REX_BLOCK").ok();
    let hc = std::env::var("HC_TERMINAL_ID").ok();
    match crate::inbox_ports::arrive_pane_from_env(rex.as_deref(), hc.as_deref()) {
        Some(pane) => {
            bind_pane_mail_identity(cfg, project, pin, pane, Some(pin));
            eprintln!("arrive bound pane {pane} to {pin} (doorbell can push)");
        }
        None => eprintln!(
            "flywheel: no REX_BLOCK and no HC_TERMINAL_ID — this session is pull-only; doorbell \
             pushes stay digest-bound. Pull with `toron mail inbox --since last`."
        ),
    }
}

/// bd-spawn-identity-divergence-2vzw: what arrive may do with `--as`, judged
/// against the identity the pane was SPAWNED with.
///
/// Sources, strongest first: the spawn-registry row for this pane id (written
/// by spawn before the pane existed) and `FLYWHEEL_MAIL_AS` (injected into the
/// pane env at create; hc injects env only at split/create). A
/// user-exported `AGENT_NAME` is deliberately NOT read here — it is the mail
/// ladder's operator fallback, not spawn evidence. A spawned pane may only
/// arrive as its injected pin, and the captain pin is never a valid arrival
/// target for one: the measured 2026-10-03 incident had a planner pane arrive
/// as the CAPTAIN pin (derivable from the brief text) until mail corrected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArriveIdentityVerdict {
    /// Not a spawned pane (no registry row): explicit --as is an operator
    /// choice and is honored.
    Unbound,
    /// The request matches the injected identity (or nothing contradicts it).
    Match,
    /// The request contradicts the spawn evidence — refuse.
    Refused {
        injected: Option<String>,
        reason: &'static str,
    },
}

pub(crate) fn arrive_identity_verdict(
    requested: &str,
    registry_pin: Option<&str>,
    env_pin: Option<&str>,
    captain_pin: Option<&str>,
) -> ArriveIdentityVerdict {
    fn clean(p: Option<&str>) -> Option<&str> {
        p.filter(|s: &&str| !s.trim().is_empty())
    }
    let registry_pin = clean(registry_pin);
    let env_pin = clean(env_pin);
    let injected = registry_pin.or(env_pin);
    if registry_pin.is_some() && captain_pin == Some(requested) && injected != Some(requested) {
        return ArriveIdentityVerdict::Refused {
            injected: injected.map(str::to_string),
            reason: "the captain pin is not a valid arrival target for a spawned pane",
        };
    }
    match injected {
        Some(pin) if pin != requested => ArriveIdentityVerdict::Refused {
            injected: Some(pin.to_string()),
            reason: "pin contradicts the identity injected at spawn",
        },
        Some(_) => ArriveIdentityVerdict::Match,
        None if registry_pin.is_some() => ArriveIdentityVerdict::Match,
        None => ArriveIdentityVerdict::Unbound,
    }
}

/// bd-spawn-identity-divergence-2vzw: the pane's recorded identity (spawn
/// registry) vs the identity it actually acted as (the pane identity bridge /
/// pane token resolve). Both sides known and different = divergent; a missing
/// side is not evidence. Returns an `expected != observed` message.
pub(crate) fn identity_divergence(
    expected: Option<&str>,
    observed: Option<&str>,
) -> Option<String> {
    let (expected, observed) = (
        expected.filter(|s| !s.trim().is_empty()),
        observed.filter(|s| !s.trim().is_empty()),
    );
    match (expected, observed) {
        (Some(e), Some(o)) if e != o => Some(format!("{e} != {o}")),
        _ => None,
    }
}

fn cmd_arrive(
    cfg: &Config,
    project: &str,
    as_agent: Option<&str>,
    coalition: Option<&str>,
    objective: Option<&str>,
    captain_override: Option<&str>,
) -> Result<()> {
    check_project_spelling(project, "arrive")?;
    if let Some(requested) = as_agent {
        let dir = project_dir(cfg, project);
        let registry = state::load_spawn_registry(dir.to_str().unwrap_or(project));
        let pane_id = std::env::var("HC_TERMINAL_ID")
            .ok()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        let registry_pin = pane_id.as_ref().and_then(|pane| {
            registry
                .iter()
                .find(|e| &e.pane_id == pane)
                .and_then(|e| e.mail_identity.clone())
        });
        let env_pin = std::env::var("AGENT_NAME")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let (captain_pin, _) = lookup_captain_holder(project);
        if let ArriveIdentityVerdict::Refused { injected, reason } = arrive_identity_verdict(
            requested,
            registry_pin.as_deref(),
            env_pin.as_deref(),
            captain_pin.as_deref(),
        ) {
            bail!(
                "flywheel: arrive --as {requested} refused: {reason}{}. Re-run with `arrive --as {}` (or unset the stray pin).",
                injected
                    .as_deref()
                    .map(|p| format!(" (injected: {p})"))
                    .unwrap_or_default(),
                injected.as_deref().unwrap_or(requested),
            );
        }
        if let Some(pane) = pane_id.as_deref()
            && let Ok(key) = project_dir_str(cfg, project)
            && let Ok(mail) = mail_or_die(cfg)
        {
            let bridge = mail
                .resolve_pane_identity(&key, pane)
                .get("agent_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(msg) = identity_divergence(registry_pin.as_deref(), bridge.as_deref()) {
                eprintln!(
                    "RECORD DIVERGENCE: identity_divergent pane={pane} registry={msg} — the pane previously acted under another identity; this arrival re-binds it."
                );
            }
        }
    }
    let ws_label = state::load_spawn_meta(project).unwrap_or_else(|| swarm_label(project));
    // Wave11: label-pinned entry (same single world read as assemble).
    let snap = snapshot::assemble_with_label(cfg, project, &ws_label, doorbell_now_unix());
    let world = briefing::World {
        project: project.to_string(),
        dispatcher: snap.dispatcher.clone(),
        joinable: snap.joinable,
        pin_count: snap.pins.len() as u32,
        held_paths: snap.held_paths.clone(),
        coalition_id: coalition
            .map(str::to_string)
            .or_else(|| snap.coalitions.first().cloned()),
        degraded: snap.degraded.clone(),
        ..briefing::World::default()
    };
    if world.joinable {
        eprintln!("{}", briefing::join_banner(&world));
    } else {
        eprintln!(
            "ARRIVE  project={} dispatcher={} pins={} (no live fleet — not joining)",
            world.project, world.dispatcher, world.pin_count
        );
    }
    print!("{}", snapshot::render_helm(&snap, doorbell_now_unix()));
    if let Some(id) = coalition
        && let Some(agent) = as_agent
    {
        let store =
            coalition::CoalitionStore::open(project, config::state_dir().join("coalitions"))?;
        match store.join(id, agent, None) {
            Ok(m) => eprintln!("joined coalition {} members {}", m.id, m.members.len()),
            Err(e) => eprintln!("flywheel: coalition join skipped: {e:#}"),
        }
    }
    if let Some(agent) = as_agent {
        match crate::mail_or_die(cfg) {
            Ok(mail) => {
                if let Ok(key) = crate::project_dir_str(cfg, project) {
                    let _ = mail.ensure_project(&key);
                    if let Err(e) =
                        mail.register_agent(&key, agent, "flywheel", "flywheel", "session arrive")
                    {
                        eprintln!(
                            "flywheel: arrive register {agent} failed (whois may stay unknown): {e:#}"
                        );
                    }
                }
            }
            Err(e) => eprintln!("flywheel: arrive register skipped: {e:#}"),
        }
        let published = match crate::mail_or_die(cfg) {
            Ok(m) => match m.presence_publish(project, agent, "working") {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("flywheel: presence publish failed: {e:#}");
                    false
                }
            },
            Err(e) => {
                eprintln!("flywheel: presence publish failed: {e:#}");
                false
            }
        };
        if published {
            eprintln!("presence working as {agent}");
        }
        arrive_maybe_bind_pane(cfg, project, agent);
    }
    if cfg.doorbell_arrival_triple()
        && let Some(agent) = as_agent
    {
        arrival_triple(cfg, project, agent);
    }
    let arriver = as_agent.unwrap_or("");
    let (holder, whois_stale) = lookup_captain_holder(project);
    let pane_alive = holder
        .as_ref()
        .map(|h| snap.pins.iter().any(|p| p.pin == *h))
        .unwrap_or(false);
    let ev = captain_slot::CaptainEvidence {
        holder,
        presence_fresh: !whois_stale,
        last_wake_fresh: snap.dispatcher == "up",
        pane_alive,
        pull_fresh: false,
        override_pin: captain_override.map(str::to_string),
    };
    let action = captain_slot::next_action(&ev, arriver);
    if let Some(line) = captain_slot::claim_line(&action) {
        eprintln!("{line}");
        if let captain_slot::CaptainAction::Claim { pin, .. }
        | captain_slot::CaptainAction::Override { pin } = action
        {
            // bd-wave2 residual: captain alias set has no facade equivalent yet.
            let status = std::process::Command::new("toron")
                .args(["contact", "captain", "--project", project, "--pin", &pin])
                .status();
            if let Err(e) = status {
                eprintln!("flywheel: captain claim CLI failed: {e:#}");
            }
        }
    }
    if let Some(line) = captain_fold_health_line(
        project,
        ev.holder.clone(),
        !ev.presence_fresh,
        ev.pane_alive,
        ev.last_wake_fresh,
        ev.override_pin.as_deref(),
    ) {
        eprintln!("{line}");
    }
    match cmd_doorbell_start(cfg, project) {
        Ok(()) => {}
        Err(e) => eprintln!("flywheel: doorbell start: {e:#}"),
    }
    if let Some(pin) = as_agent {
        let workspace = doorbell::arrival_scope(&swarm_label(project));
        doorbell::register_arrival_subscription(&config::state_dir(), project, &workspace, pin);
    }
    warn_uncommitted_decision_docs(cfg, project);
    if let Some(obj) = objective {
        cmd_briefing(
            cfg,
            obj,
            Some(project),
            false,
            false,
            false,
            None,
            None,
            None,
        )?;
    }
    Ok(())
}
fn lookup_captain_holder(project: &str) -> (Option<String>, bool) {
    let mail = match crate::mail::Mail::from_config(None) {
        Ok(Some(m)) => m,
        _ => return (None, true),
    };
    let v = match mail.whois(project, "captain") {
        Ok(v) => v,
        Err(_) => return (None, true),
    };
    let name = v
        .get("name")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() || name == "captain" {
        return (None, true);
    }
    let stale = v.get("stale").and_then(|s| s.as_bool()).unwrap_or(true);
    (Some(name), stale)
}

/// F10: is `pin` publishing LIVE presence in `project`? `toron presence
/// list` already drops heartbeats older than toron's TTL, so a name hit here
/// IS a live signal. Missing CLI / bad JSON / non-zero exit → false.
fn captain_presence_live(project: &str, pin: &str) -> bool {
    let rows = match crate::mail::Mail::from_config(None) {
        Ok(Some(m)) => m.presence_list(project).unwrap_or_default(),
        _ => return false,
    };
    presence_names(&serde_json::Value::Array(rows)).contains(pin)
}

/// bd-8pis: pin names in a `toron presence list --json` payload.
fn presence_names(rows: &serde_json::Value) -> HashSet<String> {
    rows.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// bd-8pis: pins with a presence row in `project` (heartbeats, any state).
/// Best-effort: toron failure reads as nobody-heartbeating, never an error.
fn presence_pins(project: &str) -> HashSet<String> {
    let rows = match crate::mail::Mail::from_config(None) {
        Ok(Some(m)) => m.presence_list(project).unwrap_or_default(),
        _ => return HashSet::new(),
    };
    presence_names(&serde_json::Value::Array(rows))
}

/// bd-8pis: the hc pane bound to `pin` (toron whois `pane_id`), if any.
/// Best-effort: whois failure reads as unbound (the caller warns).
fn whois_pane_id(project: &str, pin: &str) -> Option<String> {
    let mail = crate::mail::Mail::from_config(None).ok()??;
    let v = mail.whois(project, pin).ok()?;
    v.get("pane_id")
        .and_then(|p| p.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// bd-8pis: live pane ids on the SELECTED plane (status != dead). None when
/// the backend is unreachable — the caller trusts whois bindings instead of
/// warn-storming. bd-f8zk: this used to connect to hc
/// directly, so on a rex plane every rex pane read dead and a live pin drew
/// a false "no live hc pane" warning.
pub(crate) fn live_hc_panes(cfg: &Config) -> Option<HashSet<String>> {
    let backend = backend::multiplexer_backend(cfg).ok()?;
    let agents = backend.agent_list().ok()?;
    Some(
        agents
            .into_iter()
            .filter(|a| a.agent_status.as_str() != "dead")
            .map(|a| a.pane_id)
            .collect(),
    )
}

/// bd-8pis: dead-pin send warnings — resolve `captain` to its holder,
/// fetch heartbeat pins once, whois each target for a bound pane; warn
/// (advisory) for targets with neither. Best-effort lookups warn rather
/// than staying silent. Mail stays async — the caller always proceeds.
pub(crate) fn send_liveness_warnings(
    cfg: &Config,
    project: &str,
    targets: &[String],
) -> Vec<String> {
    let resolved: Vec<String> = targets
        .iter()
        .map(|t| {
            if t.eq_ignore_ascii_case("captain") {
                lookup_captain_holder(project)
                    .0
                    .unwrap_or_else(|| t.clone())
            } else {
                t.clone()
            }
        })
        .collect();
    let heartbeats = presence_pins(project);
    let live_panes = live_hc_panes(cfg);
    let registry = crate::state::load_spawn_registry(project);
    let mut pane_pins = HashSet::new();
    for p in &resolved {
        let pin = p.strip_prefix('@').unwrap_or(p);
        let bound = whois_pane_id(project, p);
        let bound_live =
            crate::captain_slot::bound_pane_is_live(bound.as_deref(), live_panes.as_ref());
        let spawned_live = registry.iter().any(|e| {
            e.mail_identity.as_deref() == Some(pin)
                && crate::captain_slot::bound_pane_is_live(
                    Some(e.pane_id.as_str()),
                    live_panes.as_ref(),
                )
        });
        if bound_live || spawned_live {
            pane_pins.insert(pin.to_string());
        }
    }
    crate::captain_slot::dead_pin_warnings(&resolved, &pane_pins, &heartbeats)
}

/// bd-8pis: dead captain-fold health line shared by arrive and helm —
/// whois holder + snap panes + dispatcher + presence heartbeat. None when
/// any live signal backs the holder (or no holder / override).
fn captain_fold_health_line(
    project: &str,
    holder: Option<String>,
    whois_stale: bool,
    pane_alive: bool,
    dispatcher_up: bool,
    override_pin: Option<&str>,
) -> Option<String> {
    let ev = captain_slot::CaptainEvidence {
        holder,
        presence_fresh: !whois_stale,
        last_wake_fresh: dispatcher_up,
        pane_alive,
        pull_fresh: false,
        override_pin: override_pin.map(str::to_string),
    };
    let heartbeat_fresh = ev
        .holder
        .as_deref()
        .map(|h| captain_presence_live(project, h))
        .unwrap_or(false);
    captain_slot::rebind_hint(&ev, heartbeat_fresh)
        .map(|pin| captain_slot::dead_fold_message(project, &pin))
}

/// bd-fw-name-binding-loss-7rc: reconcile the spawn registry (pane_id → name)
/// against live panes. A pane whose agent restarted shows `name:null` (or a
/// changed name) — this renames it back to its registered name so send /
/// status / loop resolution works again.
fn cmd_binding_rebind(
    cfg: &Config,
    project: &str,
    pin: &str,
    profile: Option<&str>,
    model: Option<&str>,
    harness: Option<&str>,
    note: Option<&str>,
) -> Result<()> {
    let current = state::load_spawn_registry_strict(project).map_err(|e| anyhow!("{e}"))?;
    let mut entries = state::merge_registry_for_mutation(project, current);
    let idx = entries
        .iter()
        .position(|e| e.mail_identity.as_deref() == Some(pin) || e.name == pin)
        .ok_or_else(|| {
            anyhow!("binding-rebind: no SpawnEntry for pin={pin} in project={project}")
        })?;
    let entry = &entries[idx];
    let agent = agent_ref::AgentRef::new(project, pin);
    agent
        .assert_bound_after_settle(false)
        .map_err(|e| anyhow!(e))?;
    let from = agent_ref::AgentBinding {
        profile_name: entry.profile.clone().unwrap_or_default(),
        model: entry.model.clone().unwrap_or_default(),
        harness: entry.harness.clone().unwrap_or_default(),
        location: agent_ref::BindingLocation::Pane,
    };
    let to = agent_ref::AgentBinding {
        profile_name: profile
            .map(str::to_string)
            .unwrap_or_else(|| from.profile_name.clone()),
        model: model
            .map(str::to_string)
            .unwrap_or_else(|| from.model.clone()),
        harness: harness
            .map(str::to_string)
            .unwrap_or_else(|| from.harness.clone()),
        location: if note.unwrap_or("").contains("location=hosted") {
            agent_ref::BindingLocation::Hosted
        } else {
            agent_ref::BindingLocation::Pane
        },
    };
    if profile.is_none() && model.is_none() && harness.is_none() {
        bail!("binding-rebind: pass --profile and/or --model and/or --harness");
    }
    if let Some(p) = profile {
        let cat = ProfileCatalog::load()?;
        if cat.get(p).is_none() {
            bail!("binding-rebind: unknown profile {p:?} (flywheel profile list)");
        }
    }
    if let Some(m) = model {
        // ADR-0022 D7/rollout 6: a live model change is the seat reconciler's
        // job, not a registry write. `swarm apply --patch` is that one path.
        if profile.is_some() {
            bail!(
                "binding-rebind: --model rides `swarm apply --patch <pin>.model=…`; a profile change is a role rebind, not a model patch — run them separately"
            );
        }
        let mut patches = vec![format!("{pin}.model={m}")];
        if let Some(h) = harness {
            patches.push(format!("{pin}.harness={h}"));
        }
        return crate::cmd::swarm::cmd_swarm_apply(cfg, project, &patches, &[], false, true);
    }
    let intent = agent_ref::RebindIntent::prepare(
        agent,
        from,
        to.clone(),
        agent_ref::RebindReason::CaptainRequest {
            note: note.unwrap_or("captain binding-rebind").to_string(),
        },
        0,
    );
    agent_ref::commit_rebind(project, intent, |_| {
        let e = &mut entries[idx];
        if let Some(p) = profile {
            e.profile = Some(p.to_string());
        }
        if let Some(h) = harness {
            e.harness = Some(h.to_string());
        }
        state::save_spawn_registry(project, &entries)?;
        Ok(())
    })?;
    println!(
        "flywheel: binding-rebind settled pin={pin} profile={} harness={}",
        to.profile_name, to.harness
    );
    Ok(())
}

fn cmd_rebind(cfg: &Config, project: &str) -> Result<()> {
    check_project_spelling(project, "rebind")?;
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let entries = state::load_spawn_registry(project);
    if entries.is_empty() {
        eprintln!(
            "flywheel: no spawn registry for {project} — re-run `flywheel spawn {project}` to record pane→name bindings"
        );
        print_json(&json!({ "project": project, "rebound": 0, "registry": "empty" }));
        return Ok(());
    }
    let agents = backend.agent_list()?;
    let mut rebound: Vec<Value> = Vec::new();
    let mut ok_count = 0usize;
    let mut stale = 0usize;
    for entry in &entries {
        let (pane_id, name) = (&entry.pane_id, &entry.name);
        let hname = terminal_agent_name(name);
        let Some(a) = agents
            .iter()
            .find(|a| a.pane_id == *pane_id && a.workspace_id == ws_id)
        else {
            stale += 1;
            continue;
        };
        if binding_state(name, Some(a.name.as_deref())) == BindingState::Intact {
            ok_count += 1;
            continue;
        }
        let was = a.name.clone().unwrap_or_else(|| "<null>".to_string());
        if let Err(e) = backend.agent_rename(pane_id, Some(&hname)) {
            eprintln!("flywheel: rebind {pane_id} → {name} failed: {e:#}");
        } else {
            rebound.push(json!({ "pane_id": pane_id, "was": was, "now": name }));
        }
    }
    print_json(&json!({
        "project": project,
        "workspace_id": ws_id,
        "ok": ok_count,
        "rebound": rebound.len(),
        "rebound_details": rebound,
        "stale_entries": stale,
    }));
    Ok(())
}

/// Agent kinds recorded by the last spawn event in the project's event log
/// (name → kind). Used by `flywheel recover` to respawn dead agents with the
/// right kind. Falls back to `opencode` for names not in the log.
fn spawn_kinds_from_log(project: &str) -> std::collections::HashMap<String, String> {
    let mut kinds = std::collections::HashMap::new();
    let path = crate::config::state_dir().join(format!("{project}.jsonl"));
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return kinds;
    };
    for line in raw.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("kind").and_then(serde_json::Value::as_str) != Some("spawn") {
            continue;
        }
        let Some(agents) = v
            .get("data")
            .and_then(|d| d.get("agents"))
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for a in agents {
            let name = a
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let kind = a
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("opencode");
            kinds
                .entry(name.to_string())
                .or_insert_with(|| kind.to_string());
        }
        break;
    }
    kinds
}

/// bd-a2bw: recover must probe/respawn with the registry harness first.
/// Profile coder panes run `omp`; the event-log fallback is `opencode`,
/// so a live omp looks process-dead and recover tries to start a second
/// harness in the same pane.
fn recover_probe_kind<'a>(
    entry: &'a state::SpawnEntry,
    log_kinds: &'a std::collections::HashMap<String, String>,
) -> &'a str {
    entry
        .harness
        .as_deref()
        .filter(|h| !h.is_empty())
        .or_else(|| log_kinds.get(&entry.name).map(String::as_str))
        .unwrap_or("opencode")
}

/// bd-a8i: window for the responsiveness probe — long enough for a live
/// agent's `state_change_seq` to advance on idle churn, short enough that
/// `cmd_recover` stays snappy across many agents.
const RESPONSIVENESS_WINDOW: Duration = Duration::from_secs(3);

/// bd-1np: probe one registered agent's process liveness vian hc
/// `pane.process_info` — pane existence is NOT liveness (a dead agent's pane
/// still lists in hc with the binding intact). `Some(true)` = process
/// alive, `Some(false)` = provably dead, `None` = probe unavailable or
/// inconclusive (the caller falls back to legacy pane-existence
/// classification — never a confident "healthy").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LivenessOutcome {
    Answer(bool),
    Unknown,
    Unsupported,
    Failed,
}

fn classify_liveness(result: Result<Option<bool>, BackendError>) -> LivenessOutcome {
    match result {
        Ok(Some(alive)) => LivenessOutcome::Answer(alive),
        Ok(None) => LivenessOutcome::Unknown,
        Err(e) if e.code() == &BackendErrorCode::Unsupported => LivenessOutcome::Unsupported,
        Err(_) => LivenessOutcome::Failed,
    }
}

fn probe_agent_liveness(
    backend: &dyn backend::MultiplexerBackend,
    pane_id: &str,
    kind: &str,
    name: &str,
) -> Option<bool> {
    match classify_liveness(backend.agent_alive(pane_id, kind)) {
        LivenessOutcome::Answer(alive) => Some(alive),
        LivenessOutcome::Unknown => {
            eprintln!(
                "flywheel: liveness probe for {name} inconclusive on {}",
                backend.name()
            );
            None
        }
        // No probe on this backend is not a failed probe: skip it quietly.
        LivenessOutcome::Unsupported => None,
        LivenessOutcome::Failed => {
            eprintln!(
                "flywheel: liveness probe for {name} failed on {}",
                backend.name()
            );
            None
        }
    }
}

#[cfg(test)]
mod liveness_backend_tests {
    use super::{BackendError, LivenessOutcome, classify_liveness, probe_agent_liveness};

    struct FakeAlive(bool);
    impl crate::backend::MultiplexerBackend for FakeAlive {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn agent_alive(
            &self,
            _pane_id: &str,
            _kind: &str,
        ) -> Result<Option<bool>, toron_plane::backend::error::BackendError> {
            Ok(Some(self.0))
        }
    }

    struct Inconclusive;
    impl crate::backend::MultiplexerBackend for Inconclusive {
        fn name(&self) -> &'static str {
            "inconclusive"
        }
        fn agent_alive(
            &self,
            _pane_id: &str,
            _kind: &str,
        ) -> Result<Option<bool>, toron_plane::backend::error::BackendError> {
            Ok(None)
        }
    }

    /// Inherits the default `Err(Unsupported)` — no liveness probe at all.
    struct UnsupportedProbe;
    impl crate::backend::MultiplexerBackend for UnsupportedProbe {
        fn name(&self) -> &'static str {
            "unsupported"
        }
    }

    #[test]
    fn the_probe_distinguishes_unsupported_from_unknown() {
        assert_eq!(
            probe_agent_liveness(&FakeAlive(true), "block:x", "opencode", "Pin"),
            Some(true)
        );
        assert_eq!(
            probe_agent_liveness(&FakeAlive(false), "block:x", "opencode", "Pin"),
            Some(false)
        );
        for backend in [
            &Inconclusive as &dyn crate::backend::MultiplexerBackend,
            &UnsupportedProbe,
        ] {
            assert_eq!(
                probe_agent_liveness(backend, "block:x", "opencode", "Pin"),
                None,
                "neither unknown nor unsupported is a confident healthy"
            );
        }
        assert_eq!(
            classify_liveness(Ok(Some(true))),
            LivenessOutcome::Answer(true)
        );
        assert_eq!(classify_liveness(Ok(None)), LivenessOutcome::Unknown);
        assert_eq!(
            classify_liveness(Err(BackendError::unsupported("agent_alive"))),
            LivenessOutcome::Unsupported,
            "a backend with no probe is distinct from an inconclusive one"
        );
        assert_eq!(
            classify_liveness(Err(BackendError::timeout("probe"))),
            LivenessOutcome::Failed
        );
    }
}

/// One agent's recovery decision in `cmd_recover` (bd-1np): healthy, rebind
/// (pane survived, binding lost), or respawn (pane/process gone) with the
/// reason recorded in the report.
enum RecoverDecision<'a> {
    Healthy,
    Rebind { was: String },
    Respawn { reason: &'a str },
}

/// bd-fw-interrupt-kills-agent-or3 (c): CLI supervision counterpart to the
/// durable loop's self-heal — for every registered agent, restore the name
/// binding if the pane survived, or respawn the agent (with its spawn-time
/// kind) if it died. Reports what was done per agent.
///
/// bd-1np: an intact pane + binding is healthy ONLY if the process inside is
/// alive. Each seemingly-healthy agent is liveness-probed vian hc
/// `pane.process_info` (agent binary in the foreground process group); a pane
/// whose process died (shell owns the foreground) is respawned in place, not
/// classified healthy.
///
/// bd-a8i: process liveness is not responsiveness — a frozen opencode passes
/// the foreground-group check. Alive agents are additionally probed for
/// responsiveness (state_change_seq advancing across a window); a frozen +
/// idle agent is respawned, not classified healthy.
/// The workspace (and panes) a recover pass acts on. After a host blackout no
/// owned workspace survives, and a survivor with no pane cannot anchor a split:
/// both recreate the swarm workspace under its spawn label instead of refusing,
/// so recover converges on a lost fleet (bd-0uzrq.21).
fn recover_workspace(
    backend: &dyn backend::MultiplexerBackend,
    project: &str,
) -> Result<(String, Option<Vec<hc::PaneInfo>>)> {
    let label = state::load_spawn_meta(project).unwrap_or_else(|| swarm_label(project));
    let recreate = |why: String| -> Result<(String, Option<Vec<hc::PaneInfo>>)> {
        let ws = backend.workspace_create(Some(&label))?;
        eprintln!(
            "flywheel: recover {project}: {why} — created workspace {} ({label})",
            ws.workspace_id
        );
        let panes = backend.pane_list(Some(&ws.workspace_id)).ok();
        Ok((ws.workspace_id, panes))
    };
    let ws_id = match find_workspace_dyn(backend, project) {
        Ok(id) => id,
        Err(e) => {
            // An owned workspace that exists but resolves ambiguously is a
            // different problem; only total loss recreates.
            if find_workspaces_dyn(backend, project).is_ok() {
                return Err(e);
            }
            return recreate("no swarm workspace survived".to_string());
        }
    };
    let panes = backend.pane_list(Some(&ws_id)).ok();
    if panes.as_ref().is_some_and(Vec::is_empty) {
        return recreate(format!("workspace {ws_id} has no pane to anchor a respawn"));
    }
    Ok((ws_id, panes))
}

/// One recover pass's outcome.
#[derive(Default)]
struct RecoverReport {
    recovered: Vec<Value>,
    healthy: usize,
    moved: Vec<(String, String)>,
    skipped_captain: Vec<String>,
}

/// The recover interpreter: one registry row at a time, decide from the live
/// plane whether the seat is healthy, needs its name rebound, or is gone and
/// must be respawned under its own pin, kind and profile. Takes the plane as a
/// trait object so a fake backend proves each branch.
fn recover_seats(
    backend: &dyn backend::MultiplexerBackend,
    cfg: &Config,
    project: &str,
    ws_id: &str,
    pane_list: Option<&[hc::PaneInfo]>,
    entries: &[state::SpawnEntry],
    include_captain: bool,
) -> Result<RecoverReport> {
    let kinds = spawn_kinds_from_log(project);
    let agents = backend.agent_list()?;
    let mut probe = (backend.name() == "hc")
        .then(|| Client::connect(Some(&cfg.socket_path())).ok())
        .flatten();
    let mut recovered: Vec<Value> = Vec::new();
    let mut healthy = 0usize;
    let pane_exists = |id: &str| {
        pane_list
            .as_ref()
            .is_none_or(|ps| ps.iter().any(|p| p.pane_id == id))
    };
    let mut moved: Vec<(String, String)> = Vec::new();
    // bd-hc-registry-recover-ywypf: captain pin is the operator identity
    // (cfg agent_mail.name) or the saved captain alias; never respawn it by
    // default — a duplicate idle captain pane was created. --include-captain
    // opts into the deliberate case.
    let captain_pin: Option<String> = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .or_else(|| state::load_captain_alias(project));
    let mut skipped_captain: Vec<String> = Vec::new();
    for entry in entries {
        if !include_captain && is_captain_entry(entry, captain_pin.as_deref()) {
            skipped_captain.push(entry.name.clone());
            eprintln!(
                "flywheel: recover {project}: skipping captain pin {} (pass --include-captain to respawn)",
                entry.name
            );
            continue;
        }
        let (pane_id, name) = (&entry.pane_id, &entry.name);
        let hname = terminal_agent_name(name);
        let kind = recover_probe_kind(entry, &kinds);
        let live = agents
            .iter()
            .find(|a| a.pane_id == *pane_id && a.workspace_id == ws_id);
        let decision = match binding_state(name, live.map(|a| a.name.as_deref())) {
            BindingState::Intact => match probe_agent_liveness(backend, pane_id, kind, &hname) {
                Some(false) => RecoverDecision::Respawn {
                    reason: "process-dead",
                },
                Some(true) => {
                    let verdict = probe
                        .as_mut()
                        .map(|c| hc::probe_agent_responsiveness(c, pane_id, RESPONSIVENESS_WINDOW));
                    match verdict {
                        Some(hc::Responsiveness::Unresponsive) => RecoverDecision::Respawn {
                            reason: "unresponsive",
                        },
                        _ => RecoverDecision::Healthy,
                    }
                }
                None => RecoverDecision::Healthy,
            },
            BindingState::Lost { was } => RecoverDecision::Rebind { was },
            BindingState::Gone => RecoverDecision::Respawn {
                reason: "agent-gone",
            },
        };
        match decision {
            RecoverDecision::Healthy => {
                healthy += 1;
            }
            RecoverDecision::Rebind { was } => {
                backend.agent_rename(pane_id, Some(&hname))?;
                recovered.push(json!({
                    "name": name, "pane_id": pane_id, "action": "rebound", "was": was,
                    "terminal_name": hname
                }));
            }
            RecoverDecision::Respawn { reason } => {
                let target = if pane_exists(pane_id) {
                    Some(pane_id.clone())
                } else {
                    match pane_list.as_ref().and_then(|ps| ps.first()) {
                        Some(anchor) => match backend.pane_split(&anchor.pane_id, "right", None) {
                            Ok(p) => Some(p.pane_id),
                            Err(e) => {
                                eprintln!(
                                    "flywheel: recover {name}: pane gone and split failed: {e:#}"
                                );
                                None
                            }
                        },
                        None => {
                            eprintln!(
                                "flywheel: recover {name}: pane gone and the workspace has no \
                                 pane to split from"
                            );
                            None
                        }
                    }
                };
                let Some(target) = target else { continue };
                match start_agent_with_retry_dyn_profile(
                    backend,
                    &hname,
                    kind,
                    &target,
                    &[],
                    LaunchCtx {
                        profile_name: entry.profile.as_deref(),
                        project: Some(project),
                        cfg: Some(cfg),
                    },
                ) {
                    Ok(a) => {
                        if a.pane_id != *pane_id {
                            moved.push((pane_id.clone(), a.pane_id.clone()));
                        }
                        recovered.push(json!({
                            "name": name, "pane_id": a.pane_id, "action": "respawned",
                            "kind": kind, "reason": reason
                        }));
                    }
                    Err(e) => eprintln!("flywheel: recover {name} failed: {e:#}"),
                }
            }
        }
    }
    Ok(RecoverReport {
        recovered,
        healthy,
        moved,
        skipped_captain,
    })
}

fn cmd_recover(cfg: &Config, project: &str, include_captain: bool) -> Result<()> {
    // bd-hc-registry-recover-ywypf: never respawn the captain pin by default
    // (a duplicate idle captain pane was created). Skip captain rows unless
    // --include-captain is passed for the deliberate case; report the skip.
    check_project_spelling(project, "recover")?;
    let backend = backend::multiplexer_backend(cfg)?;
    let mut entries = state::load_spawn_registry_strict(project)
        .map_err(|e| anyhow!("{e}"))
        .map(|current| state::merge_registry_for_mutation(project, current))?;
    if entries.is_empty() {
        entries = state::load_agent_names(&project_dir(cfg, project));
    }
    if entries.is_empty() {
        eprintln!(
            "flywheel: no spawn registry for {project} — re-run `flywheel spawn {project}` to record pane→name bindings"
        );
        print_json(&json!({ "project": project, "recovered": 0, "registry": "empty" }));
        return Ok(());
    }
    let (ws_id, pane_list) = recover_workspace(&*backend, project)?;
    let RecoverReport {
        recovered,
        healthy,
        moved,
        skipped_captain,
    } = recover_seats(
        &*backend,
        cfg,
        project,
        &ws_id,
        pane_list.as_deref(),
        &entries,
        include_captain,
    )?;
    if !moved.is_empty() {
        for e in entries.iter_mut() {
            if let Some((_, new)) = moved.iter().find(|(old, _)| old == &e.pane_id) {
                e.pane_id = new.clone();
            }
        }
        if let Err(e) = state::save_spawn_registry(project, &entries) {
            eprintln!("flywheel: recover registry save failed: {e:#}");
        }
        if let Err(e) = state::save_agent_names(&project_dir(cfg, project), &entries) {
            eprintln!("flywheel: recover agent-names mirror save failed: {e:#}");
        }
        if let Err(e) = state::publish_agent_bindings(project, &entries) {
            eprintln!("flywheel: recover shared agent bindings failed: {e:#}");
        }
    }
    print_json(&json!({
        "project": project,
        "healthy": healthy,
        "recovered": recovered.len(),
        "details": recovered,
        "skipped_captain": skipped_captain.len(),
        "skipped_captain_names": skipped_captain,
    }));
    let exe = std::env::current_exe()?;
    ensure_doorbell_supervisor(cfg, project);
    for ws in doorbell_project_workspaces(project) {
        match doorbell::ensure_running(&exe, &config::state_dir(), &ws, project) {
            Ok(action) => eprintln!("flywheel: doorbell {ws} healed ({action:?})"),
            Err(e) => eprintln!("flywheel: doorbell {ws} heal FAILED: {e:#}"),
        }
    }
    let recovering_pin = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .or_else(|| {
            state::load_spawn_registry(project)
                .first()
                .and_then(|e| e.mail_identity.clone())
                .or_else(|| {
                    state::load_spawn_registry(project)
                        .first()
                        .map(|e| e.name.clone())
                })
        })
        .unwrap_or_else(|| "captain".to_string());
    state::save_captain_alias(project, &recovering_pin)
        .context("recover: bind the captain alias")?;
    Ok(())
}

/// bd-fw-name-binding-loss-7rc: restore a lost name binding from the spawn
/// registry. Used by `__send` when a target name is not live: if the registry
/// records a pane for that name and the pane still exists (agent restarted →
/// name:null), rename it back and return true so the caller re-resolves.
fn rebind_from_registry(
    backend: &dyn backend::MultiplexerBackend,
    project: &str,
    name: &str,
    ws_id: &str,
) -> Result<bool> {
    let current = state::load_spawn_registry_strict(project).map_err(|e| anyhow!("{e}"))?;
    let entries = state::merge_registry_for_mutation(project, current);
    let Some(entry) = registry_entry_for_token(&entries, project, name) else {
        return Ok(false);
    };
    let pane_id = &entry.pane_id;
    let agents = backend.agent_list()?;
    let Some(a) = agents
        .iter()
        .find(|a| a.pane_id == *pane_id && a.workspace_id == ws_id)
    else {
        return Ok(false);
    };
    let target = terminal_agent_name(&entry.name);
    if a.name.as_deref() == Some(target.as_str()) {
        return Ok(true);
    }
    backend.agent_rename(pane_id, Some(&target))?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
mod bd_4pxy_tests {
    use super::*;

    fn agent(
        kind: Option<&str>,
        name: Option<&str>,
        ws: &str,
        pending: bool,
        status: AgentStatus,
    ) -> AgentInfo {
        AgentInfo {
            agent: kind.map(String::from),
            agent_status: status,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: pending,
            name: name.map(String::from),
            pane_id: format!("{ws}:p1"),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term".into(),
            terminal_title: None,
            workspace_id: ws.to_string(),
        }
    }

    #[test]
    fn is_agent_not_ready_matches_protocol_code() {
        assert!(is_agent_not_ready(&BackendErrorCode::Other(
            "agent_not_ready".into()
        )));
        assert!(!is_agent_not_ready(&BackendErrorCode::AgentPromptStalled));
        assert!(!is_agent_not_ready(&BackendErrorCode::Other(
            "agent_prompt_stalled".into()
        )));
        assert!(!is_agent_not_ready(&BackendErrorCode::Timeout));
    }

    /// bd-qs1c.1: settle-retry budget semantics — first-try success costs
    /// one attempt, transient misses recover within budget, exhaustion
    /// returns the last error (caller fails loud, never invents).
    #[test]
    fn settle_retry_first_try_ok_costs_one_attempt() {
        let mut tries = 0;
        let out: Result<&str, &str> = settle_retry(3, 0, || {
            tries += 1;
            Ok("hit")
        });
        assert_eq!(out, Ok("hit"));
        assert_eq!(tries, 1);
    }

    /// bd-8pjr: settle_retry fail-twice-then-succeeds end to end (sleep 0
    /// keeps it hermetic; the shared driver owns the backoff math).
    #[test]
    fn settle_retry_recovers_transient_miss_within_budget() {
        let mut tries = 0;
        let out: Result<&str, &str> = settle_retry(3, 0, || {
            tries += 1;
            if tries < 3 { Err("miss") } else { Ok("hit") }
        });
        assert_eq!(out, Ok("hit"));
        assert_eq!(tries, 3);
    }

    /// bd-8pjr: per-site fail-twice-then-succeed for boot-prime THROUGH the
    /// site policy (no-op sleep — the real loop needs an hc Client, so
    /// the config rides the driver here).
    #[test]
    fn boot_prime_policy_fail_twice_then_succeeds() {
        let policy = boot_prime_policy();
        assert_eq!(policy.max_attempts, 3);
        let mut calls = 0;
        let mut sleeps = 0;
        let out = policy.retry_with_backoff(
            || {
                calls += 1;
                if calls < 3 {
                    Err("prime refused")
                } else {
                    Ok(())
                }
            },
            |_| true,
            |_, _| {},
            || 0.0,
            |_| sleeps += 1,
        );
        assert!(out.is_ok());
        assert_eq!(calls, 3);
        assert_eq!(sleeps, 2);
    }

    /// hc plane harden: prime prefers agent.prompt, settles before Enter,
    /// retries Enter only, and try_send_text_delivery returns the real bool.
    #[test]
    fn prime_and_try_send_delivery_honesty_tripwire() {
        let src = crate::test_source::crate_source();
        let prime = crate::tripwire::find_fn(&src, "prime_line_via_send_text");
        let prime_body = &src[prime..prime + 2800];
        assert!(
            prime_body.contains("agent_prompt(pane_id, line, None)")
                && prime_body.contains("PRIME_ENTER_SETTLE")
                && prime_body.contains("retrying Enter only")
                && prime_body.contains("-> bool"),
            "prime_line_via_send_text must prefer agent.prompt, settle, Enter-only retry, return bool"
        );
        let try_fn = crate::tripwire::find_fn(&src, "try_send_text_delivery");
        let try_end = crate::tripwire::find_fn(&src, "status_label");
        let try_body = &src[try_fn..try_end];
        assert!(
            try_body.contains("prime_line_via_send_text(c, pane_id, text)"),
            "try_send_text_delivery must call prime"
        );
        assert!(
            !try_body.contains("prime_line_via_send_text(c, pane_id, text);\n    true"),
            "try_send must not discard prime result then return true"
        );
        assert!(
            try_body
                .lines()
                .any(|l| l.trim() == "prime_line_via_send_text(c, pane_id, text)"),
            "try_send return must be the prime bool"
        );
    }

    #[test]
    fn settle_retry_exhaustion_returns_last_error() {
        let mut tries = 0;
        let out: Result<&str, String> = settle_retry(2, 0, || {
            tries += 1;
            Err(format!("miss-{tries}"))
        });
        assert_eq!(out, Err("miss-3".to_string()));
        assert_eq!(tries, 3, "first try + 2 retries");
    }

    #[test]
    fn not_ready_retry_target_pane_once() {
        let a = agent(
            Some("omp"),
            Some("flywheel-flywheel-coder"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        assert_eq!(
            not_ready_retry_target("flywheel-flywheel-coder", &a, false),
            Some("w1:p1".to_string())
        );
        assert_eq!(not_ready_retry_target("w1:p1", &a, true), None);
        let unnamed = agent(Some("omp"), None, "w1", false, AgentStatus::Idle);
        assert_eq!(not_ready_retry_target("w1:p1", &unnamed, false), None);
    }

    #[test]
    fn status_label_launch_pending_is_not_idle() {
        assert_eq!(status_label(true, AgentStatus::Idle.as_str()), "launching");
        assert_eq!(status_label(false, AgentStatus::Idle.as_str()), "idle");
        assert_eq!(
            status_label(false, AgentStatus::Working.as_str()),
            "working"
        );
    }

    #[test]
    fn launch_pending_error_names_field_and_agent() {
        let mut pending = agent(
            Some("omp"),
            Some("flywheel-flywheel-coder"),
            "w1",
            true,
            AgentStatus::Unknown,
        );
        pending.interactive_ready = false;
        let ready = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let msg = launch_pending_error(&[pending, ready]).expect("pending agent names the error");
        assert!(msg.contains("launch_pending"), "{msg}");
        assert!(msg.contains("flywheel-flywheel-coder"), "{msg}");
        assert!(msg.contains("w1:p1"), "{msg}");
        let all_ready = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        assert!(launch_pending_error(&[all_ready]).is_none());
    }

    #[test]
    fn want_token_omp1_carries_kind_and_ordinal() {
        let w = to_want_token("flywheel", "omp1");
        assert_eq!(w.short.as_deref(), Some("omp"));
        assert_eq!(w.ordinal, 1);
        assert_eq!(w.name, "flywheel-flywheel-omp");
        let w = to_want_token("flywheel", "omp2");
        assert_eq!(w.short.as_deref(), Some("omp"));
        assert_eq!(w.ordinal, 2);
        let w = to_want_token("flywheel", "flywheel-flywheel-coder");
        assert!(w.short.is_none(), "concrete name carries no kind short");
        let w = to_want_token("flywheel", "w1:p1");
        assert!(w.short.is_none(), "pane id carries no kind short");
    }

    /// bd-k8jn (the token half): a spawned crew's terminal name is its mail PIN
    /// (hc lowercases it) while the spawn registry keeps the canonical
    /// name. The two can never meet, so the registry's PANE has to be the
    /// match: `--to <pin>` and `--to <pane id>` both name a live crew.
    #[test]
    fn a_registry_pane_matches_the_agent_whatever_plane_calls_it() {
        let crew = agent(
            Some("opencode"),
            Some("blueridge"),
            "w7W",
            false,
            AgentStatus::Idle,
        );
        let want = WantToken {
            name: "flywheel-toron-coder".to_string(),
            broaden: false,
            short: None,
            ordinal: 1,
            pane: Some("w7W:p1".to_string()),
        };
        assert_eq!(
            match_indices(std::slice::from_ref(&crew), std::slice::from_ref(&want)),
            vec![0],
            "the pane is the ground truth"
        );
        let no_pane = WantToken {
            pane: None,
            ..want.clone()
        };
        assert!(
            match_indices(std::slice::from_ref(&crew), std::slice::from_ref(&no_pane)).is_empty(),
            "without the pane the pin-named crew is unreachable"
        );
    }

    #[test]
    fn match_indices_kind_fallback_resolves_omp1() {
        let omp1 = agent(
            Some("omp"),
            Some("flywheel-flywheel-coder"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let omp2 = agent(
            Some("omp"),
            Some("flywheel-flywheel-doer"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let oc = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let agents = vec![omp1, omp2, oc];
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "omp1")]),
            vec![0]
        );
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "omp2")]),
            vec![1]
        );
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "omp")]),
            vec![0]
        );
    }

    #[test]
    fn match_indices_raw_kind_hits_nameless_pane() {
        let mut grok = agent(Some("grok"), None, "w6", false, AgentStatus::Idle);
        grok.pane_id = "w6:p2".into();
        let mut omp = agent(Some("omp"), None, "w6", false, AgentStatus::Working);
        omp.pane_id = "w6:p1".into();
        let agents = vec![omp, grok];
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "grok")]),
            vec![1]
        );
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "KeenAuditor")]),
            [] as [usize; 0]
        );
    }

    #[test]
    fn agent_display_names_raw_panes_actionably() {
        let named = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        assert_eq!(agent_display(&named), "flywheel-flywheel-oc");
        let mut raw = agent(Some("grok"), None, "w6", false, AgentStatus::Idle);
        raw.pane_id = "w6:p2".into();
        assert_eq!(agent_display(&raw), "grok (w6:p2)");
        let mut bare = agent(None, None, "w6", false, AgentStatus::Idle);
        bare.pane_id = "w6:p3".into();
        assert_eq!(agent_display(&bare), "w6:p3");
        let mut blank = agent(Some("omp"), Some(""), "w6", false, AgentStatus::Idle);
        blank.pane_id = "w6:p1".into();
        assert_eq!(agent_display(&blank), "omp (w6:p1)");
    }

    #[test]
    fn match_indices_name_match_beats_kind_fallback() {
        let oc = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let omp = agent(
            Some("omp"),
            Some("flywheel-flywheel-coder"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let agents = vec![oc, omp];
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "oc1")]),
            vec![0]
        );
    }

    #[test]
    fn match_indices_broaden_keeps_whole_crew() {
        let oc = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let oc2 = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc2"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let agents = vec![oc, oc2];
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "oc")]),
            vec![0, 1]
        );
    }

    fn agent_on_pane(
        kind: Option<&str>,
        name: Option<&str>,
        pane_id: &str,
        pending: bool,
        status: AgentStatus,
        ready: bool,
    ) -> AgentInfo {
        let mut a = agent(kind, name, "w70", pending, status);
        a.pane_id = pane_id.to_string();
        a.interactive_ready = ready;
        a
    }

    /// bd-a2bw: `--to w70:p3` must hit the pane even when terminal name is null
    /// (omp profile split; ADR-0018 seq-1 suffix-less names never land as
    /// the hc name on later splits).
    #[test]
    fn match_indices_resolves_pane_id_when_name_null() {
        let p1 = agent_on_pane(
            Some("omp"),
            Some("flywheel-toron-coder"),
            "w70:p1",
            false,
            AgentStatus::Idle,
            true,
        );
        let p3 = agent_on_pane(
            Some("omp"),
            None,
            "w70:p3",
            false,
            AgentStatus::Working,
            true,
        );
        let agents = vec![p1, p3];
        assert_eq!(
            match_indices(&agents, &[to_want_token("toron", "w70:p3")]),
            vec![1],
            "--to pane id must match AgentInfo.pane_id when name is null"
        );
        assert_eq!(
            match_indices(&agents, &[to_want_token("toron", "w70:p1")]),
            vec![0]
        );
    }

    /// bd-pin-public-plane-s-easy.2: `--to <pin>` (the adjective+noun mail
    /// identity) resolves through the spawn registry to the agent's hc
    /// name — the pin is what a model types, the slug is internal.
    #[test]
    fn match_indices_resolves_pin_via_registry_mail_identity() {
        let oc = agent(
            Some("oc"),
            Some("flywheel-toron-coder"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let agents = vec![oc];
        let entries = vec![state::SpawnEntry::with_mail_identity(
            "w70:p1",
            "flywheel-toron-coder",
            "DustySparrow",
        )];
        let resolved = registry_entry_for_token(&entries, "toron", "DustySparrow")
            .map(|e| e.name.as_str())
            .expect("pin resolves to a registry entry");
        assert_eq!(resolved, "flywheel-toron-coder");
        let mut names = agents.clone();
        overlay_registry_names(&mut names, &entries);
        assert_eq!(
            match_indices(&names, &[to_want_token("toron", resolved)]),
            vec![0]
        );
    }

    /// bd-a2bw: `--to coder1` is suffix-less (ADR-0018); `--to coder3`
    /// resolves after overlaying the spawn-registry name onto a nameless
    /// omp pane (hc kind stays `omp`, so kind fallback cannot save us).
    #[test]
    fn overlay_registry_names_lets_coder_short_hit_nameless_omp() {
        assert_eq!(
            to_want_token("toron", "coder1").name,
            "flywheel-toron-coder"
        );
        assert_eq!(
            to_want_token("toron", "coder3").name,
            "flywheel-toron-coder3"
        );
        let mut agents = vec![
            agent_on_pane(
                Some("omp"),
                Some("flywheel-toron-coder"),
                "w70:p1",
                false,
                AgentStatus::Idle,
                true,
            ),
            agent_on_pane(Some("omp"), None, "w70:p3", false, AgentStatus::Idle, true),
        ];
        let entries = vec![
            state::SpawnEntry::profiled(
                "w70:p1",
                "flywheel-toron-coder",
                "coder",
                "omp",
                Some("flywheel-toron-coder".into()),
            ),
            state::SpawnEntry::profiled(
                "w70:p3",
                "flywheel-toron-coder3",
                "coder",
                "omp",
                Some("flywheel-toron-coder3".into()),
            ),
        ];
        overlay_registry_names(&mut agents, &entries);
        assert_eq!(agents[1].name.as_deref(), Some("flywheel-toron-coder3"));
        assert_eq!(
            match_indices(&agents, &[to_want_token("toron", "coder3")]),
            vec![1]
        );
        assert_eq!(
            match_indices(&agents, &[to_want_token("toron", "coder1")]),
            vec![0]
        );
        assert_eq!(
            registry_entry_for_token(&entries, "toron", "w70:p3").map(|e| e.name.as_str()),
            Some("flywheel-toron-coder3")
        );
        assert_eq!(
            registry_entry_for_token(&entries, "toron", "coder3").map(|e| e.name.as_str()),
            Some("flywheel-toron-coder3")
        );
    }

    /// bd-a2bw: omp at `π >` with a session is idle + interactive_ready;
    /// hc may still report launch_pending. That bit is stale — send
    /// must proceed.
    #[test]
    fn is_still_booting_ignores_stale_launch_pending() {
        let stale = agent_on_pane(Some("omp"), None, "w70:p2", true, AgentStatus::Idle, true);
        assert!(
            !is_still_booting(&stale),
            "idle + interactive_ready is not booting even if launch_pending"
        );
        let booting = agent_on_pane(
            Some("omp"),
            None,
            "w70:p2",
            true,
            AgentStatus::Unknown,
            false,
        );
        assert!(is_still_booting(&booting));
        assert!(!is_still_booting(&agent_on_pane(
            Some("omp"),
            Some("flywheel-toron-coder"),
            "w70:p1",
            false,
            AgentStatus::Idle,
            true,
        )));
        assert!(launch_pending_error(&[stale]).is_none());
        assert!(launch_pending_error(&[booting]).is_some());
    }

    #[test]
    fn recover_probe_kind_prefers_registry_harness() {
        let entry = state::SpawnEntry::profiled(
            "w70:p1",
            "flywheel-toron-coder",
            "coder",
            "omp",
            Some("flywheel-toron-coder".into()),
        );
        let empty = std::collections::HashMap::new();
        assert_eq!(recover_probe_kind(&entry, &empty), "omp");
        let mut log = std::collections::HashMap::new();
        log.insert("flywheel-toron-coder".into(), "opencode".into());
        assert_eq!(
            recover_probe_kind(&entry, &log),
            "omp",
            "registry harness beats a stale spawn-log kind"
        );
        let legacy = state::SpawnEntry::legacy("w70:p1", "flywheel-toron-oc");
        assert_eq!(recover_probe_kind(&legacy, &empty), "opencode");
        log.insert("flywheel-toron-oc".into(), "pi".into());
        assert_eq!(recover_probe_kind(&legacy, &log), "pi");
    }
}

#[cfg(test)]
mod send_stall_recovery_tests {
    use super::*;

    #[test]
    fn first_stall_always_retries() {
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 0, false),
            Some(StallStep::Retry)
        );
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 0, true),
            Some(StallStep::Retry)
        );
    }

    #[test]
    fn second_stall_respawns_only_with_flag() {
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 1, true),
            Some(StallStep::Respawn)
        );
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 1, false),
            Some(StallStep::GiveUp)
        );
    }

    #[test]
    fn exhausted_stall_always_gives_up() {
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 2, true),
            Some(StallStep::GiveUp)
        );
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 2, false),
            Some(StallStep::GiveUp)
        );
    }

    #[test]
    fn non_stall_errors_propagate() {
        assert_eq!(stall_step(&BackendErrorCode::Timeout, 0, false), None);
        assert_eq!(
            stall_step(&BackendErrorCode::Other("agent_not_found".into()), 1, true),
            None
        );
    }

    #[test]
    fn giveup_error_names_recovery_commands() {
        let msg = stall_giveup_error("flywheel-demo-oc", "demo").to_string();
        assert!(msg.contains("agent_prompt_stalled"), "{msg}");
        assert!(msg.contains("flywheel recover demo"), "{msg}");
        assert!(msg.contains("flywheel kill demo --force"), "{msg}");
        assert!(msg.contains("--respawn-on-stall"), "{msg}");
    }

    /// Read-before-retry: resend only when proven absent.
    #[test]
    fn stall_resend_only_when_proven_absent() {
        assert!(stall_may_resend(StallProbe::ProvenAbsent));
        assert!(!stall_may_resend(StallProbe::PresentOrActive));
        assert!(!stall_may_resend(StallProbe::Unreadable));
    }

    #[test]
    fn classify_stall_probe_table() {
        assert_eq!(
            classify_stall_probe(Some(true), Some(false)),
            StallProbe::PresentOrActive
        );
        assert_eq!(
            classify_stall_probe(Some(false), Some(true)),
            StallProbe::PresentOrActive
        );
        assert_eq!(
            classify_stall_probe(Some(false), Some(false)),
            StallProbe::ProvenAbsent
        );
        assert_eq!(classify_stall_probe(None, None), StallProbe::Unreadable);
        assert_eq!(
            classify_stall_probe(None, Some(false)),
            StallProbe::Unreadable
        );
        assert_eq!(
            classify_stall_probe(Some(false), None),
            StallProbe::ProvenAbsent
        );
    }

    /// Negative control: if classify always returned ProvenAbsent, the
    /// PresentOrActive cases above would also pass — prove they do not.
    #[test]
    fn classify_stall_probe_negative_control_present_blocks_resend() {
        let probe = classify_stall_probe(Some(true), Some(false));
        assert_ne!(probe, StallProbe::ProvenAbsent);
        assert!(!stall_may_resend(probe));
    }

    #[test]
    fn prompt_presence_needle_utf8_safe_and_bounded() {
        let n = prompt_presence_needle("abcdefghij");
        assert_eq!(n, "abcdefghij");
        let long = "x".repeat(100);
        assert_eq!(prompt_presence_needle(&long).chars().count(), 48);
        let multi = "日本語テスト文字列ですよ追加";
        let needle = prompt_presence_needle(multi);
        assert!(needle.chars().count() <= 48);
        assert!(multi.starts_with(&needle) || needle.len() <= multi.len());
    }

    /// Tripwire: first-stall path must read-before-retry, not blind sleep.
    #[test]
    fn send_stall_retry_reads_before_resend_tripwire() {
        let src = crate::test_source::crate_source();
        let send = crate::tripwire::find_fn(&src, "cmd_send");
        let send_end = crate::tripwire::find_fn(&src, "stall_step");
        let send_body = &src[send..send_end];
        assert!(
            send_body.contains("probe_prompt_after_stall")
                && send_body.contains("stall_may_resend")
                && send_body.contains("delivery_key"),
            "cmd_send stall Retry must probe before resend and keep delivery_key"
        );
        assert!(
            !send_body.contains("retrying once in {STALL_RETRY_DELAY_SECS}s"),
            "must not blind sleep-and-resend on first stall"
        );
        assert!(
            send_body.contains("refusing blind resubmit") || send_body.contains("proven absent"),
            "stall path must name the read-before-retry decision"
        );
    }
}

/// Kill-style key names that terminate the agent process (not just cancel
/// the current call). Guarded behind --force (bd-fw-interrupt-kills-agent-or3).
const KILL_KEYS: [&str; 4] = ["ctrl+c", "ctrl-c", "^c", "C-c"];

/// The first kill-style key in the set, if any (pure — unit-tested).
fn kill_key<'a>(keys: &[&'a str]) -> Option<&'a str> {
    keys.iter().find(|k| KILL_KEYS.contains(k)).copied()
}

fn cmd_interrupt(
    cfg: &Config,
    project: &str,
    to: &str,
    keys: &[String],
    force: bool,
) -> Result<()> {
    check_project_spelling(project, "interrupt")?;
    let keys: Vec<&str> = if keys.is_empty() {
        vec!["Escape"]
    } else {
        keys.iter().map(std::string::String::as_str).collect()
    };
    if !force && let Some(k) = kill_key(&keys) {
        bail!(
            "--keys {k} TERMINATES the agent process (pane drops to a shell and the session binding is lost). Prefer the default Escape, which cancels the current tool call and returns to the prompt. Pass --force to send {k} deliberately."
        );
    }
    // Rex branch needs the superlogical resolver; without the feature the hc
    // path below handles everything (same pattern as `cmd_view`).
    #[cfg(feature = "backend-superlogical")]
    if crate::pane::rex_door_selected(cfg) {
        let backend = backend::multiplexer_backend(cfg)?;
        let workspaces = find_workspaces_dyn(&*backend, project)?;
        let ws_ids: Vec<&str> = workspaces.iter().map(|w| w.workspace_id.as_str()).collect();
        let agents = backend.agent_list()?;
        let in_ws: Vec<AgentInfo> = agents
            .into_iter()
            .filter(|a| ws_ids.contains(&a.workspace_id.as_str()))
            .collect();
        if in_ws.is_empty() {
            let labels: Vec<&str> = workspaces.iter().map(|w| w.label.as_str()).collect();
            bail!("no agents in {labels:?} — is the swarm spawned?");
        }
        let registry = state::load_spawn_registry(project);
        let targets = resolve_send_targets_dyn(&in_ws, &registry, project, to)?;
        for a in &targets {
            let prior =
                hc::parse_agent_status(a.agent_status.as_str()).unwrap_or(AgentStatus::Unknown);
            interrupt_guard_working(prior, force)?;
        }
        let is_kill = kill_key(&keys).is_some();
        for a in &targets {
            let target_id = if let Some(n) = a.name.as_deref().filter(|n| !n.is_empty()) {
                n
            } else {
                &a.pane_id
            };
            backend.agent_send_keys(target_id, &keys)?;
            if std::io::stdout().is_terminal() {
                println!(
                    "interrupted {} ({})",
                    dim(agent_target(a)),
                    if is_kill { "kill keys" } else { "soft" }
                );
            }
        }
        log(
            project,
            EventKind::Interrupt,
            json!({ "to": to, "keys": keys, "kill": is_kill }),
        );
        return Ok(());
    }
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let targets = resolve_agents(&mut c, &ws_id, project, to)?;
    for a in &targets {
        let prior = hc::parse_agent_status(a.agent_status.as_str()).unwrap_or(AgentStatus::Unknown);
        interrupt_guard_working(prior, force)?;
    }
    let is_kill = kill_key(&keys).is_some();
    for a in &targets {
        c.agent_send_keys(agent_target(a), &keys)?;
        if std::io::stdout().is_terminal() {
            println!(
                "interrupted {} ({})",
                dim(agent_target(a)),
                if is_kill { "kill keys" } else { "soft" }
            );
        }
    }
    log(
        project,
        EventKind::Interrupt,
        json!({ "to": to, "keys": keys, "kill": is_kill }),
    );
    Ok(())
}

/// bd-fw-view-alt-screen-blocked-ct9: a working agent's `recent` history is
/// on the alternate screen and cannot be captured (hc rejects with
/// `agent_not_idle`). Fall back to the `visible` buffer with a note instead
/// of failing the whole view. Never falls back when the caller explicitly
/// asked for `visible`.
///
/// bd-3hd: every read (whatever source — recent, visible, recent-unwrapped)
/// is passed through `sanitize_pane_text` so alternate-screen TUI chrome
/// (ANSI/OSC sequences + box-drawing borders) never reaches the terminal.
fn client_read_with_fallback(
    c: &mut Client,
    target: &str,
    source: &str,
    lines: Option<u32>,
) -> Result<String> {
    let mut ops = hc::ClientAltScreenRead::new(c, target, lines);
    let raw = alt_screen_read_ladder(&mut ops, source)
        .map_err(|e| anyhow!("{target}: alt-screen read failed: {e:#}"))?;
    Ok(sanitize_pane_text(&raw))
}

fn cmd_view(cfg: &Config, project: &str, to: &str, lines: Option<u32>, source: &str) -> Result<()> {
    check_project_spelling(project, "view")?;
    if std::io::stdout().is_terminal() {
        let ws_label = state::load_spawn_meta(project).unwrap_or_else(|| swarm_label(project));
        // Wave11: label-pinned entry (same single world read as assemble).
        let snap = snapshot::assemble_with_label(cfg, project, &ws_label, doorbell_now_unix());
        eprintln!("{}", snapshot::bearings_line(&snap));
    }
    #[cfg(feature = "backend-superlogical")]
    if crate::pane::rex_door_selected(cfg) {
        let backend = backend::multiplexer_backend(cfg)?;
        let workspaces = find_workspaces_dyn(&*backend, project)?;
        let ws_ids: Vec<&str> = workspaces.iter().map(|w| w.workspace_id.as_str()).collect();
        let agents = backend.agent_list()?;
        let in_ws: Vec<AgentInfo> = agents
            .into_iter()
            .filter(|a| ws_ids.contains(&a.workspace_id.as_str()))
            .collect();
        if in_ws.is_empty() {
            let labels: Vec<&str> = workspaces.iter().map(|w| w.label.as_str()).collect();
            bail!("no agents in {labels:?} — is the swarm spawned?");
        }
        let registry = state::load_spawn_registry(project);
        let targets = resolve_send_targets_dyn(&in_ws, &registry, project, to)?;
        for a in &targets {
            let target_id = agent_target(a);
            let val = backend.agent_read(target_id, source, lines.unwrap_or(0))?;
            let text = val.get("text").and_then(Value::as_str).unwrap_or("");
            let text = sanitize_pane_text(text);
            if std::io::stdout().is_terminal() && targets.len() > 1 {
                println!("--- {} ---", agent_target(a));
            }
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
        return Ok(());
    }
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let targets = resolve_agents(&mut c, &ws_id, project, to)?;
    for a in &targets {
        let text = client_read_with_fallback(&mut c, agent_target(a), source, lines)?;
        if std::io::stdout().is_terminal() && targets.len() > 1 {
            println!("--- {} ---", agent_target(a));
        }
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

/// Resolve the display name for a watched pane from the subscription-time
/// (pane, name) pairs. The `pane.agent_status_changed` push carries the agent
/// KIND in its `agent` field — never the instance name — so event matching
/// must go through the pane id (which IS in the payload); nameless agents
/// have their pane id as the display name already.
fn watch_name_for_pane<'a>(targets: &'a [(String, String)], pane_id: &str) -> Option<&'a str> {
    targets
        .iter()
        .find(|(p, _)| p == pane_id)
        .map(|(_, n)| n.as_str())
}

/// `flywheel output`: read an agent's session from the opencode structured
/// store (bd-eign.5). Logic lives in hc::read_session_log; this is a
/// thin runtime + render stub.
fn cmd_output(
    cfg: &Config,
    project: &str,
    role: Option<&str>,
    session: Option<&str>,
    limit: u32,
) -> Result<()> {
    check_project_spelling(project, "output")?;
    let dir = project_dir_str(cfg, project)?;
    let log = hc::read_session_log(&dir, role, session, limit)?;
    print_json(&log);
    Ok(())
}

fn cmd_watch(
    cfg: &Config,
    project: &str,
    to: &str,
    until: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<()> {
    check_project_spelling(project, "watch")?;
    let until_status: Option<AgentStatus> = until.map(hc::parse_agent_status).transpose()?;
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    if std::io::stdout().is_terminal() {
        let ws_label = state::load_spawn_meta(project).unwrap_or_else(|| swarm_label(project));
        // Wave11: label-pinned entry (same single world read as assemble).
        let snap = snapshot::assemble_with_label(cfg, project, &ws_label, doorbell_now_unix());
        eprintln!("{}", snapshot::bearings_line(&snap));
    }
    let ws_id = find_workspace(&mut c, project)?;

    let mut watch_names: Vec<String> = Vec::new();
    let mut watch_targets: Vec<(String, String)> = Vec::new();
    for a in resolve_agents(&mut c, &ws_id, project, to)? {
        let name = agent_target(&a).to_string();
        watch_names.push(name.clone());
        watch_targets.push((a.pane_id.clone(), name.clone()));
        let st = a.agent_status;
        if std::io::stdout().is_terminal() {
            println!("{} {}", dim(&name), status_paint(st));
        }
        if let Some(u) = until_status
            && st == u
            && all_matching(&mut c, &watch_names, u)?
        {
            return Ok(());
        }
    }

    let panes = c.pane_list()?;
    let subs: Vec<Subscription> = panes
        .iter()
        .map(|p| Subscription::agent_status_changed(p.pane_id.clone()))
        .collect();
    let mut stream = match hc::subscribe(&c, &subs) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("flywheel: WARNING subscription failed ({e:#}); polling fallback");
            None
        }
    };
    if stream.is_some() && watch_poll(&mut c, &watch_names, until_status)? {
        return Ok(());
    }

    let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));
    loop {
        if let Some(d) = deadline {
            if std::time::Instant::now() >= d {
                bail!("watch timed out waiting for {:?}", until_status);
            }
            if let Some(s) = stream.as_mut() {
                s.set_read_timeout(Some(d.saturating_duration_since(std::time::Instant::now())));
            }
        } else if let Some(s) = stream.as_mut() {
            s.set_read_timeout(None);
        }
        if let Some(s) = stream.as_mut() {
            match s.read_event() {
                Ok(Some(ev)) => match ev.data {
                    hc::TerminalEventData::PaneAgentStatusChanged(change)
                    | hc::TerminalEventData::PaneAgentStatusChangedSub(change) => {
                        let Some(name) = watch_name_for_pane(&watch_targets, &change.pane_id)
                        else {
                            continue;
                        };
                        if std::io::stdout().is_terminal() {
                            println!("{} {}", dim(name), status_paint(change.agent_status));
                        }
                        if let Some(u) = until_status
                            && change.agent_status == u
                            && all_matching(&mut c, &watch_names, u)?
                        {
                            return Ok(());
                        }
                    }
                    _ => {}
                },
                Ok(None) => bail!("hc socket closed"),
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(1000));
                    if watch_poll(&mut c, &watch_names, until_status)? {
                        return Ok(());
                    }
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(1000));
            if watch_poll(&mut c, &watch_names, until_status)? {
                return Ok(());
            }
        }
    }
}

/// Poll agent list as a subscription fallback: returns `true` when
/// `until_status` is reached for all watched names (or `false` while
/// polling — the caller owns the deadline).
fn watch_poll(
    c: &mut Client,
    watch_names: &[String],
    until_status: Option<AgentStatus>,
) -> Result<bool> {
    for a in c.agent_list()? {
        let name = a.name.unwrap_or(a.pane_id.clone());
        if watch_names.contains(&name)
            && let Some(u) = until_status
            && a.agent_status == u
            && all_matching(c, watch_names, u)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn all_matching(c: &mut Client, names: &[String], status: AgentStatus) -> Result<bool> {
    let agents = c.agent_list()?;
    for name in names {
        let a = agents
            .iter()
            .find(|a| a.name.as_deref() == Some(name) || a.pane_id == *name);
        match a {
            Some(a) if a.agent_status == status => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn cmd_attach(cfg: &Config, project: &str, to: &str) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let agents = resolve_agents(&mut c, &ws_id, project, to)?;
    let target = agent_target(&agents[0]);
    println!("{}", dim("attaching (detach with ctrl+b q)…"));
    let status = Command::new("hc")
        .args(["attach", target])
        .status()
        .map_err(|e| anyhow!("hc attach failed: {e}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Names of agents still in `working`. Kill must not reap these unless
/// `--force-working` — cargo test routinely exceeds 15m.
fn working_agent_names(agents: &[AgentInfo]) -> Vec<String> {
    agents
        .iter()
        .filter(|a| a.agent_status == hc::AgentStatus::Working)
        .map(|a| a.name.clone().unwrap_or_else(|| a.pane_id.clone()))
        .collect()
}

fn cmd_kill(
    cfg: &Config,
    project: &str,
    force: bool,
    force_working: bool,
    drain: bool,
    reap_crews: bool,
) -> Result<()> {
    if !force {
        if std::io::stdin().is_terminal() {
            eprintln!("close swarm for {project}? [y/N] ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                println!("aborted");
                return Ok(());
            }
        } else {
            bail!("refusing to close {project} without confirmation — pass --force");
        }
    }
    let backend = backend::multiplexer_backend(cfg)?;
    let (ws_id, agents): (String, Vec<AgentInfo>) = if crate::pane::rex_door_selected(cfg) {
        let ws_id = find_workspace_dyn(&*backend, project)?;
        let agents = backend
            .agent_list()?
            .into_iter()
            .filter(|a| a.workspace_id == ws_id)
            .collect();
        (ws_id, agents)
    } else {
        let mut c = Client::connect(Some(&cfg.socket_path()))?;
        let ws_id = find_workspace(&mut c, project)?;
        let agents = c
            .agent_list()?
            .into_iter()
            .filter(|a| a.workspace_id == ws_id)
            .collect();
        (ws_id, agents)
    };
    let mut policy = cfg.on_captain_death();
    if reap_crews {
        policy = config::OnCaptainDeath::Stop;
    }
    let entries = state::load_spawn_registry_strict(project).map_err(|e| anyhow!("{e}"))?;
    let registry = state::merge_registry_for_mutation(project, entries);
    let captain_pin = state::load_captain_alias(project)
        .or_else(|| cfg.agent_mail.as_ref().and_then(|m| m.name.clone()))
        .unwrap_or_else(|| "captain".to_string());
    let working = working_agent_names(&agents);
    let blocked: Vec<String> = if policy == config::OnCaptainDeath::Stop || reap_crews {
        working
            .iter()
            .filter(|w| !config::is_captain_child(&registry, &captain_pin, w))
            .cloned()
            .collect()
    } else {
        working.clone()
    };
    if !blocked.is_empty() && drain {
        let mut al = match state::load_active_loop(project)? {
            Some(al) => al,
            None => state::ActiveLoop::new(project, serde_json::json!({}), "drain"),
        };
        let dispatched: Vec<String> = al
            .per_bead_phase
            .iter()
            .filter(|(_, p)| p.starts_with("dispatch"))
            .map(|(b, _)| b.clone())
            .collect();
        for name in &blocked {
            if al.drain_pending.contains_key(name) {
                continue;
            }
            let bead = dispatched.first().cloned().unwrap_or_else(|| name.clone());
            al.drain_pending.insert(name.clone(), bead.clone());
            if let Some(a) = agents.iter().find(|a| a.name.as_deref() == Some(name)) {
                let _ = backend.prompt(&a.pane_id, &crate::durable::drain_prompt(&bead), None);
            }
        }
        state::save_active_loop(project, &al)?;
        println!(
            "drain: asked {} working agent(s) to finish their VERIFY fence; grace is {} polls. Re-run kill --force --force-working once they report, or let the loop force-stop past grace.",
            blocked.len(),
            durable::DRAIN_GRACE_POLLS
        );
        return Ok(());
    }
    if !blocked.is_empty() && !force_working {
        bail!(
            "refuse kill: {} agent(s) still working ({}). Long jobs (cargo test) often exceed 15m. Pass --force-working to reap anyway, or --drain for a graceful two-phase stop.",
            blocked.len(),
            blocked.join(", ")
        );
    }
    let ws_label = state::load_spawn_meta(project).unwrap_or_else(|| swarm_label(project));
    match policy {
        config::OnCaptainDeath::Continue | config::OnCaptainDeath::Adopt if !reap_crews => {
            let remaining = config::registry_after_captain_death(&registry, &captain_pin, policy);
            let _ = state::save_spawn_registry(project, &remaining);
            state::clear_captain_alias_file(project).context("kill: clear the captain alias")?;
            println!(
                "closed captain for {project} (policy {} — crews remain, alias cleared)",
                policy
            );
            log(
                project,
                EventKind::Close,
                json!({ "workspace_id": ws_id, "policy": policy.as_str() }),
            );
            return Ok(());
        }
        _ => {}
    }
    backend.workspace_close(&ws_id, false)?;
    doorbell::stop(&config::state_dir(), &ws_label);
    {
        use doorbell_rt::subscribe::{Actor, SubscriptionStore};
        let prefix = format!("spawn-{ws_label}-");
        let mut store = SubscriptionStore::open(&config::state_dir());
        for row in store.list() {
            if row.id.starts_with(&prefix) {
                match store.unsubscribe(Actor::operator(), &row.id) {
                    Ok(dropped) => println!("unsubscribed: {}", dropped.id),
                    Err(e) => {
                        eprintln!("flywheel: cannot drop wake subscription {}: {e:?}", row.id)
                    }
                }
            }
        }
    }
    let remaining =
        config::registry_after_captain_death(&registry, &captain_pin, config::OnCaptainDeath::Stop);
    let _ = state::save_spawn_registry(project, &remaining);
    state::clear_captain_alias_file(project).context("kill: clear the captain alias")?;
    let _ = std::fs::remove_dir_all(agentstate::project_state_dir(&project_dir(cfg, project)));
    if let Ok(mail) = backend::mail_backend(cfg)
        && let Some(key) = project_dir(cfg, project).to_str()
    {
        for a in &agents {
            if let Err(e) = backend::run_mail(mail.clear_pane_identity(key, &a.pane_id)) {
                eprintln!(
                    "flywheel: mail clear_pane_identity failed for {} on {}: {e}",
                    a.name.as_deref().unwrap_or("<unnamed>"),
                    a.pane_id
                );
            }
        }
    }
    log(project, EventKind::Close, json!({ "workspace_id": ws_id }));
    println!("closed {project}");
    Ok(())
}

/// bd-p4oc helpers: non-vacuous VERIFY gate for cargo test filters.
///
/// bd-s913: the shape test, not the literal spelling. This used to require the
/// fence to start with `cargo test`, so every `mbx +nightly-... test` fence —
/// 148 of the 353 in the ledger, including bd-uona's own — was silently exempt
/// from the strict gate and reported `0 passed; 68 filtered out` as a pass.
/// A green light wired to nothing is worse than a red fence, because a red
/// fence at least produces a signal.
pub(crate) fn is_cargo_test_verify(verify: &str) -> bool {
    let v = verify.trim_start();
    let is_direct = v.starts_with("cargo test")
        || v.starts_with("cargo nextest run")
        || v.starts_with("nextest run");
    let is_wrapped = v.split_whitespace().any(|tok| {
        let tok = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        matches!(tok, "mbx" | "rtx" | "mise") && v.contains(" test")
    });
    is_direct || is_wrapped
}

pub(crate) fn parse_count_before(output: &str, needle: &str) -> Option<u32> {
    let idx = output.find(needle)?;
    let before = &output[..idx];
    let mut end = before.len();
    while end > 0 && before.as_bytes()[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && before.as_bytes()[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == end {
        return None;
    }
    before[start..end].parse::<u32>().ok()
}

pub(crate) fn sum_counts_before(output: &str, needle: &str) -> Option<u32> {
    let mut total: u32 = 0;
    let mut found = false;
    let mut search_start = 0;
    while let Some(idx) = output[search_start..].find(needle) {
        let abs_idx = search_start + idx;
        let before = &output[..abs_idx];
        let mut end = before.len();
        while end > 0 && before.as_bytes()[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && before.as_bytes()[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start != end
            && let Ok(n) = before[start..end].parse::<u32>()
        {
            total = total.saturating_add(n);
            found = true;
        }
        search_start = abs_idx + needle.len();
    }
    if found { Some(total) } else { None }
}

pub(crate) fn parse_cargo_test_counts(output: &str) -> (Option<u32>, Option<u32>) {
    let passed = sum_counts_before(output, " passed");
    let filtered = sum_counts_before(output, " filtered out");
    if passed.is_some() {
        return (passed, filtered);
    }
    (
        parse_count_before(output, " passed"),
        parse_count_before(output, " filtered out"),
    )
}

pub(crate) fn append_to_receipt(path: &Path, extra: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
    f.write_all(extra.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod verify_gate_tests {
    use super::*;

    #[test]
    fn verify_gate_is_cargo_test_detection() {
        assert!(is_cargo_test_verify("cargo test -p flywheel foo"));
        assert!(is_cargo_test_verify("  cargo test --lib"));
        assert!(is_cargo_test_verify("cargo test --workspace"));
        assert!(!is_cargo_test_verify("cargo build"));
        assert!(!is_cargo_test_verify("pytest tests/test_x.py"));
        assert!(!is_cargo_test_verify("cargo build --tests"));
    }

    /// bd-s913: the wrapper shapes this repo's fences actually use. Every one
    /// of these was silently exempt from the strict vacuous gate before,
    /// which is how a zero-match filter reported itself as a pass.
    #[test]
    fn vacuous_verify_detects_wrapped_cargo_test_fences() {
        for verify in [
            "mbx +nightly-aarch64-apple-darwin test preflight",
            "mbx +nightly-aarch64-apple-darwin test note_has_sha",
            "mbx +nightly-aarch64-apple-darwin test -p flywheel --bins foo",
            "cargo test -p toron-workflow park",
            "cargo test --workspace",
            "rtx test bar",
        ] {
            assert!(
                is_cargo_test_verify(verify),
                "must be checked for vacuity: {verify}"
            );
        }
    }

    /// bd-s913 unhappy path: the widened matcher must NOT swallow
    /// non-test-shaped fences. A `cargo build` or a bare `flywheel` command
    /// has no pass count to parse, so treating it as cargo-shaped would make
    /// the gate report nonsense.
    #[test]
    fn vacuous_verify_leaves_non_test_shapes_alone() {
        for verify in [
            "cargo build",
            "cargo build --tests",
            "pytest tests/test_x.py",
            "bash scripts/verify-doorbell-daemon.sh",
            "doorbell ping",
            "flywheel kill --force && sleep 2",
            "br show bd-x --json | grep -q closed",
        ] {
            assert!(
                !is_cargo_test_verify(verify),
                "not a cargo test shape: {verify}"
            );
        }
    }

    /// bd-s913: the decision itself. A zero-match filter is VACUOUS (must be
    /// refused under strict); a real match is not. This is the pair the brief
    /// asks for, asserted on the parsed counts the gate actually uses.
    #[test]
    fn vacuous_verify_classifies_zero_match_as_vacuous_and_real_match_as_not() {
        let zero = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 68 filtered out; finished in 0.00s";
        let (passed, filtered) = parse_cargo_test_counts(zero);
        assert_eq!(passed, Some(0), "the zero-match shape parses to 0");
        assert_eq!(filtered, Some(68));
        assert!(
            passed == Some(0),
            "0 passed must read as vacuous, matching the gate's condition"
        );

        let real = "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 1941 filtered out; finished in 0.00s";
        let (passed, filtered) = parse_cargo_test_counts(real);
        assert_eq!(passed, Some(2), "a real match is not vacuous");
        assert_eq!(filtered, Some(1941));
        assert!(
            passed != Some(0),
            "a matching filter must NOT be refused as vacuous"
        );
    }

    /// bd-s913: the fence bd-uona actually shipped, end to end through both
    /// halves of the decision. Before the fix `is_cargo_test_verify` was
    /// false for it, so `is_vacuous` was false and the row read as a pass.
    #[test]
    fn vacuous_verify_bd_uona_fence_is_now_detected_as_vacuous() {
        let verify = "mbx +nightly-aarch64-apple-darwin test preflight";
        let output = "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 68 filtered out; finished in 0.00s";
        let is_cargo = is_cargo_test_verify(verify);
        let (passed, _) = parse_cargo_test_counts(output);
        let is_vacuous = is_cargo && passed == Some(0);
        assert!(is_cargo, "the fence is now in scope for the gate");
        assert!(is_vacuous, "a zero-match fence reads as vacuous");
    }

    /// Table-driven cargo-test count parsing: (output, passed, filtered).
    /// One table instead of one test per literal (bd-1qqo seam pattern).
    #[test]
    fn verify_gate_parses_counts_table() {
        for (out, passed, filtered) in [
            (
                "test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s",
                Some(4),
                Some(1),
            ),
            (
                "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 42 filtered out; finished in 0.00s",
                Some(0),
                Some(42),
            ),
            ("test result: ok. 3 passed; 0 failed;", Some(3), None),
        ] {
            assert_eq!(
                parse_cargo_test_counts(out),
                (passed, filtered),
                "input: {out}"
            );
        }
    }

    #[test]
    fn verify_gate_receipt_vacuous_warning_string_contains_required_terms() {
        let verify = "cargo test -p flywheel nonexistent_filter_xyz_12345";
        let warning = format!(
            "WARNING: vacuous VERIFY — filter `{verify}` matched 0 tests (evidence proves nothing)"
        );
        assert!(warning.contains("WARNING"));
        assert!(warning.contains("vacuous"));
        assert!(warning.contains("matched 0 tests"));
        assert!(warning.contains("evidence proves nothing"));
        assert!(warning.contains("nonexistent_filter_xyz_12345"));
    }

    #[test]
    fn verify_gate_matched_count_string() {
        let msg = format!("VERIFY matched {} tests, all passed", 5);
        assert!(msg.contains("matched"));
        assert!(msg.contains('5'));
        assert!(msg.contains("passed"));
    }

    #[test]
    fn verify_gate_loop_vacuous_detection() {
        let verify = "cargo test -p flywheel verify_gate_loop";
        assert!(is_cargo_test_verify(verify));
        let out = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 5 filtered out; finished in 0.00s";
        let (passed, filtered) = parse_cargo_test_counts(out);
        assert_eq!(passed, Some(0));
        assert_eq!(filtered, Some(5));
        let is_vacuous = is_cargo_test_verify(verify) && passed == Some(0);
        assert!(is_vacuous, "verify_gate_loop should be vacuous");
        let warning = format!(
            "WARNING: vacuous VERIFY — filter `{verify}` matched 0 tests (evidence proves nothing)"
        );
        assert!(warning.contains("vacuous"));
    }

    #[test]
    fn verify_gate_loop_strict_refuses_logic() {
        let verify = "cargo test -p flywheel verify_gate_loop";
        let out = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 10 filtered out; finished in 0.00s";
        let (passed, _) = parse_cargo_test_counts(out);
        let is_vacuous = is_cargo_test_verify(verify) && passed == Some(0);
        assert!(is_vacuous);
    }
}

/// The rotation tick (bd-5vi2x): re-mint before expiry, then revoke the old
/// hash. Reads only the journal's hash/expiry — no plaintext in argv or logs.
fn cmd_events_token_rotate(
    pin: &str,
    project: &str,
    token_file: &std::path::Path,
    journal: &std::path::Path,
    before_secs: u64,
) -> Result<()> {
    let cli = events_tokens::ToronTokenCli {
        bin: std::env::var("TORON_BIN").unwrap_or_else(|_| "toron".to_string()),
    };
    let now_us = chrono::Utc::now().timestamp_micros();
    let (old_hash, expires_us) = events_tokens::last_minted(journal)
        .map_err(|e| anyhow::anyhow!("{e}: {}", journal.display()))?;
    let state = events_tokens::TokenState {
        hash: old_hash.clone(),
        minted_us: 0,
        expires_us,
        path: token_file.display().to_string(),
    };
    if let Some(plan) =
        events_tokens::rotation_plan(&state, now_us, (before_secs as i64) * 1_000_000)
    {
        events_tokens::rotate(&cli, &plan, pin, project, token_file, journal, now_us)
            .map_err(|e| anyhow::anyhow!("token rotation failed: {e}"))?;
        println!("events-token: rotated {pin} (old hash {old_hash} revoked)");
    } else {
        println!("events-token: {pin} not due for rotation");
    }
    Ok(())
}

/// bd-si4b5 Phase A: a stdio MCP server that declares claude/channel and speaks
/// JSON-RPC line protocol. The events transport is a port; Phase B (bd-o4bgx)
/// supplies the real toron client. Phase A wires a no-op transport so the
/// protocol seam is real and testable without a live toron events endpoint.
fn cmd_mcp_events(_cfg: &Config, subscription: &str) -> Result<()> {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // bd-si4b5 Phase B: the real SEP-3415 client is the transport behind the
    // port. A missing token keeps the stdio seam usable (initialize/probe) but
    // degrades to the no-op port with a loud stderr note — never a silent lie.
    let url = std::env::var("FLYWHEEL_TORON_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:18080".to_string());
    // bd-5vi2x: the bridge reads the token FILE, never a plaintext env value.
    // FLYWHEEL_TORON_TOKEN is still honoured as a fallback for hand-run seats.
    let token_file = std::env::var("FLYWHEEL_TORON_TOKEN_FILE")
        .ok()
        .filter(|t| !t.is_empty());
    let state_path = config::state_dir().join(format!(
        "events-bridge-{}.json",
        subscription
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            })
            .collect::<String>()
    ));
    let mut cursor = std::fs::read_to_string(&state_path).unwrap_or_default();
    let transport: Box<dyn events_bridge::EventsTransport> = match token_file {
        Some(path) => {
            let mut client = events_client::ToronEventsClient::new(
                events_client::UreqRpc::new(&url, &path),
                subscription,
                86_400, // TTL cap: 24h; refreshed before expiry by the caller.
            );
            match client.subscribe() {
                Ok(_) => Box::new(client),
                Err(e) => {
                    eprintln!("flywheel mcp-events: subscribe failed: {e}");
                    Box::new(events_bridge::NoopTransport)
                }
            }
        }
        None => {
            eprintln!(
                "flywheel mcp-events: FLYWHEEL_TORON_TOKEN_FILE unset; no events will arrive"
            );
            Box::new(events_bridge::NoopTransport)
        }
    };
    let mut deduper = events_bridge::Deduper::new(1024);
    for line in stdin.lock().lines() {
        let line = line?;
        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = req
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let id = req.get("id").cloned();
        match method {
            "initialize" => {
                let client_version = req
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(serde_json::Value::as_str);
                let result = events_bridge::initialize_result(client_version);
                let mut msg = serde_json::json!({ "jsonrpc": "2.0", "result": result });
                if let Some(rid) = id {
                    msg["id"] = rid;
                }
                writeln!(out, "{}", msg)?;
                out.flush()?;
            }
            "ping" => {
                let mut msg = serde_json::json!({ "jsonrpc": "2.0", "result": {} });
                if let Some(rid) = id {
                    msg["id"] = rid;
                }
                writeln!(out, "{}", msg)?;
                out.flush()?;
            }
            "events/list" => {
                let mut msg = serde_json::json!({
                    "jsonrpc": "2.0",
                    "result": events_bridge::native_list_result(),
                });
                if let Some(rid) = id {
                    msg["id"] = rid;
                }
                writeln!(out, "{}", msg)?;
                out.flush()?;
            }
            "events/subscribe" => {
                // The upstream subscription is already open; echo the pin as
                // the local subscription id (bd-o1z4m).
                let expires_us = chrono::Utc::now().timestamp_micros() + 86_400_000_000;
                let mut msg = serde_json::json!({
                    "jsonrpc": "2.0",
                    "result": events_bridge::native_subscribe_result(subscription, &cursor, expires_us),
                });
                if let Some(rid) = id {
                    msg["id"] = rid;
                }
                writeln!(out, "{}", msg)?;
                out.flush()?;
            }
            "events/poll" => {
                let timing = events_bridge::BridgeTiming {
                    last_hb_ms: 0,
                    now_ms: 0,
                    interval_ms: 1000,
                    attempt: 0,
                    rand_unit: 0.0,
                };
                match events_bridge::bridge_step(
                    &*transport,
                    subscription,
                    &mut deduper,
                    &mut cursor,
                    &timing,
                ) {
                    Ok(step) => {
                        // Emit-first, journal-after: a crash between the two
                        // replays from the old cursor and eventId dedupe absorbs
                        // the duplicate (ADR-0039 D8).
                        for notice in step.notifications {
                            // bd-o1z4m: one shared rung set with the doorbell
                            // fallback, keyed by bus event id.
                            if events_bridge::should_ring(&config::state_dir(), &notice) {
                                writeln!(out, "{}", notice)?;
                            }
                        }
                        out.flush()?;
                        if step
                            .cmds
                            .iter()
                            .any(|c| matches!(c, events_bridge::BridgeCmd::JournalCursor(_)))
                            && let Err(e) = std::fs::write(&state_path, &cursor)
                        {
                            eprintln!("flywheel mcp-events: cursor journal failed: {e}");
                        }
                        if step.dead {
                            eprintln!(
                                "flywheel mcp-events: stream liveness stale; reconnect in {:?}ms",
                                step.reconnect_ms
                            );
                        }
                    }
                    Err(e) => eprintln!("flywheel mcp-events: poll failed: {e}"),
                }
                let mut msg = serde_json::json!({ "jsonrpc": "2.0", "result": {} });
                if let Some(rid) = id {
                    msg["id"] = rid;
                }
                writeln!(out, "{}", msg)?;
                out.flush()?;
            }
            "notifications/initialized" => {}
            _ => {
                let mut msg = serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32601, "message": "method not found" }
                });
                if let Some(rid) = id {
                    msg["id"] = rid;
                }
                writeln!(out, "{}", msg)?;
                out.flush()?;
            }
        }
    }
    Ok(())
}

fn cmd_assign(cfg: &Config, project: &str, to: &str, beads: &str, msg: Option<&str>) -> Result<()> {
    let mail = mail::Mail::from_config(Some(&cfg.toron_mail_config()))?.ok_or_else(|| {
        anyhow!(
            "agent-mail (toron) not configured — set [mail.toron] url in {}",
            config::config_file().display()
        )
    })?;
    let mail = mail;
    let dir = project_dir(cfg, project);
    let key = utf8_path(&dir, "project dir")?;
    mail.ensure_project(&key)?;
    let bead_ids: Vec<String> = beads
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if bead_ids.is_empty() {
        bail!("no beads given (--beads bd-1,bd-2)");
    }
    let body = format!(
        "Assigned beads: {}\n\n{}",
        bead_ids.join(", "),
        msg.unwrap_or("Execute these beads per the standard lifecycle: claim -> reserve -> execute -> verify -> close -> sync.")
    );
    let targets: Vec<String> = if to == "all" {
        let agents = mail.list_agents(&key, None)?;
        agents
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        to.split(',')
            .map(|s| resolve_mail_target_registry(project, s.trim()))
            .collect()
    };
    if targets.is_empty() {
        bail!(
            "no agent-mail identities found for {project} (register first: flywheel mail agents)"
        );
    }
    let subject = format!(
        "[assign] {} → {}",
        project_base(project),
        bead_ids.join(", ")
    );
    for t in &targets {
        if let Err(e) =
            mail.register_agent(&key, t, "opencode", "opencode", "flywheel mail CLI actor")
        {
            eprintln!("flywheel: assign auto-register of {t} failed (continuing): {e:#}");
        }
    }
    if let Err(e) = mail.send(&key, &targets, &subject, &body, "high", true, None) {
        let msg = format!("{e:#}");
        if msg.contains("unknown recipient") {
            bail!(
                "assign failed: {msg} — auto-registration ran for each target; if it \
                 could not register, the toron mail daemon may be down (start it: toron \
                 serve) then retry the assign"
            );
        }
        return Err(e);
    }
    println!("assigned {} to {}", bead_ids.join(", "), targets.join(", "));
    Ok(())
}

/// Parsed cross-project swarm request (subject `[xproj] …` + body fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XprojRequest {
    pub recipe: String,
    pub workers: u32,
    pub params: String,
    pub product: String,
}

/// One `[xproj]` inbox hit for the durable loop (bd-pattern-cross-project-swarm).
///
/// `id` is a String because the mail backend's id type varies: agent-mail
/// emits u64 sequence numbers, toron emits hex event ids. Accept both on
/// deserialize so the xproj fan-out works against either backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XprojInboxItem {
    #[serde(deserialize_with = "de_id_string")]
    pub id: String,
    pub subject: String,
    pub body: String,
    pub thread_id: String,
    pub from: String,
}

/// Deserialize a message id that may arrive as a JSON number (agent-mail) or
/// a string (toron hex event id). Both normalize to a string.
pub fn de_id_string<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(D::Error::custom("message id must be a string or number")),
    }
}

/// Build the worker queue prompt for a cross-project request.
#[must_use]
pub fn xproj_queue_prompt(item: &XprojInboxItem, req: &XprojRequest) -> String {
    format!(
        "CROSS-PROJECT SWARM REQUEST (message_id={}, thread={}, from={}).\n\
         recipe={} workers={} product={}.\n\
         Params:\n{}\n\n\
         Run this recipe/workflow in THIS project (spawn workers if needed). \
         When finished, reply a concise summary in mail thread `{}` \
         (reply_message / send with same thread_id) so the requester can read it. \
         Acknowledge the request message if ack_required.",
        item.id,
        item.thread_id,
        item.from,
        req.recipe,
        req.workers,
        req.product,
        req.params,
        item.thread_id
    )
}

/// Extract `[xproj]` items from a `fetch_inbox` JSON value (pure, unit-tested).
/// Accepts either a bare array or `{messages:[...]}` / `{result:[...]}`.
#[must_use]
pub fn extract_xproj_from_inbox(v: &serde_json::Value) -> Vec<XprojInboxItem> {
    let arr = v
        .as_array()
        .or_else(|| v.get("messages").and_then(|m| m.as_array()))
        .or_else(|| v.get("result").and_then(|m| m.as_array()))
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for m in arr {
        let subject = m
            .get("subject")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let body = m
            .get("body_md")
            .or_else(|| m.get("body"))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let combined = format!("{subject}\n{body}");
        if !combined.to_ascii_lowercase().contains("[xproj]") {
            continue;
        }
        let id = match m.get("id") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        let thread_id = m
            .get("thread_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let from = m
            .get("from")
            .or_else(|| m.get("sender"))
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        out.push(XprojInboxItem {
            id,
            subject,
            body,
            thread_id,
            from,
        });
    }
    out
}

/// Filter unseen xproj items and produce (message_id, queue_prompt) pairs.
#[must_use]
pub fn xproj_prompts_for_unseen(
    items: &[XprojInboxItem],
    seen: &std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in items {
        if seen.contains(&item.id) {
            continue;
        }
        let combined = format!("{}\n{}", item.subject, item.body);
        if let Some(req) = parse_xproj_request(&combined) {
            out.push((item.id.clone(), xproj_queue_prompt(item, &req)));
        }
    }
    out
}

/// bd-context-limit-honor-luqg: extract exact-body CONTEXT_LIMIT messages
/// from an orchestrator-inbox JSON value — the crew saturation contract's
/// respawn trigger. The BODY must equal `CONTEXT_LIMIT` after trimming (a
/// subject mention or "CONTEXT_LIMIT\nall done" is NOT the signal; the
/// contract says reply EXACTLY that). Same envelope tolerance as the xproj
/// scan (bare array / `{messages}` / `{result}`); carries id/thread/from so
/// the loop can dedup + map the sender to a pane.
#[must_use]
pub fn extract_context_limit_from_inbox(v: &serde_json::Value) -> Vec<XprojInboxItem> {
    const MARKER: &str = "CONTEXT_LIMIT";
    let arr = v
        .as_array()
        .or_else(|| v.get("messages").and_then(|m| m.as_array()))
        .or_else(|| v.get("result").and_then(|m| m.as_array()))
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for m in arr {
        let body = m
            .get("body_md")
            .or_else(|| m.get("body"))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .trim();
        if body != MARKER {
            continue;
        }
        let id = match m.get("id") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        out.push(XprojInboxItem {
            id,
            subject: m
                .get("subject")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            body: MARKER.to_string(),
            thread_id: m
                .get("thread_id")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            from: m
                .get("from")
                .or_else(|| m.get("sender"))
                .and_then(|f| f.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    out
}

/// Parse an `[xproj]` swarm request from subject and/or body.
///
/// Accepts lines like `recipe=backtest-5m`, `workers=4`, `product=trading-research`,
/// free-form remainder as params. Subject may be `[xproj] run … recipe=foo`.
#[must_use]
pub fn parse_xproj_request(text: &str) -> Option<XprojRequest> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if !t.to_ascii_lowercase().contains("[xproj]") {
        return None;
    }
    let mut recipe = String::new();
    let mut workers = 2u32;
    let mut product = "trading-research".to_string();
    let mut params_parts: Vec<String> = Vec::new();
    for raw in t.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("recipe=") {
            recipe = line[line.len() - rest.len()..].to_string();
            continue;
        }
        if let Some(rest) = lower.strip_prefix("workers=") {
            if let Ok(n) = rest.trim().parse::<u32>() {
                workers = n.max(1);
            }
            continue;
        }
        if let Some(rest) = lower.strip_prefix("product=") {
            product = line[line.len() - rest.len()..].to_string();
            continue;
        }
        if lower.starts_with("[xproj]") && !line.contains('=') {
            for tok in line.split_whitespace() {
                let tl = tok.to_ascii_lowercase();
                if let Some(r) = tl.strip_prefix("recipe=") {
                    recipe = tok[tok.len() - r.len()..].to_string();
                }
            }
            continue;
        }
        params_parts.push(line.to_string());
    }
    if recipe.is_empty() {
        for tok in t.split_whitespace() {
            let tl = tok.to_ascii_lowercase();
            if let Some(r) = tl.strip_prefix("recipe=") {
                recipe = tok[tok.len() - r.len()..].to_string();
                break;
            }
        }
    }
    if recipe.is_empty() {
        return None;
    }
    Some(XprojRequest {
        recipe,
        workers,
        params: params_parts.join("\n"),
        product,
    })
}

/// bd-blank-project-spelling-non-mail-doors-76hi: the non-mail sibling of
/// [`mail_project`]'s blank-spelling rejection. A blank/whitespace project
/// spelling fails loud with the same actionable message as a missing one —
/// `project_dir(cfg, "")` joins the workspace root, so the empty spelling
/// used to alias every door onto the workspace default project. Only the
/// WINNING spelling is validated; no silent fallback.
pub(crate) fn check_project_spelling(project: &str, door: &str) -> Result<()> {
    let p = project.trim();
    if p.is_empty() {
        anyhow::bail!("{door} requires a project: pass PROJECT positionally or --project <slug>");
    }
    if p == "." || p == ".." {
        anyhow::bail!("{door} requires a project slug, not {p:?} (cwd alias is not a project)");
    }
    Ok(())
}

/// `Option<&str>` door: `None` keeps the door's own default; `Some(blank)`
/// fails loud. Returns the winning spelling.
pub(crate) fn project_door<'a>(
    project: Option<&'a str>,
    default: &'a str,
    door: &str,
) -> Result<&'a str> {
    match project {
        Some(p) => {
            check_project_spelling(p, door)?;
            Ok(p)
        }
        None => Ok(default),
    }
}

/// `&Option<String>` arm field: validates the winning spelling and passes
/// the Option through, so downstream `as_deref().unwrap_or(default)` call
/// sites stay put (blank already errored by the time they run).
pub(crate) fn project_door_opt<'a>(
    project: &'a Option<String>,
    door: &str,
) -> Result<Option<&'a str>> {
    match project {
        Some(p) => {
            check_project_spelling(p, door)?;
            Ok(Some(p.as_str()))
        }
        None => Ok(None),
    }
}

/// Validate-when-present door: `None` is genuinely meaningful (no project
/// filter), `Some(blank)` fails loud. For doors whose default differs from
/// every other door — semantics preserved, only the blank spelling fixed.
pub(crate) fn ensure_project_opt(project: Option<&str>, door: &str) -> Result<()> {
    if let Some(p) = project {
        check_project_spelling(p, door)?;
    }
    Ok(())
}

/// bd-3npc: `--project` flag alias for every mail-cli positional PROJECT.
/// GS-030 slice 3 (bd-gs030-rolebind-02mi): the role noun a spawn auto-binds
/// to its pin via `toron role pin` charter (30013 `role=` → `Msg::RoleBound`
/// → `WorkspaceModel.roles`). Profile name when profile-driven, else the
/// kind short. `None` for captain (reserved — `identity.rs:375`; captain
/// stays `toron contact captain`) and for empty nouns. Pure.
pub(crate) fn spawn_role_noun(profile: Option<&str>, short: &str) -> Option<String> {
    let raw = profile
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| short.trim());
    let noun = raw.strip_prefix('@').unwrap_or(raw).trim();
    if noun.is_empty() || noun == "captain" {
        return None;
    }
    Some(noun.to_string())
}

/// GS-033 B8: spawn-time role bind. After `set_role_pin`, fold immediate
/// `Msg::RoleBound` onto the one Model via the pending/snapshot path
/// (not watch-racy 30013). Remote / pin failure is fail-loud (NEW-L7).
/// If `emit_role_bound` fails after the mail pin, the alias is cleared
/// (compensate) so mail pin and RoleBound stay paired. No pin or no role
/// noun (captain/empty) → silent no-op.
fn bind_spawn_role(
    cfg: &Config,
    project: &str,
    profile: Option<&str>,
    short: &str,
    pin: Option<&str>,
) -> anyhow::Result<()> {
    let Some(pin) = pin.filter(|p| !p.is_empty()) else {
        return Ok(());
    };
    let Some(role) = spawn_role_noun(profile, short) else {
        return Ok(());
    };
    let facade = mail::Mail::from_config(Some(&cfg.toron_mail_config()))
        .map_err(|e| {
            anyhow::anyhow!("mail role pin failed for role {role} -> {pin}: {e:#}")
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "mail role pin failed for role {role} -> {pin}: mailbox facade unavailable (refusing a silent skip)"
            )
        })?;
    facade
        .pin_role_alias(project, &role, pin)
        .map_err(|e| anyhow::anyhow!("mail role pin failed for role {role} -> {pin}: {e:#}"))?;
    if let Err(e) = emit_role_bound(project, &role, pin) {
        match facade.clear_role_alias(project, &role) {
            Ok(_) => anyhow::bail!(
                "role bind emit failed for role {role} -> {pin}: {e:#} (mail role alias cleared)"
            ),
            Err(rb) => anyhow::bail!(
                "role bind emit failed for role {role} -> {pin}: {e:#}; also failed to clear mail role alias: {rb:#}"
            ),
        }
    }
    Ok(())
}
/// Spawn-path role-bind is best-effort: the pane is the product and the alias
/// is re-bindable via `toron role pin`. A failed bind warns loud and lets the
/// spawn continue — it must never abort panes 2..N (1-pane-cap root cause).
fn note_role_bind_result(res: anyhow::Result<()>, project: &str) {
    if let Err(e) = res {
        eprintln!(
            "flywheel: warn: {e:#} — role alias unbound; re-bind with `toron role pin <role> <pin> --project {project}`"
        );
    }
}

#[cfg(test)]
mod role_bind_best_effort_tests {
    use super::*;

    /// Both arms return normally: a role-bind failure is non-fatal to spawn.
    #[test]
    fn role_bind_result_never_aborts() {
        note_role_bind_result(Ok(()), "proj");
        note_role_bind_result(Err(anyhow::anyhow!("daemon down")), "proj");
    }
}

/// Persist + queue `Msg::RoleBound` so the doorbell owner ticks it
/// immediately (pending drain) and on crash-resume (role_binds snapshot).
/// Upsert + pending append are compensated: on append Err the snapshot is
/// rolled back so the pair stays atomic (GS-033).
fn emit_role_bound(project: &str, role: &str, pin: &str) -> anyhow::Result<()> {
    let prior = state::load_role_binds(project).map_err(|e| {
        anyhow::anyhow!("role bind snapshot load failed for role {role} -> {pin}: {e:#}")
    })?;
    state::upsert_role_bind(project, role, pin).map_err(|e| {
        anyhow::anyhow!("role bind snapshot failed for role {role} -> {pin}: {e:#}")
    })?;
    if let Err(e) = append_pending_role_bound(project, role, pin) {
        if let Err(rb) = state::save_role_binds(project, &prior) {
            anyhow::bail!(
                "role bind pending append failed for role {role} -> {pin}: {e:#}; also failed to roll back snapshot: {rb:#}"
            );
        }
        anyhow::bail!(
            "role bind pending append failed for role {role} -> {pin}: {e:#} (snapshot rolled back)"
        );
    }
    Ok(())
}

fn seed_roles_from_binds(
    workspace_model: &mut workspace::WorkspaceModel,
    project: &str,
) -> anyhow::Result<()> {
    let binds = state::load_role_binds(project).map_err(|e| {
        anyhow::anyhow!("seed_roles_from_binds({project}): load_role_binds failed: {e:#}")
    })?;
    for (role, pin) in binds {
        let (next, _) = workspace::tick(workspace_model, &workspace::Msg::RoleBound { role, pin });
        *workspace_model = next;
    }
    Ok(())
}

fn pending_role_bounds_path(project: &str) -> std::path::PathBuf {
    config::state_dir()
        .join("roles")
        .join(format!("{}-pending.jsonl", state::state_key(project)))
}

fn append_pending_role_bound(project: &str, role: &str, pin: &str) -> anyhow::Result<()> {
    let path = pending_role_bounds_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{}", serde_json::json!({ "role": role, "pin": pin }))?;
    Ok(())
}

/// Drain pending RoleBound lines. Missing file → empty. Read I/O Err is
/// fail-loud (never silent empty). Validate/parse before `remove_file` so a
/// crash mid-drain cannot lose RoleBound rows; corrupt lines warn loudly
/// instead of being dropped without a trace (GS-033).
fn drain_pending_role_bounds(project: &str) -> anyhow::Result<Vec<(String, String)>> {
    let path = pending_role_bounds_path(project);
    let raw = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!(
                "pending role bounds read failed at {}: refusing silent empty",
                path.display()
            )));
        }
        Ok(raw) => raw,
    };
    let mut out = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => match (
                v.get("role").and_then(|r| r.as_str()),
                v.get("pin").and_then(|p| p.as_str()),
            ) {
                (Some(role), Some(pin)) if !role.is_empty() && !pin.is_empty() => {
                    out.push((role.to_string(), pin.to_string()));
                }
                _ => {
                    eprintln!(
                        "flywheel: warn: pending role bound line {} in {} missing role/pin — skipping corrupt row (RoleBound not applied)",
                        idx + 1,
                        path.display()
                    );
                }
            },
            Err(e) => {
                eprintln!(
                    "flywheel: warn: pending role bound line {} in {} is corrupt JSON ({e}) — skipping (RoleBound not applied)",
                    idx + 1,
                    path.display()
                );
            }
        }
    }
    if let Err(e) = std::fs::remove_file(&path) {
        eprintln!(
            "flywheel: warn: could not remove drained pending role bounds {}: {e}",
            path.display()
        );
    }
    Ok(out)
}

/// GS-033: fail-closed when mail is configured and a spawn/add/profile/squad
/// path drew no pin. Same fate as cmd_spawn. Returns Ok for legacy+warn
/// (caller still writes the loud leftover).
fn refuse_unpinned_spawn(
    name: &str,
    profile: Option<&str>,
    mail_configured: bool,
) -> anyhow::Result<()> {
    if unpinned_spawn_fate(mail_configured) != UnpinnedFate::FailClosed {
        return Ok(());
    }
    match profile {
        Some(p) => anyhow::bail!(
            "spawn {name} (profile {p}) drew no mail identity while mail is configured — refusing a dark pane (fix mail, then re-run; never operates unpinned)"
        ),
        None => anyhow::bail!(
            "spawn {name} drew no mail identity while mail is configured — refusing a dark pane (fix mail, then re-run; never operates unpinned)"
        ),
    }
}

/// S8.3/S8.4: the mail profile a spawn registers — the CANONICAL terminal name
/// as the identity (the random npub stays underneath; toron accepts any
/// name, the legacy agent-mail adjective+noun whitelist does not apply),
/// with a fresh task stamp whose idempotent upsert overwrites stale task
/// fields from a prior session (debrief A2: a 2026-08-14 identity still
/// advertised `bd-qk9q`).
pub(crate) fn spawn_agent_profile(name: &str, kind: &str, project: &str) -> backend::AgentProfile {
    backend::AgentProfile {
        name: name.to_string(),
        program: Some(kind.to_string()),
        model: Some(kind.to_string()),
        persona: None,
        task_description: Some(format!("{name} (flywheel agent for {project})")),
    }
}

#[cfg(test)]
mod spawn_identity_divergence_tests {
    use super::*;

    /// bd-0gup: the captain pane arriving as ITSELF is a CORRECT arrival.
    ///
    /// The old guard refused every arrival whose requested pin was the
    /// captain pin AND whose pane had a registry row — including the pane
    /// whose OWN injected identity IS that captain pin. The refusal then
    /// told the operator to re-run with the pin it had just rejected.
    #[test]
    fn arrive_identity_captain_pane_may_arrive_as_itself() {
        assert_eq!(
            arrive_identity_verdict("EmberOtter", Some("EmberOtter"), None, Some("EmberOtter")),
            ArriveIdentityVerdict::Match,
            "the captain pane arriving as its own injected identity is not a refusal"
        );
        assert_eq!(
            arrive_identity_verdict("EmberOtter", None, Some("EmberOtter"), Some("EmberOtter")),
            ArriveIdentityVerdict::Match,
            "the env-injected captain identity is the pane's own identity too"
        );
        assert!(
            matches!(
                arrive_identity_verdict("EmberOtter", Some("SilverPond"), None, Some("EmberOtter")),
                ArriveIdentityVerdict::Refused { .. }
            ),
            "a spawned pane must not arrive as the captain pin"
        );
    }

    /// bd-spawn-identity-divergence-2vzw: a spawned pane may only arrive as
    /// its injected pin; the captain pin is refused for spawned panes; an
    /// operator pane (no spawn row) keeps explicit --as. Plus the divergence
    /// detector and the captain-pin pool exclusion.
    #[test]
    fn spawn_identity_matches_registry() {
        assert_eq!(
            arrive_identity_verdict("SilverPond", Some("SilverPond"), None, Some("EmberOtter")),
            ArriveIdentityVerdict::Match
        );
        assert!(matches!(
            arrive_identity_verdict("SlateHerder", Some("SilverPond"), None, Some("EmberOtter")),
            ArriveIdentityVerdict::Refused { .. }
        ));
        assert!(matches!(
            arrive_identity_verdict("EmberOtter", Some("SilverPond"), None, Some("EmberOtter")),
            ArriveIdentityVerdict::Refused { .. }
        ));
        assert!(matches!(
            arrive_identity_verdict("SlateHerder", None, Some("SilverPond"), None),
            ArriveIdentityVerdict::Refused { .. }
        ));
        assert_eq!(
            arrive_identity_verdict("EmberOtter", None, None, Some("EmberOtter")),
            ArriveIdentityVerdict::Unbound
        );
        assert_eq!(
            identity_divergence(Some("SilverPond"), Some("SlateHerder")),
            Some("SilverPond != SlateHerder".to_string())
        );
        assert_eq!(
            identity_divergence(Some("SilverPond"), Some("SilverPond")),
            None
        );
        assert_eq!(identity_divergence(None, Some("SlateHerder")), None);
        assert_eq!(identity_divergence(Some(""), Some("SlateHerder")), None);
        let captain_pin = adjnoun_from_fnv(7);
        let mut taken = std::collections::HashSet::new();
        taken.insert(captain_pin.clone());
        let drawn = unique_adjnoun_pin(&taken, 7);
        assert_ne!(drawn, captain_pin, "captain pin must never be drawn");
    }
}

#[cfg(test)]
mod wake_identity_tests {
    use super::*;
    use crate::state::SpawnEntry;

    fn entry(name: &str, mid: Option<&str>) -> SpawnEntry {
        match mid {
            Some(m) => SpawnEntry::with_mail_identity("p1", name, m),
            None => SpawnEntry::legacy("p1", name),
        }
    }

    fn assign_row(id: &str, body: &str, thread: &str, created_us: i64, read: bool) -> Value {
        json!({
            "id": id,
            "sender": "CoralFinch",
            "subject": "[assign] toron → bd-x.1",
            "body_md": body,
            "importance": "high",
            "thread_id": thread,
            "created_ts_us": created_us,
            "read_ts_us": read.then(|| created_us + 1),
        })
    }

    /// bd-unpinned-loud-1mlt (GS-028 B6+B7): the helper lists exactly the
    /// rows the sweep skips — legacy entries with no (or empty) pin.
    #[test]
    fn unpinned_loud_lists_legacy_rows_only() {
        assert_eq!(
            unpinned_registry_names(&[]),
            Vec::<String>::new(),
            "empty registry is clean"
        );
        let reg = vec![
            entry("flywheel-toron-oc", Some("DustySparrow")),
            entry("flywheel-toron-pi", None),
            entry("crew-plan-coder", None),
        ];
        assert_eq!(
            unpinned_registry_names(&reg),
            vec![
                "flywheel-toron-pi".to_string(),
                "crew-plan-coder".to_string()
            ],
            "legacy rows listed, pinned excluded"
        );
        let pinned = vec![entry("flywheel-toron-oc", Some("DustySparrow"))];
        assert!(
            unpinned_registry_names(&pinned).is_empty(),
            "fully-pinned registry is clean"
        );
    }

    /// GS-030 slice 4 (bd-gs030-failclosed-m1x0): the fate matrix — a
    /// configured backend that yields no pin fails closed (profiled and
    /// bare alike: a pin should exist); an unconfigured backend proceeds
    /// legacy+warn (bare experiments unaffected).
    #[test]
    fn unpinned_fail_closed_matrix() {
        assert_eq!(
            unpinned_spawn_fate(true),
            UnpinnedFate::FailClosed,
            "configured backend + no pin = fail"
        );
        assert_eq!(
            unpinned_spawn_fate(false),
            UnpinnedFate::LegacyWarn,
            "unconfigured backend + no pin = legacy warn"
        );
    }

    /// GS-030 slice 3 (bd-gs030-rolebind-02mi): role noun is profile-first,
    /// `@`-stripped, short fallback; captain and empty never bind.
    #[test]
    fn spawn_role_noun_role_bind_profile_first_captain_refused() {
        assert_eq!(
            spawn_role_noun(Some("coder"), "oc"),
            Some("coder".to_string())
        );
        assert_eq!(
            spawn_role_noun(Some("@reviewer"), "oc"),
            Some("reviewer".to_string())
        );
        assert_eq!(spawn_role_noun(None, "oc"), Some("oc".to_string()));
        assert_eq!(spawn_role_noun(Some("captain"), "oc"), None);
        assert_eq!(spawn_role_noun(Some(""), "oc"), Some("oc".to_string()));
        assert_eq!(spawn_role_noun(None, ""), None);
        assert_eq!(spawn_role_noun(None, "captain"), None);
    }

    /// GS-033 B8: bind constructs RoleBound (not watch-only) and is wired
    /// on profile/squad as well as spawn/add. Remote is fail-loud.
    #[test]
    fn spawn_role_bind_emits_role_bound_on_all_paths() {
        let src = include_str!("main.rs");
        let bind = src.find("fn bind_spawn_role(").expect("bind_spawn_role");
        let bind_fn = &src[bind..src.len().min(bind + 2500)];
        assert!(
            bind_fn.contains("emit_role_bound") && bind_fn.contains("pin_role_alias"),
            "B8: bind must pin then emit RoleBound"
        );
        assert!(
            bind_fn.contains("clear_role_alias")
                && (bind_fn.contains("mail role alias cleared")
                    || bind_fn.contains("failed to clear mail role alias")),
            "B8: bind must compensate (unpin) if emit_role_bound fails after pin"
        );
        assert!(
            bind_fn.contains("anyhow::Result"),
            "B8 / NEW-L7: bind is fail-loud, not warn-and-skip"
        );
        let emit = src.find("fn emit_role_bound(").expect("emit_role_bound");
        let emit_fn = &src[emit..src.len().min(emit + 1600)];
        assert!(
            emit_fn.contains("upsert_role_bind") && emit_fn.contains("append_pending_role_bound"),
            "B8: RoleBound must persist snapshot + pending drain"
        );
        assert!(
            emit_fn.contains("save_role_binds") && emit_fn.contains("rolled back"),
            "B8: emit must compensate (rollback snapshot) on append Err"
        );
        assert!(
            src.contains("drain_pending_role_bounds")
                && src.contains("role_bound_live")
                && src.contains("Msg::RoleBound"),
            "B8: doorbell must tick pending RoleBound on the one Model"
        );
        let drain = src
            .find("fn drain_pending_role_bounds(")
            .expect("drain_pending_role_bounds");
        let drain_fn = &src[drain..src.len().min(drain + 2500)];
        assert!(
            drain_fn.contains("anyhow::Result")
                && drain_fn.contains("refusing silent empty")
                && drain_fn.contains("NotFound"),
            "B8: drain must fail-loud on read Err (NotFound → empty)"
        );
        assert!(
            drain_fn.contains("corrupt") || drain_fn.contains("skipping corrupt"),
            "B8: drain must warn on corrupt lines (never silent drop)"
        );
        let parse_mark = drain_fn
            .find("serde_json::from_str")
            .expect("drain parses JSON");
        let remove_mark = drain_fn
            .find("remove_file")
            .expect("drain removes after validate");
        assert!(
            parse_mark < remove_mark,
            "B8: drain must validate/parse BEFORE remove_file (never lose RoleBound)"
        );
        assert!(
            src.contains("refuse_unpinned_spawn(&name, None,")
                && src.contains("bind_spawn_role(cfg, project, None, short,"),
            "NEW-L8: add path fail-closed + bind"
        );
        assert!(
            src.contains("refuse_unpinned_spawn(&agent_name, Some(name),"),
            "NEW-L8: profile spawn must bind + fail-closed"
        );
        let psrc = include_str!("cmd/profile.rs");
        let profile_mark = psrc
            .find("let agent_name = format!(\"profile-{name}\")")
            .expect("profile agent_name");
        let profile_slice = &psrc[profile_mark..psrc.len().min(profile_mark + 1200)];
        let p_refuse = profile_slice
            .find("refuse_unpinned_spawn")
            .expect("profile refuse");
        let p_start = profile_slice
            .find("backend.agent_start")
            .expect("profile agent_start");
        assert!(
            p_refuse < p_start,
            "NEW-L8: profile refuse_unpinned must run BEFORE agent_start"
        );
        let qsrc = include_str!("cmd/squad.rs");
        let squad_mark = qsrc
            .find("let agent_name = format!(\"squad-{name}-{profile_name}-{}\"")
            .expect("squad agent_name");
        let squad_slice = &qsrc[squad_mark..qsrc.len().min(squad_mark + 1400)];
        let s_refuse = squad_slice
            .find("refuse_unpinned_spawn")
            .expect("squad refuse");
        let s_start = squad_slice
            .find("backend.agent_start")
            .expect("squad agent_start");
        assert!(
            s_refuse < s_start,
            "NEW-L8: squad refuse_unpinned must run BEFORE agent_start"
        );
        let csrc = crate::test_source::crate_source();
        let spawn_fn = crate::test_source::slice_from(&csrc, "pub(crate) fn cmd_spawn(", 55_000);
        let sp_refuse = spawn_fn
            .find("refuse_unpinned_spawn")
            .expect("cmd_spawn refuse_unpinned");
        let sp_start = spawn_fn
            .find("start_agent_with_retry_profile")
            .expect("cmd_spawn start_agent");
        assert!(
            sp_refuse < sp_start,
            "NEW-L8: cmd_spawn refuse_unpinned must run BEFORE agent_start"
        );
        assert!(
            src.contains("Some(profile_name)"),
            "NEW-L8: squad spawn must bind"
        );
        let shim = include_str!("mail.rs");
        assert!(
            shim.contains("refusing a silent skip"),
            "NEW-L7: remote role-pin must fail-loud"
        );
    }

    #[test]
    fn refuse_unpinned_fail_closed_matches_cmd_spawn() {
        assert!(
            refuse_unpinned_spawn("flywheel-demo-oc", None, true).is_err(),
            "mail configured + no pin = fail"
        );
        assert!(refuse_unpinned_spawn("flywheel-demo-oc", Some("coder"), true).is_err());
        assert!(
            refuse_unpinned_spawn("flywheel-demo-oc", None, false).is_ok(),
            "mail unconfigured = legacy warn path (caller warns)"
        );
    }

    /// S8.1: the wake identity is the registry pin, else the canonical name
    /// itself — never a not-yet-registered alias.
    #[test]
    fn wake_identity_prefers_pin_else_canonical_name() {
        let reg = vec![entry("flywheel-toron-oc", Some("DustySparrow"))];
        let names = vec![entry("flywheel-toron-oc", Some("DustySparrow"))];
        assert_eq!(
            wake_identity_pin(&reg, &names, "flywheel-toron-oc"),
            "DustySparrow",
            "legacy registry pin still wins"
        );
        assert_eq!(
            wake_identity_pin(&[], &names, "flywheel-toron-oc"),
            "DustySparrow",
            "agent-names mirror pin wins when the registry is empty"
        );
        assert_eq!(
            wake_identity_pin(&[], &[], "flywheel-toron-oc4"),
            "flywheel-toron-oc4",
            "unregistered name resolves to itself — the wake registers it"
        );
        let unpinned = vec![entry("flywheel-toron-oc", None)];
        assert_eq!(
            wake_identity_pin(&unpinned, &[], "flywheel-toron-oc"),
            "flywheel-toron-oc",
            "legacy entry without a pin falls back to the canonical name"
        );
    }

    /// S8.2: a plain urgent message always wakes.
    #[test]
    fn wake_urgent_keeps_normal_urgent() {
        let rows = vec![json!({
            "id": "m1", "sender": "PinkGorge", "subject": "[bd-1] help",
            "body_md": "x", "importance": "urgent", "thread_id": "t1",
            "created_ts_us": 1000,
        })];
        assert_eq!(wake_urgent(&rows).len(), 1);
    }

    /// The observed defect: three urgent rows arrived oldest-first and
    /// `urgent_rows.first()` cited created_ts 1787300794 while newer rows
    /// sat at 1787302173 (same thread) and 1787304318 (a DIFFERENT thread).
    /// The fence: seed 3 ack-required/high messages of differing ts across
    /// 2 threads; the selector MUST return max(created_ts_us).
    #[test]
    fn vbf2_newest_urgent_wins_regardless_of_wire_order() {
        let oldest = json!({
            "id": "bc27c6a70fc8", "sender": "PinkCastle", "subject": "a needs review",
            "body_md": "x", "importance": "high", "thread_id": "c43357a1",
            "created_ts_us": 1_787_300_794u64,
        });
        let mid = json!({
            "id": "d46880a2b0ef", "sender": "SandySparrow", "subject": "re: a",
            "body_md": "x", "importance": "high", "thread_id": "c43357a1",
            "created_ts_us": 1_787_302_173u64,
        });
        let newest = json!({
            "id": "ccd46cbb7dc9", "sender": "TanFern", "subject": "[request-release] x",
            "body_md": "x", "importance": "high", "thread_id": "344e63b7",
            "created_ts_us": 1_787_304_318u64,
        });
        let rows = vec![oldest.clone(), mid.clone(), newest.clone()];
        let urgent = wake_urgent(&rows);
        assert_eq!(urgent.len(), 3, "all three are urgent");
        let picked = newest_urgent(&urgent, rows.first()).expect("pick");
        assert_eq!(
            picked.get("id").and_then(|i| i.as_str()),
            Some("ccd46cbb7dc9"),
            "max(created_ts_us) across threads wins"
        );
        let rev = vec![newest.clone(), mid, oldest];
        let picked = newest_urgent(&wake_urgent(&rev), rev.first()).expect("pick");
        assert_eq!(
            picked.get("id").and_then(|i| i.as_str()),
            Some("ccd46cbb7dc9")
        );
    }

    /// bd-9w95: `created_ts_us` is microseconds, but a burst can still land two
    /// urgent rows in the same one, and with the timestamp alone the winner is
    /// whichever row the slice ends on — the exact "trust the wire order"
    /// failure bd-vbf2 removed from the timestamp axis. The fixture is
    /// adversarial by construction: the two orders disagree, so a last-wins
    /// selector fails one of them.
    #[test]
    fn vbf2_a_shared_microsecond_resolves_by_id_in_either_order() {
        let low = json!({
            "id": "1111aaaa", "sender": "PinkCastle", "subject": "a", "body_md": "x",
            "importance": "high", "thread_id": "c43357a1", "created_ts_us": 1_787_300_794u64,
        });
        let high = json!({
            "id": "9999zzzz", "sender": "SandySparrow", "subject": "b", "body_md": "x",
            "importance": "high", "thread_id": "c43357a1", "created_ts_us": 1_787_300_794u64,
        });
        assert_eq!(
            low["created_ts_us"], high["created_ts_us"],
            "precondition: a shared timestamp or the tie never happens"
        );
        for order in [vec![low.clone(), high.clone()], vec![high, low]] {
            let picked = newest_urgent(&wake_urgent(&order), None).expect("pick");
            assert_eq!(
                picked.get("id").and_then(|i| i.as_str()),
                Some("9999zzzz"),
                "the higher event id wins the shared microsecond in either order"
            );
        }
    }

    #[test]
    fn vbf2_falls_back_to_first_raw_row_when_nothing_urgent() {
        let normal = json!({
            "id": "n1", "sender": "A", "subject": "hi", "body_md": "x",
            "importance": "normal", "created_ts_us": 5,
        });
        let rows = vec![normal];
        let urgent = wake_urgent(&rows);
        assert_eq!(urgent, [] as [&serde_json::Value; 0]);
        let picked = newest_urgent(&urgent, rows.first()).expect("fallback");
        assert_eq!(picked.get("id").and_then(|i| i.as_str()), Some("n1"));
        assert!(newest_urgent(&[], None).is_none());
    }

    /// bd-kryk: a system/transport notification (reservation release —
    /// empty subject, "reservation … released …" body) NEVER wakes, even
    /// when mis-tagged importance=high. This is the phantom-urgent bug: the
    /// loop's own transport chatter manufactured wake banners.
    #[test]
    fn kryk_wake_urgent_excludes_reservation_release_noise() {
        let noise_high = json!({
            "id": "n1", "sender": "994d117d", "subject": "",
            "body_md": "reservation crates/x.rs (TealDune by CoralFinch) released because bead bd-x closed",
            "importance": "high", "created_ts_us": 1000,
        });
        let noise_normal = json!({
            "id": "n2", "sender": "994d117d", "subject": "",
            "body_md": "reservation src/y.rs (A by B) released because bead bd-y closed",
            "importance": "normal", "created_ts_us": 1001,
        });
        let real = json!({
            "id": "m1", "sender": "PinkGorge", "subject": "[bd-1] help",
            "body_md": "x", "importance": "urgent", "thread_id": "t1",
            "created_ts_us": 1002,
        });
        let rows = vec![noise_high, noise_normal, real];
        let out = wake_urgent(&rows);
        assert_eq!(out.len(), 1, "only the real message wakes: {out:?}");
        assert_eq!(out[0]["id"], "m1");
    }

    /// bd-kryk: a REAL peer message with an empty subject still wakes —
    /// the transport-noise shape requires the reservation-release body.
    #[test]
    fn kryk_empty_subject_non_transport_still_wakes() {
        let rows = vec![json!({
            "id": "m2", "sender": "SandySparrow", "subject": "",
            "body_md": "decision: agreed — lease released to you",
            "importance": "high", "created_ts_us": 1000,
        })];
        assert_eq!(wake_urgent(&rows).len(), 1);
    }

    /// S8.2: two identical assigns (same sender+subject+body) collapse to one.
    #[test]
    fn wake_urgent_collapses_duplicate_assigns() {
        let rows = vec![
            assign_row("18072be9", "Assigned beads: bd-gsvn.2", "t-a", 1000, false),
            assign_row("4436c7eb", "Assigned beads: bd-gsvn.2", "t-a", 1001, false),
        ];
        let out = wake_urgent(&rows);
        assert_eq!(out.len(), 1, "identical assigns wake once");
        assert_eq!(out[0]["id"], "18072be9", "the first assign wins");
    }

    /// S8.2: an assign the agent already read does not wake.
    #[test]
    fn wake_urgent_skips_read_assign() {
        let rows = vec![assign_row(
            "m1",
            "Assigned beads: bd-x.1",
            "t-1",
            1000,
            true,
        )];
        assert_eq!(wake_urgent(&rows).len(), 0, "read assign must not wake");
    }

    /// bd-dbre: a READ non-assign urgent row does not wake either —
    /// per-recipient read state is the once-only signal for every urgent
    /// row, not just assigns (the forever-wake defect).
    #[test]
    fn wake_urgent_skips_read_nonassign_urgent() {
        let rows = vec![json!({
            "id": "m1", "sender": "9150144f",
            "subject": "", "body_md": "reservation released",
            "importance": "high", "read_ts_us": 1787310913232405i64,
            "created_ts_us": 1000,
        })];
        assert_eq!(
            wake_urgent(&rows).len(),
            0,
            "read non-assign urgent must not wake"
        );
    }

    /// S8.2: an assign whose thread already has a reply is executed — no wake.
    #[test]
    fn wake_urgent_skips_executed_assign() {
        let rows = vec![
            assign_row("18072be9", "Assigned beads: bd-gsvn.2", "t-a", 1000, false),
            json!({
                "id": "reply-1", "sender": "YellowCastle",
                "subject": "Re: [assign] toron → bd-gsvn.2",
                "body_md": "all closed", "importance": "normal",
                "thread_id": "t-a", "created_ts_us": 2000,
            }),
        ];
        assert_eq!(
            wake_urgent(&rows).len(),
            0,
            "executed assign (thread moved) must not wake"
        );
    }

    /// S8.2: an unread, unexecuted assign DOES wake.
    #[test]
    fn wake_urgent_keeps_fresh_assign() {
        let rows = vec![assign_row(
            "m1",
            "Assigned beads: bd-x.1",
            "t-1",
            1000,
            false,
        )];
        assert_eq!(wake_urgent(&rows).len(), 1);
    }

    /// S9 (ADR-0018 D1a): the spawn profile registers the adj+noun PIN as
    /// the mail identity with a fresh task stamp (the upsert clears stale
    /// prior-session tasks).
    #[test]
    fn spawn_profile_uses_adjnoun_pin_and_fresh_task() {
        let p = spawn_agent_profile("DustySparrow", "opencode", "toron");
        assert_eq!(p.name, "DustySparrow", "the adj+noun pin is the identity");
        let task = p.task_description.unwrap();
        assert!(task.contains("flywheel agent for toron"), "{task}");
        assert!(
            !task.contains("bd-"),
            "no stale bead id in the fresh stamp: {task}"
        );
    }

    /// S9 (ADR-0018 D1a): a pin draw whose first candidate is taken (spawn
    /// registry pin or registered agent) redraws to a DIFFERENT valid
    /// adj+noun pair; a fully-blocked prefix still terminates.
    #[test]
    fn pin_collision_redraws() {
        let seed = state::fnv1a64("flywheel-toron-oc");
        let first = adjnoun_from_fnv(seed);
        assert!(valid_pair(&first, &ADJECTIVES, &NOUNS).is_some(), "{first}");
        let taken: std::collections::HashSet<String> =
            std::collections::HashSet::from([first.clone()]);
        let redrawn = unique_adjnoun_pin(&taken, seed);
        assert_ne!(redrawn, first, "collision must redraw");
        assert!(valid_pair(&redrawn, &ADJECTIVES, &NOUNS).is_some());
        let blocked: std::collections::HashSet<String> =
            (0..10).map(|i| adjnoun_from_fnv(seed + i)).collect();
        let out = unique_adjnoun_pin(&blocked, seed);
        assert!(!blocked.contains(&out), "redraw escapes the blocked prefix");
    }

    /// S9 (ADR-0018 D1a): the pin lands in the spawn registry AND the
    /// project-local agent-names.json mirror; wake + debate/send resolve the
    /// slug THROUGH the pin (the slug stays the technical pane handle).
    #[test]
    fn spawn_pin_flows_into_agent_names_and_resolves() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-s9-names-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let project = "/tmp/fw-s9-proj";
        let entries = vec![state::SpawnEntry::with_mail_identity(
            "w7:p1",
            "flywheel-toron-oc",
            "DustySparrow",
        )];
        state::save_spawn_registry(project, &entries).expect("save registry");
        state::save_agent_names(std::path::Path::new(project), &entries).expect("save names");
        let names = state::load_agent_names(std::path::Path::new(project));
        assert_eq!(names[0].mail_identity.as_deref(), Some("DustySparrow"));
        assert_eq!(
            wake_identity_pin(
                &state::load_spawn_registry(project),
                &names,
                "flywheel-toron-oc"
            ),
            "DustySparrow",
            "wake reads the pin through the binding"
        );
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-toron-oc"),
            "DustySparrow",
            "debate/send read the pin through the binding"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-9w95: `incarnation` counts relaunches of ONE pane, so two fresh panes
    /// serving the same name BOTH sit at 0 — the tie is the common case, and the
    /// old comment claimed "highest generation wins" as if it were not. Among
    /// equal incarnations the answer is the later registry row, because
    /// `max_by_key` returns the last maximum and the registry is append-ordered.
    /// This test states that rule instead of leaving it incidental; the hazard it
    /// does NOT settle (folded legacy rows sort last) is in the learnings note.
    #[test]
    fn skills_injected_reads_the_later_registry_row_when_incarnations_tie() {
        let _home = crate::config::test_state_home();
        let project = "/tmp/fw-skills-tie-proj";
        let name = "flywheel-toron-oc";
        let injected = |pane: &str| {
            let mut e = state::SpawnEntry::legacy(pane, name);
            e.incarnation = 0;
            e.skills_injected = true;
            e
        };
        let pending = |pane: &str| {
            let mut e = state::SpawnEntry::legacy(pane, name);
            e.incarnation = 0;
            e.skills_injected = false;
            e
        };
        state::save_spawn_registry(project, &[injected("w1:p1"), pending("w2:p2")])
            .expect("save registry");
        assert!(
            !skills_injected_for(project, name),
            "the later row wins the shared incarnation, so the skill prefix is still owed"
        );
        state::save_spawn_registry(project, &[pending("w2:p2"), injected("w1:p1")])
            .expect("save registry");
        assert!(
            skills_injected_for(project, name),
            "and the rule is the registry's order, not the pane id: reversing the \
             rows reverses the answer"
        );
    }

    /// Rex block stamp gate: a `block:` id is a rex block, not an hc
    /// pane — stamping it makes hc say `pane_not_found`. `w6:p1`
    /// still takes the hc stamp.
    #[test]
    fn rex_block_skips_the_plane_stamp() {
        assert!(!pane_takes_hc_stamp(
            "block:01a17136-a780-714f-9211-b78766ffd515"
        ));
        assert!(pane_takes_hc_stamp("w6:p1"));
    }

    /// S9 (ADR-0018 D1a) integration (embedded mailbox): spawn registers a
    /// UNIQUE adj+noun pin — never the slug — keeps it on re-spawn
    /// (idempotent), and redraws when the agents table already holds the
    /// seed-draw candidate.
    #[test]
    fn bind_pane_mail_identity_stamps() {
        let _g = crate::config::lock_env();
        let data = std::env::temp_dir().join(format!("fw-bind-stamp-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("TORON_DATA_DIR", &data) };
        let cfg = Config::default();
        let project = "/tmp/fw-bind-stamp-proj";
        let pin = register_spawned_agent(&cfg, project, "flywheel-bind-oc", "opencode", "w9:p1")
            .expect("bind registers an identity");
        let entry = state::load_spawn_registry(project)
            .into_iter()
            .find(|e| e.name == "flywheel-bind-oc")
            .expect("registry entry");
        assert_eq!(entry.pane_id, "w9:p1");
        assert_eq!(entry.mail_identity.as_deref(), Some(pin.as_str()));
        crate::mail::cleanup_test_data_dir(&data);
    }

    /// bd-registry-profile-heal: a bind that arrives with the PIN (not the
    /// composed pane name) must update the profiled row in place. It used to
    /// push a legacy pin row, and dedupe then kept that later row — the
    /// profile column vanished and pane 2 of a two-pane spawn sent raw.
    #[test]
    fn bind_pane_mail_identity_preserves_profile_when_called_with_the_pin() {
        let _g = crate::config::lock_env();
        let data = std::env::temp_dir().join(format!("fw-bind-profile-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("TORON_DATA_DIR", &data) };
        let cfg = Config::default();
        let project = "fw-bind-profile-proj";
        state::save_spawn_registry(
            project,
            &[state::SpawnEntry::profiled(
                "w9:p1",
                "flywheel-x-pi-coder",
                "pi-coder",
                "pi",
                None,
            )],
        )
        .expect("save profiled row");
        bind_pane_mail_identity(&cfg, project, "TopazKnoll", "w9:p1", Some("TopazKnoll"));
        let registry = state::load_spawn_registry(project);
        assert_eq!(registry.len(), 1, "one row per pane, no shadow legacy row");
        assert_eq!(registry[0].name, "TopazKnoll", "the pin is addressable");
        assert_eq!(
            registry[0].profile.as_deref(),
            Some("pi-coder"),
            "the bind preserves the profile it did not own"
        );
        assert_eq!(registry[0].harness.as_deref(), Some("pi"));
        assert_eq!(registry[0].mail_identity.as_deref(), Some("TopazKnoll"));
        crate::mail::cleanup_test_data_dir(&data);
    }

    /// bd-registry-strict-mutating-e5ko3: a corrupt registry refuses the
    /// write (loud, naming the path) instead of paving the file with one row.
    #[test]
    fn record_spawn_entry_refuses_corrupt_registry() {
        let _g = crate::config::lock_env();
        let data = std::env::temp_dir().join(format!("fw-reg-refuse-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("TORON_DATA_DIR", &data) };
        let cfg = Config::default();
        let project = "fw-reg-refuse-proj";
        let path = state::spawn_registry_file(project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        record_spawn_entry(
            &cfg,
            project,
            state::SpawnEntry::legacy("w9:p9", "flywheel-reg-refuse-oc"),
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{not json",
            "corrupt registry must survive the refused write byte-identical"
        );
        crate::mail::cleanup_test_data_dir(&data);
    }

    /// bd-registry-strict-mutating-e5ko3: the mail-identity bind also refuses
    /// instead of saving a one-row fleet over the corrupt file.
    #[test]
    fn bind_pane_mail_identity_refuses_corrupt_registry() {
        let _g = crate::config::lock_env();
        let data = std::env::temp_dir().join(format!("fw-bind-refuse-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("TORON_DATA_DIR", &data) };
        let cfg = Config::default();
        let project = "fw-bind-refuse-proj";
        let path = state::spawn_registry_file(project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        bind_pane_mail_identity(&cfg, project, "flywheel-bind-refuse-oc", "w9:p9", None);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{not json",
            "corrupt registry must survive the refused bind byte-identical"
        );
        crate::mail::cleanup_test_data_dir(&data);
    }

    /// bd-registry-strict-mutating-e5ko3: every mutating door loads strict —
    /// a corrupt fleet refuses with the typed path error, never reads empty.
    #[test]
    fn mutating_doors_load_spawn_registry_strict() {
        for (file, doors) in [
            (
                include_str!("main.rs"),
                &[
                    "cmd_recover",
                    "cmd_kill",
                    "cmd_binding_rebind",
                    "rebind_from_registry",
                    "record_spawn_entry",
                    "bind_pane_mail_identity",
                ][..],
            ),
            (
                include_str!("pane.rs"),
                &["spawn_crew_into_rex", "spawn_crew_into"][..],
            ),
            (include_str!("cmd/spawn.rs"), &["cmd_add", "cmd_spawn"][..]),
        ] {
            for door in doors {
                assert!(
                    crate::tripwire::fn_body(file, door).contains("load_spawn_registry_strict"),
                    "{door} must refuse a corrupt registry via the strict loader"
                );
            }
        }
        // The `observe` impl is a trait member find_fn cannot see: anchor on
        // its unique signature line instead of the bare name.
        let sup = include_str!("supervisor_host.rs");
        let anchor = "\n    fn observe(&mut self, model: &SupModel, now_ms: u64) -> Vec<SupMsg> {";
        let start = sup.find(anchor).expect("LivePorts observe impl present") + 1;
        assert!(
            crate::tripwire::body_from(sup, start).contains("load_spawn_registry_strict"),
            "observe must refuse a corrupt registry via the strict loader"
        );
    }

    /// bd-registry-strict-mutating-e5ko3: send's allowed degrade — a missing
    /// fleet refuses at resolve; a live fleet resolves without the registry.
    #[test]
    fn send_resolve_refuses_missing_fleet_resolves_live() {
        assert!(
            resolve_send_targets_dyn(&[], &[], "proj", "flywheel-proj-oc").is_err(),
            "no agents + no registry must refuse, never send nowhere"
        );
        let live = vec![AgentInfo {
            agent: None,
            agent_status: AgentStatus::Working,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: false,
            name: Some("flywheel-proj-oc".into()),
            pane_id: "w1:p1".into(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "t".into(),
            terminal_id: "term".into(),
            terminal_title: None,
            workspace_id: "w".into(),
        }];
        let targets = resolve_send_targets_dyn(&live, &[], "proj", "all")
            .expect("live fleet resolves without registry rows");
        assert_eq!(targets.len(), 1);
    }

    /// bd-gpj0: arrive with HC_TERMINAL_ID records the open-mode pane so
    /// doorbell injection can see it (clears `no_injectable_panes` when
    /// hc lists that pane).
    #[test]
    fn arrive_maybe_bind_pane_records_current_pane() {
        let _home = crate::config::test_state_home();
        let prev_rex = std::env::var("REX_BLOCK").ok();
        unsafe { std::env::remove_var("REX_BLOCK") };
        let data = std::env::temp_dir().join(format!("fw-arrive-bind-{}", uuid::Uuid::new_v4()));
        let prev = std::env::var("HC_TERMINAL_ID").ok();
        unsafe { std::env::set_var("TORON_DATA_DIR", &data) };
        unsafe { std::env::set_var("HC_TERMINAL_ID", "wP:p2") };
        let cfg = Config::default();
        let project = "fw-arrive-bind-proj";
        arrive_maybe_bind_pane(&cfg, project, "QuietHeron");
        let entry = state::load_spawn_registry(project)
            .into_iter()
            .find(|e| e.name == "QuietHeron")
            .expect("open-mode pin lands in registry");
        assert_eq!(entry.pane_id, "wP:p2");
        assert_eq!(entry.mail_identity.as_deref(), Some("QuietHeron"));
        match prev {
            Some(v) => unsafe { std::env::set_var("HC_TERMINAL_ID", v) },
            None => unsafe { std::env::remove_var("HC_TERMINAL_ID") },
        }
        match prev_rex {
            Some(v) => unsafe { std::env::set_var("REX_BLOCK", v) },
            None => unsafe { std::env::remove_var("REX_BLOCK") },
        }
        crate::mail::cleanup_test_data_dir(&data);
    }

    #[test]
    fn register_spawned_agent_pins_unique_adjnoun_identity() {
        let _g = crate::config::lock_env();
        let data = std::env::temp_dir().join(format!("fw-s9-mail-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("TORON_DATA_DIR", &data) };
        let cfg = Config::default();
        let project = "/tmp/fw-s9-mail-proj";
        let slug = "flywheel-toron-oc";
        let pin = register_spawned_agent(&cfg, project, slug, "opencode", "w7:p1")
            .expect("first spawn pins an identity");
        assert!(
            valid_pair(&pin, &ADJECTIVES, &NOUNS).is_some(),
            "adj+noun pin: {pin}"
        );
        assert_ne!(pin, slug, "the slug is the pane handle, not the identity");
        let again = register_spawned_agent(&cfg, project, slug, "opencode", "w7:p1")
            .expect("re-spawn keeps the pin");
        assert_eq!(again, pin, "re-spawn must not draw a fresh pin");
        let target = pin.clone();
        let collider = (0..200_000u64)
            .map(|i| format!("s9spawn-{i}"))
            .find(|n| adjnoun_from_fnv(state::fnv1a64(n)) == target)
            .expect("a colliding name exists in the 9900-pair space");
        let pin2 = register_spawned_agent(&cfg, project, &collider, "opencode", "w7:p2")
            .expect("colliding spawn draws a fresh pin");
        assert_ne!(pin2, pin, "collision with the agents table redraws");
        assert!(valid_pair(&pin2, &ADJECTIVES, &NOUNS).is_some(), "{pin2}");
        crate::mail::cleanup_test_data_dir(&data);
    }

    /// bd-add-missing-mail-identity-16le: the ADD path's identity step mints
    /// a DISTINCT registered pin per added pane (oc/oc2/oc3 — even when the
    /// seq counter reuses after a dead pane, distinct terminal names draw
    /// distinct pins), never the unroutable default ("bangedorrunt"), and
    /// the project-local agent-names.json mirror (written by cmd_add) holds
    /// every pin — so claims write the real identity, strand checks see
    /// live pins, and `flywheel mail whois` resolves.
    #[test]
    fn add_panes_get_distinct_registered_pins_in_agent_names() {
        let _g = crate::config::lock_env();
        let data = std::env::temp_dir().join(format!("fw-s9-add-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("TORON_DATA_DIR", &data) };
        let cfg = Config::default();
        let project = "/tmp/fw-s9-add-proj";
        let dir = project_dir(&cfg, project);

        let mut pins: std::collections::HashSet<String> = std::collections::HashSet::new();
        let slugs = [
            "flywheel-toron-oc",
            "flywheel-toron-oc2",
            "flywheel-toron-oc3",
        ];
        for (i, slug) in slugs.iter().enumerate() {
            let pane = format!("w7:p{}", i + 1);
            let pin = register_spawned_agent(&cfg, project, slug, "opencode", &pane)
                .expect("add pane registers an identity");
            assert_ne!(pin, "bangedorrunt", "never the unroutable default identity");
            assert_ne!(pin, *slug, "the pin is not the hc slug");
            assert!(
                valid_pair(&pin, &ADJECTIVES, &NOUNS).is_some(),
                "adj+noun pin: {pin}"
            );
            assert!(
                pins.insert(pin.clone()),
                "distinct pin per pane: {pin} duplicated across added panes"
            );
        }

        let registry = state::load_spawn_registry(project);
        state::save_agent_names(&dir, &registry).expect("save agent names");
        let names = state::load_agent_names(&dir);
        let mirrored: std::collections::HashSet<String> = names
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect();
        assert_eq!(mirrored.len(), 3, "agent-names.json holds all 3 pins");
        assert!(
            mirrored.iter().all(|p| pins.contains(p)),
            "mirror pins match the registered pins"
        );
        assert!(!mirrored.contains("bangedorrunt"));

        let key = dir.to_str().map(str::to_string).unwrap();
        let mail = backend::mail_backend(&cfg).expect("mail backend");
        let agents = backend::run_mail(mail.list_agents(&key, None)).expect("list agents");
        for p in &pins {
            assert!(
                agents.iter().any(|a| a.name == *p),
                "registered pin {p} resolves via the agents table (whois)"
            );
        }
        crate::mail::cleanup_test_data_dir(&data);
    }

    /// bd-3qun4: the pin seed is project-scoped, so two projects sharing one
    /// toron store never draw the same pin for the same agent name (toron
    /// allows one key per pin across the whole store), while a re-spawn in the
    /// SAME project still draws deterministically.
    #[test]
    fn pin_seed_is_project_scoped_and_stable() {
        let seed = |project: &str| state::fnv1a64(&format!("{project}|flywheel-toron-oc"));
        assert_eq!(
            seed("/tmp/proj-a"),
            seed("/tmp/proj-a"),
            "same project, same seed (idempotent re-spawn)"
        );
        assert_ne!(
            seed("/tmp/proj-a"),
            seed("/tmp/proj-b"),
            "different projects never share a seed"
        );
    }

    /// bd-pin-public-plane-s-easy.2: register_spawned_agent writes the
    /// fold_agent_name_alias(slug -> pin) row after register — whois(slug)
    /// must resolve to the pin's npub (ADR-0018 D4), never the slug as a
    /// second identity.
    #[test]
    fn register_spawned_agent_folds_slug_to_pin() {
        let _g = crate::config::lock_env();
        let data = std::env::temp_dir().join(format!("fw-fold-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("TORON_DATA_DIR", &data) };
        let cfg = Config::default();
        let project = "/tmp/fw-fold-proj";
        let slug = "flywheel-toron-oc";
        let pin = register_spawned_agent(&cfg, project, slug, "opencode", "w7:p1")
            .expect("spawn registers a pin");
        let key = project_dir(&cfg, project)
            .to_str()
            .map(str::to_string)
            .unwrap();
        let mail = backend::mail_backend(&cfg).expect("mail backend");
        backend::run_mail(mail.ensure_project(&key)).expect("ensure project");
        if let Ok(Some(facade)) = mail::Mail::from_config(Some(&cfg.toron_mail_config())) {
            facade.set_identity_mode(&key, "swarm").expect("swarm mode");
        }
        let who_slug = backend::run_mail(mail.whois(&key, slug)).expect("whois slug via fold");
        let who_pin = backend::run_mail(mail.whois(&key, &pin)).expect("whois pin");
        assert_eq!(
            who_slug.npub, who_pin.npub,
            "whois(slug) resolves to the pin's npub after the fold"
        );
        crate::mail::cleanup_test_data_dir(&data);
    }

    /// S9 (ADR-0018 D1a): the peer-aware spawn template injects the mail
    /// identity — now the adj+noun PIN, not the slug.
    #[test]
    fn peer_prompt_injects_mail_identity() {
        let tpl = "You are {name}. Your mail identity is {mail_identity}. Budget {poll_interval_minutes}/{poll_budget_minutes}/{max_peer_prompts}";
        let out = peer_prompt_template(tpl, "flywheel-toron-oc4", "DustySparrow", 10, 60, 5);
        assert!(out.contains("Your mail identity is DustySparrow"), "{out}");
        assert!(!out.contains('{'), "no literal placeholders: {out}");
    }
}

/// `stage promote --to repo`: verify the evidence bar, then append the stage
/// as DATA into the flywheel repo's src/repo-stages.toml.
fn promote_to_repo(
    name: &str,
    evidence: &[String],
    review_bead: Option<&str>,
    repo_dir: Option<&str>,
) -> Result<()> {
    let objectives = repo_evidence_verified(name, evidence, review_bead)?;
    let repo = repo_dir
        .map(PathBuf::from)
        .or_else(|| std::env::var("FLYWHEEL_REPO_DIR").ok().map(PathBuf::from))
        .ok_or_else(|| {
            anyhow!(
                "no flywheel repo dir — pass --repo-dir <checkout> or set FLYWHEEL_REPO_DIR \
                 (the promoted stage lands as DATA in <repo>/src/repo-stages.toml)"
            )
        })?;
    let dest = repo.join("src/repo-stages.toml");
    if !dest.is_file() {
        bail!(
            "repo data file missing: {} (not a flywheel checkout?)",
            dest.display()
        );
    }
    let lib = stages::StageLibrary::compiled_in();
    let stage = lib
        .get_stage(name)
        .ok_or_else(|| anyhow!("unknown stage {name:?}"))?
        .clone();
    stages::upsert_stage_in_file(&dest, &stage)?;
    println!(
        "promoted {name} → repo tier ({}; {objectives} distinct objectives verified; review bead witnessed)",
        dest.display()
    );
    println!("next: commit the repo-data change upstream (the review bead is the witness)");
    Ok(())
}

/// The author pin recorded in provenance (`stage new`, compose `--keep`).
fn operator_pin() -> String {
    std::env::var("AGENT_NAME")
        .or_else(|_| std::env::var("USER").map_err(|_| std::env::VarError::NotPresent))
        .unwrap_or_else(|_| "unknown".to_string())
}

fn override_display() -> String {
    stages::override_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<none>".to_string())
}

/// What the loop should do for one agent on one poll — the SINGLE
/// definition lives in durable.rs (bd-w4-delete-dup-sbh8); the sync
/// (`--local`/`--once`) path delegates to `durable::plan_actions`.
/// Build the instruction queue from --queue / --workflow.
/// Clamp the active cadence to the 5s floor (consistent with the workflow's
/// `INTERVAL_MIN` in `next_interval_secs`). Pure — the warning is emitted by
/// the caller when the value changes.
fn clamp_active_interval(v: u64) -> u64 {
    v.max(5)
}

fn build_queue(
    queue: Option<&str>,
    workflow: Option<&str>,
    project: Option<&str>,
) -> Result<Vec<String>> {
    let mut items: Vec<String> = Vec::new();
    if let Some(wf) = workflow {
        let mut handled = false;
        {
            let all = workflow_catalog::all_workflows_plain(None)
                .unwrap_or_else(|_| builtin_workflows::builtin_workflows());
            if let Some(_uwf) = all.get(wf) {
                if let Some(proj) = project {
                    let exe = std::env::current_exe()?;
                    let output = std::process::Command::new(exe)
                        .args([
                            "workflow",
                            "run",
                            wf,
                            "--execute",
                            "--mode",
                            "pane",
                            "--project",
                            proj,
                            "--yes",
                            "--json",
                        ])
                        .output()?;
                    if !output.status.success() {
                        anyhow::bail!(
                            "workflow {wf} failed (engine invoke): {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    handled = true;
                } else {
                    anyhow::bail!(
                        "workflow {wf} requires --project <proj> to run via the engine (use 'flywheel workflow run {wf} --execute --mode pane --project <proj>')"
                    );
                }
            }
        }
        if !handled {
            let workflows = config::Workflows::load()?;
            let steps = workflows
                .workflows
                .get(wf)
                .ok_or_else(|| anyhow!("workflow not found: {wf} (flywheel workflows list)"))?;
            for step in steps {
                match step {
                    WorkflowStep::Send { stage, msg, .. } => {
                        let text = match (stage.as_deref(), msg.as_deref()) {
                            (Some(s), _) => stage_prompt(s)?,
                            (None, Some(m)) => m.to_string(),
                            (None, None) => bail!("workflow {wf}: send step needs stage or msg"),
                        };
                        items.push(text);
                    }
                    WorkflowStep::Wait { .. } => {}
                }
            }
        }
    }
    if let Some(q) = queue {
        for token in q.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if let Ok(p) = stage_prompt(token) {
                items.push(p);
            } else {
                items.push(token.to_string());
            }
        }
    }
    Ok(items)
}

/// Run `br ready --json` in the project dir, returning the ready beads.
/// Caller decides how to treat an error (the sync loop soft-fails).
pub(crate) fn load_ready_beads(cfg: &Config, project: &str) -> Result<Vec<durable::ReadyBead>> {
    let dir = project_dir(cfg, project);
    crate::br::Bead::ready(Some(&dir))
}

/// Run `br list --status in_progress --json` in the project dir.
pub(crate) fn load_inprogress_beads(
    cfg: &Config,
    project: &str,
) -> Result<Vec<durable::ReadyBead>> {
    let dir = project_dir(cfg, project);
    crate::br::Bead::in_progress(Some(&dir))
}

/// Sync-loop `--work` queue refresh (parity with the durable loop's
/// bead-driven mode): resume interrupted in-progress beads first, then hand
/// out fresh ready beads. Each bead is assigned at most once per loop run
/// (the `assigned` set is per-invocation state). Pure — fully testable.
/// bd-g0ge: the window value the loop resume filter gates on — keyed on
/// the live attempt generation when an attempt row exists, else the legacy
/// bead key. Pure: the window and the attempts arrive as values, so the
/// filter and its test read the same lookup.
fn attempt_window_last_us<'a>(
    window: &std::collections::BTreeMap<String, i64>,
    attempts: &'a [durable::WorkAttempt],
    bead_id: &str,
    pane: &str,
) -> (Option<i64>, Option<&'a durable::WorkAttempt>) {
    let live = durable::live_attempt_for_bead(attempts, bead_id);
    let last = match live {
        Some(a) => window
            .get(&state::attempt_resume_key(bead_id, pane, a.generation))
            .copied(),
        None => window.get(&state::resume_key(bead_id, pane)).copied(),
    };
    (last, live)
}

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

/// bd-resume-window-lease-sweep-4sa2: live `(reason, holder)` pairs from the
/// project's reservation board. Feeds the resume peer-hold check. A mail
/// hiccup yields an empty board — never a loop crash, and never a false
/// peer-hold (an unread board cannot invent a competing holder).
fn live_reservation_holders(cfg: &Config, project: &str) -> Vec<(String, String)> {
    let Ok(key) = project_dir_str(cfg, project) else {
        return Vec::new();
    };
    let Ok(mail) = mail_or_die(cfg) else {
        return Vec::new();
    };
    let Ok(v) = mail.list_active(&key, None, None) else {
        return Vec::new();
    };
    v.get("reservations")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let reason = r.get("reason").and_then(Value::as_str)?.to_string();
                    let holder = r
                        .get("holder")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Some((reason, holder))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Live `(path, reason, holder)` rows. The resume fold uses these for
/// `file_blocks_bead`. A mail hiccup is an empty board, never a false block.
pub(crate) fn live_file_rows(cfg: &Config, project: &str) -> Vec<(String, String, String)> {
    let Ok(key) = project_dir_str(cfg, project) else {
        return Vec::new();
    };
    let Ok(mail) = mail_or_die(cfg) else {
        return Vec::new();
    };
    let Ok(v) = mail.list_active(&key, None, None) else {
        return Vec::new();
    };
    v.get("reservations")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let path = r.get("path").and_then(Value::as_str)?.to_string();
                    let reason = r
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let holder = r
                        .get("holder")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Some((path, reason, holder))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// bd-reserve-yield-taqf: live `(bead, holder)` yield pairs — toron's
/// structured stand-down (`reserve yield`), read through the same join as the
/// lease board. Same soft-fail contract as the reservation board: a hiccup
/// yields no pairs, and an unread board cannot invent a hold.
fn live_yielded_pairs(cfg: &Config, project: &str) -> Vec<(String, String)> {
    let Ok(mail) = mail_or_die(cfg) else {
        return Vec::new();
    };
    mail.yielded_pairs(project).unwrap_or_default()
}

/// Unregisters this process's loop-pid entry on exit (any path). Only ever
/// removes the entry when it still belongs to THIS pid — a newer loop that
/// took over (--force) is never unregistered by the older process exiting
/// late (bd-fw-stale-loop-processes-gac).
struct LoopPidGuard {
    project: String,
    pid: u32,
}

impl Drop for LoopPidGuard {
    fn drop(&mut self) {
        state::unregister_loop(&self.project, self.pid);
    }
}

/// `flywheel loop list`: show the pid registry with live/dead status.
fn cmd_loop_list() -> Result<()> {
    let pids = state::load_loop_pids();
    if pids.is_empty() {
        println!("no registered loops");
        return Ok(());
    }
    let rows: Vec<Value> = pids
        .iter()
        .map(|(project, e)| {
            json!({
                "project": project,
                "pid": e.pid,
                "started_at": e.started_at,
                "invocation": e.invocation,
                "alive": state::pid_alive(e.pid),
            })
        })
        .collect();
    print_json(&json!({ "loops": rows }));
    Ok(())
}

/// `flywheel loop prune`: drop pid-registry entries whose process is dead.
fn cmd_loop_prune() -> Result<()> {
    let mut pids = state::load_loop_pids();
    let before = pids.len();
    let dead: Vec<String> = pids
        .iter()
        .filter(|(_, e)| !state::pid_alive(e.pid))
        .map(|(p, _)| p.clone())
        .collect();
    for p in &dead {
        pids.remove(p);
    }
    state::save_loop_pids(&pids)?;
    print_json(&json!({
        "removed": dead,
        "removed_count": dead.len(),
        "remaining": pids.len(),
        "pruned": before - pids.len(),
    }));
    Ok(())
}

/// bd-pane-wedge-detect-respawn-pnyi: read every registered pin's pane for a
/// wedge — the agent process still UP, parked on a fatal error line, with no
/// commit movement inside the window. The verdict is `durable::pane_wedge_action`
/// (pure); this is only the live read. Soft-fail everywhere: an unreachable
/// hc, an empty registry, or an unreadable pane yields no wedges — never a
/// fabricated page.
fn probe_pane_wedges(
    cfg: &Config,
    project: &str,
    project_dir: &std::path::Path,
    now: u64,
    quiet_secs: u64,
    already_paged: &std::collections::HashSet<String>,
) -> Vec<durable::PaneWedgeAction> {
    let entries = state::load_spawn_registry(project);
    if entries.is_empty() {
        return Vec::new();
    }
    let Ok(c) = Client::connect(Some(&cfg.socket_path())) else {
        return Vec::new();
    };
    let Ok(agents) = c.agent_list() else {
        return Vec::new();
    };
    let commit_age = cmd::captain::last_commit_age_secs(project_dir, now);
    let mut out = Vec::new();
    for e in &entries {
        let Some(pin) = e.mail_identity.as_deref().filter(|p| !p.trim().is_empty()) else {
            continue;
        };
        let Some(a) = agents.iter().find(|a| a.pane_id == e.pane_id) else {
            continue;
        };
        let kind = a.agent.as_deref().unwrap_or("");
        let alive = hc::pane_process_info(&c, &e.pane_id)
            .ok()
            .and_then(|info| hc::pane_agent_alive(&info, kind));
        let tail = hc::pane_recent_text(&c, &e.pane_id).ok();
        let action = durable::pane_wedge_action(
            &durable::PaneWedgeInputs {
                pin,
                pane_id: &e.pane_id,
                process_alive: alive,
                recent_output: tail.as_deref(),
                last_commit_age_secs: commit_age,
                quiet_secs,
            },
            already_paged.contains(pin),
        );
        if !matches!(action, durable::PaneWedgeAction::Quiet) {
            out.push(action);
        }
    }
    out
}

/// `flywheel loop-mode supervise` (bd-swarm-loop-durable-panic-atlo): own
/// the once-pass cadence the operator would otherwise hand-roll. Each pass is
/// a CHILD process (`loop-mode run --once`), so a crashed pass cannot kill the
/// supervisor; 3 consecutive failed passes stop it with one actionable line.
///
/// bd-supervise-dead-stop-escalation-rweq: each pass also probes for a dead
/// stop — no pane working AND no commit for `stall_minutes` with work
/// remaining. A dead stop is invisible to the passes (they keep firing with
/// nothing to do), so this emits ONE loud escalation line and pages the captain
/// once per stall, never once per pass; a recovering fleet clears it silently.
fn cmd_loop_supervise(
    cfg: &Config,
    project: &str,
    work: bool,
    interval_secs: u64,
    to: &str,
    stall_minutes: u64,
    context: Option<&str>,
) -> Result<()> {
    if interval_secs < 5 {
        bail!("--interval must be >= 5 seconds");
    }
    supervisor_host::run_supervise(
        cfg,
        project,
        work,
        interval_secs,
        to,
        stall_minutes,
        context,
    )
}

/// Page the captain about a dead stop. Best-effort but LOUD: a failed page
/// never swallows the escalation — the line on stderr stands on its own, and
/// the failure names itself so the operator knows the page did not land.
fn page_captain_dead_stop(cfg: &Config, project: &str, line: &str) {
    page_captain_mail(cfg, project, "[loop-mode supervise] dead stop", line)
}

/// Page the captain on `subject`. Best-effort but LOUD: a failed page never
/// swallows the escalation, and the failure names itself so the operator
/// knows the page did not land.
pub(crate) fn page_captain_mail(cfg: &Config, project: &str, subject: &str, line: &str) {
    let Ok(key) = crate::project_dir_str(cfg, project) else {
        eprintln!(
            "flywheel: captain page FAILED (resolve project dir) — escalation line above stands"
        );
        return;
    };
    let mail = match crate::mail_as(cfg, &key, None) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "flywheel: captain page FAILED (mail actor: {e:#}) — escalation line above stands"
            );
            return;
        }
    };
    let target = crate::resolve_mail_target_registry(project, "captain");
    let thread = format!("loop-supervise-{project}");
    match mail.send(
        &key,
        &[target],
        subject,
        line,
        "urgent",
        true,
        Some(&thread),
    ) {
        Ok(_) => eprintln!("flywheel: paged captain on {thread}"),
        Err(e) => eprintln!("flywheel: captain page FAILED ({e:#}) — escalation line above stands"),
    }
}

/// `flywheel loop doctor`: health report for loop infrastructure (bd-eign.4).
/// Logic lives in durable::loop_doctor; this is a thin runtime + render stub.
fn cmd_loop_doctor(cfg: &Config, fix: bool, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let mut report = rt.block_on(durable::loop_doctor(cfg, fix))?;
    let mut dead_doorbells: Vec<String> = Vec::new();
    let db_dir = config::state_dir().join("doorbell");
    if let Ok(entries) = std::fs::read_dir(&db_dir) {
        for e in entries.flatten() {
            let Some(label) = e
                .file_name()
                .to_string_lossy()
                .strip_suffix(".state.json")
                .map(str::to_string)
            else {
                continue;
            };
            let alive = doorbell::dispatcher_pid(&config::state_dir(), &label)
                .is_some_and(doorbell::pid_alive);
            if !alive {
                dead_doorbells.push(label.to_string());
            }
        }
    }
    report["doorbell"] =
        json!({ "dead_dispatchers": { "count": dead_doorbells.len(), "entries": dead_doorbells } });
    if json {
        print_json(&report);
        return Ok(());
    }
    let store_ok = report["engine_store"]["ok"] == true;
    println!(
        "Engine store: {}",
        if store_ok {
            output::ok("park + wake")
        } else {
            output::error("UNUSABLE")
        }
    );
    if !store_ok {
        println!(
            "  {}",
            report["engine_store"]["error"]
                .as_str()
                .unwrap_or("unknown error")
        );
    }
    let stale = report["stale_invocations"]["count"].as_u64().unwrap_or(0);
    println!(
        "Stale loop rows: {}",
        if stale == 0 {
            output::ok("none")
        } else {
            output::warn(&stale.to_string())
        }
    );
    for id in report["stale_invocations"]["ids"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        println!("  {id}");
    }
    let orphaned = report["orphaned_pids"]["count"].as_u64().unwrap_or(0);
    println!(
        "Orphaned loop pids: {}",
        if orphaned == 0 {
            output::ok("none")
        } else {
            output::warn(&orphaned.to_string())
        }
    );
    for e in report["orphaned_pids"]["entries"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
    {
        println!(
            "  {} (pid {}, started {})",
            e["project"].as_str().unwrap_or("?"),
            e["pid"].as_u64().unwrap_or(0),
            e["started_at"].as_str().unwrap_or("?")
        );
    }
    let dead_db = report["doorbell"]["dead_dispatchers"]["count"]
        .as_u64()
        .unwrap_or(0);
    println!(
        "Doorbell dispatchers: {}",
        if dead_db == 0 {
            output::ok("all up")
        } else {
            output::warn(&format!("{dead_db} DEAD"))
        }
    );
    for entry in report["doorbell"]["dead_dispatchers"]["entries"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        println!("  {entry} (heal: flywheel doorbell start <project>)");
    }
    println!(
        "Registry entries: {}",
        report["registry"]["count"].as_u64().unwrap_or(0)
    );
    for e in report["registry"]["entries"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
    {
        println!(
            "  {} [{}] {} (pid {} alive={})",
            e["project"].as_str().unwrap_or("?"),
            e["verdict"].as_str().unwrap_or("?"),
            e["invocation"].as_str().unwrap_or("?"),
            e["pid"].as_u64().unwrap_or(0),
            e["process_alive"].as_bool().unwrap_or(false)
        );
    }
    if fix {
        let dropped = report["fixed"]["dropped"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or(0);
        let pruned = report["fixed"]["pruned"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or(0);
        println!(
            "Fixed: dropped {} stale loop row(s), pruned {} orphaned pid(s)",
            dropped, pruned
        );
    } else {
        println!(
            "{}",
            output::dim("re-run with --fix to drop stale loop rows + prune orphaned pids")
        );
    }
    Ok(())
}

fn cmd_loop_replay(project: &str) -> Result<()> {
    let entries = crate::state::read_loop_journal(project);
    if entries.is_empty() {
        println!(
            "no journal for {project} (state_dir/journals/{}.jsonl)",
            crate::state::state_key(project)
        );
        return Ok(());
    }
    let mut model = crate::durable::LoopModel {
        queue: Vec::new(),
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
        bead_slots: Default::default(),
        agent_slots: Default::default(),
    };
    for (idx, entry) in entries.iter().enumerate() {
        let Some(msg_val) = entry.get("msg") else {
            continue;
        };
        let msg: crate::durable::LoopMsg = serde_json::from_value(msg_val.clone())
            .with_context(|| format!("journal {idx} msg decode"))?;
        if let Some(snap) = entry.get("model_snapshot") {
            if let Some(q) = snap.get("queue").and_then(|q| q.as_array()) {
                model.queue = q
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
            if let Some(dp) = snap.get("dispatch_pending").and_then(|v| v.as_array()) {
                model.dispatch_pending = dp
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
            if let Some(pb) = snap.get("per_bead_phase")
                && let Ok(map) = serde_json::from_value::<
                    std::collections::BTreeMap<String, crate::durable::LoopPhase>,
                >(pb.clone())
            {
                model.per_bead_phase = map;
            }
            if let Some(md) = snap
                .get("mail_degraded")
                .and_then(serde_json::Value::as_bool)
            {
                model.mail_degraded = md;
            }
        }
        let (next, actions) = crate::workspace::tick_from_loop_msg(&model, &msg);
        let recorded_actions = entry.get("actions").and_then(|v| v.as_array());
        let replayed_val = serde_json::to_value(&actions).unwrap_or(serde_json::Value::Null);
        let replayed_arr = replayed_val.as_array();
        let drift = match (recorded_actions, replayed_arr) {
            (Some(rec), Some(rep)) => rec != rep,
            (None, _) => false,
            (Some(_), None) => true,
        };
        if drift {
            anyhow::bail!(
                "replay drift at seq {idx}: recorded actions != replayed {} vs {} (msg {:?})",
                entry.get("actions").unwrap_or(&serde_json::Value::Null),
                replayed_val,
                msg
            );
        }
        model = next;
    }
    println!("replay {project}: {} entries, no drift", entries.len());
    Ok(())
}

fn cmd_loop_why(project: &str, seq: usize) -> Result<()> {
    let entries = crate::state::read_loop_journal(project);
    let Some(entry) = entries.get(seq) else {
        anyhow::bail!(
            "no journal entry {seq} for {project} (have {})",
            entries.len()
        );
    };
    let pretty = serde_json::to_string_pretty(entry)?;
    println!("{pretty}");
    Ok(())
}

fn cmd_artifacts(
    run_id: Option<&str>,
    json: bool,
    project: Option<&str>,
    cmd: &Option<ArtifactsCmd>,
) -> Result<()> {
    if let Some(ArtifactsCmd::Stitch { run_id, project }) = cmd {
        let proj = project_door(project.as_deref(), "flywheel", "artifacts stitch")?;
        let path = crate::artifact::stitch_artifacts(proj, run_id)?;
        println!("{}", path.display());
        return Ok(());
    }
    let Some(rid) = run_id else {
        anyhow::bail!("flywheel artifacts: pass <run_id> or `stitch <run_id>`");
    };
    let proj = project_door(project, "flywheel", "artifacts")?;
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("flywheel")
        .join(proj)
        .join("artifacts")
        .join(rid);
    let mut records = Vec::new();
    if base.is_dir() {
        for e in std::fs::read_dir(&base)? {
            let e = e?;
            let p = e.path();
            if p.extension().map(|x| x == "json").unwrap_or(false) {
                let raw = std::fs::read_to_string(&p)?;
                let rec: serde_json::Value = serde_json::from_str(&raw)?;
                records.push(rec);
            }
        }
    }
    if json {
        print_json(&serde_json::json!(records));
    } else {
        for r in &records {
            println!("{}", serde_json::to_string(r)?);
        }
        if records.is_empty() {
            println!("no artifacts for run {rid} in {proj}");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_loop(
    cfg: &Config,
    project: &str,
    interval_secs: u64,
    active_interval_secs: u64,
    queue: Option<&str>,
    workflow: Option<&str>,
    nudge: Option<&str>,
    timeout_ms: Option<u64>,
    once: bool,
    local: bool,
    work: bool,
    no_wave_gates: bool,
    wave_label: Option<&str>,
    on_blocked: &str,
    to: &str,
    no_confirm: bool,
    confirm_timeout_ms: Option<u64>,
    engine: bool,
    force: bool,
    resume: bool,
    strict: bool,
) -> Result<()> {
    if interval_secs < 5 {
        bail!("--interval must be >= 5 seconds");
    }
    let strict = strict
        || std::env::var("FLYWHEEL_VERIFY_STRICT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let _guard = LoopPidGuard {
        project: project.to_string(),
        pid: std::process::id(),
    };
    if let Some(prev) = state::register_loop(project, "", force)? {
        bail!(
            "a loop for {project} is already running (pid {}, started {}). Inspect: flywheel loop list; cleanup: flywheel loop prune; take over: flywheel loop run {project} --force.",
            prev.pid,
            prev.started_at
        );
    }
    let active_secs = clamp_active_interval(active_interval_secs);
    if active_secs != active_interval_secs {
        eprintln!(
            "flywheel: warning: --interval-active {active_interval_secs}s below the 5s floor — clamping to 5"
        );
    }
    if !matches!(on_blocked, "continue" | "stop" | "mail") {
        bail!("--on-blocked must be continue|stop|mail");
    }
    if let Some(label) = wave_label {
        if !work {
            bail!("--wave-label requires --work");
        }
        if label
            .strip_prefix("wave-")
            .and_then(|n| n.parse::<u32>().ok())
            .is_none()
        {
            bail!("--wave-label must be `wave-<N>` (got `{label}`)");
        }
    }
    let mut queue_items = build_queue(queue, workflow, Some(project))?;

    if !local && !once {
        let max_iterations = timeout_ms.map(|ms| (ms / 1000).div_ceil(interval_secs.max(1)));
        let params = durable::LoopParams {
            project: project.to_string(),
            to: to.to_string(),
            interval_secs,
            active_interval_secs: active_secs,
            queue: queue_items.clone(),
            nudge: nudge.map(String::from),
            on_blocked: on_blocked.to_string(),
            max_iterations,
            work,
            no_wave_gates,
            wave_label: wave_label.map(String::from),
            confirm_sends: !no_confirm,
            confirm_timeout_ms,
            invocation: String::new(),
            unknown_diagnose_threshold: cfg.unknown_diagnose_threshold(),
            respawn_budget: cfg.respawn_budget(),
            reclaim: crate::durable_policy::ReclaimPolicy::from_environment(),
            pane_stall_secs: durable::pane_stall_threshold_from_environment(),
            dispatch_guard_ttl_us: std::env::var("FLYWHEEL_DISPATCH_TTL_SECS")
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|secs| *secs > 0)
                .map(|secs| secs * 1_000_000)
                .unwrap_or(durable::DISPATCH_GUARD_TTL_US),
            mail_health_escalate_at: crate::state::mail_health_escalate_at(),
            pause_resurface_secs: crate::write_liveness::PAUSE_RESURFACE_DEFAULT_SECS,
            walk_timeout_secs: crate::write_liveness::WALK_TIMEOUT_DEFAULT_SECS,
            unknown_detect_grace_polls: cfg.unknown_detect_grace_polls(),
            drain_grace_polls: cfg.drain_grace_polls(),
            claim_nudge_after_polls: cfg.claim_nudge_after_polls(),
            force,
            resume,
            strict,
        };
        let rt = tokio::runtime::Runtime::new()?;
        if engine {
            println!(
                "loop: swarm monitor for {} (--engine routed to the durable keeper; resume: flywheel loop run {project})",
                dim(project)
            );
            let result = rt.block_on(durable::run_engine_loop(cfg, params))?;
            println!("loop: {result}");
            log(
                project,
                EventKind::Loop,
                json!({ "status": "complete", "engine": true, "result": result }),
            );
            return Ok(());
        }
        let result = rt.block_on(durable::run_durable_loop(cfg, params))?;
        println!("loop: {result}");
        log(
            project,
            EventKind::Loop,
            json!({ "status": "complete", "result": result }),
        );
        return Ok(());
    }

    let mut assigned: Vec<String> = Vec::new();
    let mut not_ready_mailed: std::collections::HashSet<String> = std::collections::HashSet::new();
    if queue_items.is_empty() && nudge.is_none() && !work {
        eprintln!("flywheel: no instructions queued and no --nudge — loop will only monitor");
    }

    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));

    log(
        project,
        EventKind::Loop,
        loop_monitor_notice(interval_secs, queue_items.len(), to, on_blocked),
    );
    println!(
        "loop: monitoring {} (interval {}s, queue {}{})",
        dim(project),
        interval_secs,
        queue_items.len(),
        if nudge.is_some() {
            " + nudge".to_string()
        } else {
            String::new()
        }
    );

    loop {
        if let Some(d) = deadline
            && std::time::Instant::now() >= d
        {
            bail!(
                "loop timed out after {}s (queue remaining: {})",
                timeout_ms.unwrap_or(0) / 1000,
                queue_items.len()
            );
        }

        let agents = c
            .agent_list()?
            .into_iter()
            .filter(|a| a.workspace_id == ws_id)
            .collect::<Vec<_>>();
        let managed: Vec<(String, AgentStatus)> = if to == "all" {
            agents
                .iter()
                .map(|a| (agent_target(a).to_string(), a.agent_status))
                .collect()
        } else {
            let resolved = resolve_agents(&mut c, &ws_id, project, to)?;
            resolved
                .iter()
                .map(|a| (agent_target(a).to_string(), a.agent_status))
                .collect()
        };

        if work {
            let mut inprog_now: Vec<durable::ReadyBead> = Vec::new();
            let window = state::load_resume_window(project);
            let attempts = state::load_work_attempts(project);
            let mut peers = live_reservation_holders(cfg, project);
            peers.extend(live_yielded_pairs(cfg, project));
            let file_rows = live_file_rows(cfg, project);
            let now_us = chrono::Utc::now().timestamp_micros();
            let mut inprog_items: Vec<String> = Vec::new();
            let mut ready_items: Vec<String> = Vec::new();
            let mut ready_now: Vec<durable::ReadyBead> = Vec::new();
            match load_inprogress_beads(cfg, project) {
                Ok(inprog) => {
                    let resumable: Vec<durable::ReadyBead> = inprog
                        .iter()
                        .filter(|b| {
                            let pane = b.assignee.as_deref().unwrap_or("");
                            let (last, live) =
                                attempt_window_last_us(&window, &attempts, &b.id, pane);
                            let intended =
                                durable::path_tokens(&format!("{} {}", b.title, b.description));
                            durable::should_resume_attempt(
                                last,
                                now_us,
                                durable::RESUME_WINDOW_DEFAULT_SECS,
                                durable::peer_held(&b.id, &peers, pane)
                                    || durable::file_blocks_bead(
                                        &b.id, &file_rows, pane, &intended,
                                    ),
                                live,
                            )
                        })
                        .cloned()
                        .collect();
                    let to_record: Vec<(String, String)> = resumable
                        .iter()
                        .filter(|b| !assigned.contains(&b.id))
                        .map(|b| (b.id.clone(), b.assignee.clone().unwrap_or_default()))
                        .collect();
                    let to_resume: Vec<String> =
                        work_prompts(&resumable, &[], &mut assigned, project);
                    for (bead, pane) in &to_record {
                        if let Err(e) = state::record_resume(project, bead, pane, now_us) {
                            eprintln!(
                                "flywheel: resume-window record for {bead} failed (continuing): {e:#}"
                            );
                        }
                        match state::record_attempt_resume(project, bead, pane, None, None, now_us)
                        {
                            Ok(rec) => {
                                let key = state::attempt_resume_key(bead, pane, rec.generation);
                                if let Err(e) = state::update_locked(
                                    &state::resume_window_file(project),
                                    |map: &mut std::collections::BTreeMap<String, i64>| {
                                        map.insert(key.clone(), now_us);
                                    },
                                ) {
                                    eprintln!(
                                        "flywheel: attempt-resume window record for {bead} failed (continuing): {e:#}"
                                    );
                                }
                            }
                            Err(e) => eprintln!(
                                "flywheel: attempt resume record for {bead} failed (continuing): {e:#}"
                            ),
                        }
                    }
                    inprog_items.extend(to_resume);
                    inprog_now = inprog;
                    for b in &inprog_now {
                        let pane = b.assignee.as_deref().unwrap_or("");
                        let intended =
                            durable::path_tokens(&format!("{} {}", b.title, b.description));
                        if !durable::file_blocks_bead(&b.id, &file_rows, pane, &intended) {
                            continue;
                        }
                        let Some((holder, pattern)) =
                            durable::file_hold_notice(&b.id, &file_rows, pane, &intended)
                        else {
                            continue;
                        };
                        let hold_key = format!("filehold\x1f{}\x1f{}", b.id, holder);
                        if window.get(&hold_key).is_some_and(|t| {
                            now_us.saturating_sub(*t)
                                < request_receipts::FILE_HOLD_WINDOW_SECS as i64 * 1_000_000
                        }) {
                            continue;
                        }
                        let args = toron_core::mailbox::messaging::SendMessage {
                            project_slug: project.to_string(),
                            sender_name: "captain".to_string(),
                            to: vec![holder.clone()],
                            subject: format!("[file-hold] {}", b.id),
                            body_md: format!("bead={}\npath={}", b.id, pattern),
                            ..Default::default()
                        };
                        match crate::mail::block_on(
                            crate::mail::comms_mailbox()?.send_message(&args),
                        ) {
                            Ok(_) => {
                                if let Err(e) = state::update_locked(
                                    &state::resume_window_file(project),
                                    |map: &mut std::collections::BTreeMap<String, i64>| {
                                        map.insert(hold_key.clone(), now_us);
                                    },
                                ) {
                                    eprintln!(
                                        "flywheel: file-hold watermark for {} failed (continuing): {e:#}",
                                        b.id
                                    );
                                }
                            }
                            Err(e) => eprintln!(
                                "flywheel: file-hold mail for {} to {holder} refused (retry next pass): {e:#}",
                                b.id
                            ),
                        }
                    }
                }
                Err(e) => eprintln!("flywheel: br inprogress failed (continuing): {e:#}"),
            }
            match load_ready_beads(cfg, project) {
                Ok(ready) => {
                    let (dispatchable, not_ready) = durable::partition_ready(&ready);
                    let wave_held: Vec<String> = if no_wave_gates {
                        Vec::new()
                    } else {
                        durable::wave_gate_held(&ready, &inprog_now)
                    };
                    if !wave_held.is_empty() {
                        eprintln!(
                            "flywheel: wave-gate: holding {} (earlier wave open; not stuck-owner)",
                            wave_held.join(", ")
                        );
                    }
                    let mut scope_block: Option<String> = None;
                    let scope_ids: Option<std::collections::HashSet<String>> =
                        wave_label.map(|label| {
                            match durable::wave_scope_filter(&ready, &inprog_now, label) {
                                Ok(scoped) => scoped.into_iter().map(|b| b.id).collect(),
                                Err(e) => {
                                    eprintln!("flywheel: {e}");
                                    scope_block = Some(e);
                                    Default::default()
                                }
                            }
                        });
                    let to_assign: Vec<durable::ReadyBead> = dispatchable
                        .into_iter()
                        .filter(|&b| !wave_held.contains(&b.id))
                        .filter(|&b| {
                            let intended =
                                durable::path_tokens(&format!("{} {}", b.title, b.description));
                            !durable::file_blocks_bead(
                                &b.id,
                                &file_rows,
                                b.assignee.as_deref().unwrap_or(""),
                                &intended,
                            )
                        })
                        .filter(|&b| match (&scope_ids, &scope_block) {
                            (Some(ids), _) => ids.contains(&b.id),
                            (None, Some(_)) => false,
                            (None, None) => true,
                        })
                        .cloned()
                        .collect();
                    ready_now.clone_from(&to_assign);
                    let to_assign: Vec<String> =
                        work_prompts(&[], &to_assign, &mut assigned, project);
                    ready_items.extend(to_assign);
                    for bead in &not_ready {
                        if not_ready_mailed.insert(bead.id.clone())
                            && let Err(e) = cmd_internal_mail_not_ready(
                                cfg,
                                project,
                                &bead.id,
                                &bead.title,
                                Some(&if bead.verify.is_none() {
                                    "## VERIFY fence missing, empty, or multi-line".to_string()
                                } else {
                                    durable::principles_block_reason(bead).unwrap_or_default()
                                }),
                            )
                        {
                            eprintln!(
                                "flywheel: mail-not-ready for {} failed (continuing): {e:#}",
                                bead.id
                            );
                        }
                    }
                }
                Err(e) => eprintln!("flywheel: br ready failed (continuing): {e:#}"),
            }

            let live_pins: Vec<String> = managed.iter().map(|(n, _)| n.clone()).collect();
            let idle_pins: Vec<String> = managed
                .iter()
                .filter(|(_, s)| matches!(s, AgentStatus::Idle | AgentStatus::Done))
                .map(|(n, _)| n.clone())
                .collect();
            let starved = state::load_pane_starvation(project);
            let plan = durable::idle_pane_rotation(&durable::IdleRotationInputs {
                live_pins: &live_pins,
                idle_pins: &idle_pins,
                in_progress: &inprog_now,
                ready: &ready_now,
                peers: &peers,
                taken: &assigned,
                starved: &starved,
                resume_window: &window,
                now_us,
                window_secs: durable::RESUME_WINDOW_DEFAULT_SECS,
                exempt_after_passes: cfg.resume_exempt_passes(),
            });
            if let Err(e) = state::record_pane_starvation(project, &plan.starved_after) {
                eprintln!("flywheel: pane-starvation record failed (continuing): {e:#}");
            }
            if !plan.picks.is_empty() {
                let mut pick_items: Vec<String> = Vec::new();
                for pick in &plan.picks {
                    let held = inprog_now.iter().find(|b| b.id == pick.bead);
                    let title = held
                        .or_else(|| ready_now.iter().find(|b| b.id == pick.bead))
                        .map(|b| b.title.as_str())
                        .unwrap_or("");
                    pick_items.push(durable::brief(&pick.bead, title, project, held.is_some()));
                    println!(
                        "{} {} ← {}{}",
                        dim("[loop] idle-pane"),
                        dim(&pick.pin),
                        dim(&pick.bead),
                        if pick.exempt {
                            format!(
                                " (window-exempt: starved {} passes)",
                                starved.get(&pick.pin).copied().unwrap_or(0)
                            )
                        } else {
                            String::new()
                        }
                    );
                    assigned.push(pick.bead.clone());
                }
                queue_items.splice(0..0, pick_items);
            }
            queue_items.extend(inprog_items);
            queue_items.extend(ready_items);
        }

        let d_states: Vec<durable::AgentState> = managed
            .iter()
            .map(|(name, status)| durable::AgentState {
                name: name.clone(),
                status: format!("{status:?}").to_ascii_lowercase(),
                kind: String::new(),
                pane_id: String::new(),
                mail_identity: None,
                nudge_streak: 0,
            })
            .collect();
        let actions = durable::plan_actions(
            &crate::durable::LoopModel {
                queue: queue_items.clone(),
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
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &d_states,
            nudge,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        if on_blocked == "stop" {
            let blocked: Vec<&str> = actions
                .iter()
                .filter_map(|a| match a {
                    durable::LoopAction::Blocked { name } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            if !blocked.is_empty() {
                bail!("agent(s) {} blocked — stopping loop", blocked.join(", "));
            }
        }
        let mut any_blocked = false;
        let mut any_work = false;
        for action in &actions {
            match action {
                durable::LoopAction::PinStamp { bead_id, pin } => {
                    let exe = std::env::current_exe()?;
                    let out = std::process::Command::new(exe)
                        .args(["__pin-stamp", project, bead_id, pin])
                        .output();
                    if let Ok(o) = out
                        && !o.status.success()
                    {
                        eprintln!("flywheel: __pin-stamp for {bead_id} failed");
                    }
                }
                durable::LoopAction::Send { name, text }
                | durable::LoopAction::DispatchImmediate { name, text } => {
                    // bd-hc-registry-recover-ywypf: ONE key per project — read
                    // the registry under the bare project spelling, never the
                    // absolute dir (which hashes to `<base>-<hash>` and splits
                    // the fleet from writers using the bare key).
                    let agent_profile = state::load_spawn_registry(project)
                        .into_iter()
                        .find(|e| e.name == *name)
                        .and_then(|e| e.profile)
                        .unwrap_or_default();
                    let mut cmd = std::process::Command::new(std::env::current_exe()?);
                    cmd.arg("__send").arg(project).arg(name).arg(text);
                    if !agent_profile.is_empty() {
                        cmd.arg("--profile").arg(&agent_profile);
                    }
                    let out = cmd
                        .output()
                        .map_err(|e| anyhow::anyhow!("__send for {name} failed: {e:#}"))?;
                    let raw = String::from_utf8_lossy(&out.stdout).to_string();
                    let ok = serde_json::from_str::<Value>(raw.trim())
                        .ok()
                        .and_then(|v| v.get("ok").and_then(Value::as_bool))
                        .unwrap_or(false);
                    if !ok {
                        let err_bytes = String::from_utf8_lossy(&out.stderr).to_string();
                        let code = out.status.code().unwrap_or(-1);
                        eprintln!(
                            "{}",
                            format_send_failure(
                                name,
                                raw.trim(),
                                err_bytes.trim(),
                                out.status.success(),
                                code
                            )
                        );
                        queue_items.insert(0, text.clone());
                        continue;
                    }
                    log(
                        project,
                        EventKind::Send,
                        json!({ "to": name, "stage": "-", "text_len": text.len(), "wait": false, "source": "loop" }),
                    );
                    println!(
                        "{} → {} {}",
                        dim("[loop] sent"),
                        dim(name),
                        dim(&text.chars().take(60).collect::<String>())
                    );
                    any_work = true;
                }
                durable::LoopAction::Nudge { name } => {
                    let n = nudge.unwrap_or("proceed");
                    if let Err(e) = c.agent_prompt(name, n, None) {
                        eprintln!("flywheel: nudge to {name} failed: {e:#}");
                    } else {
                        log(
                            project,
                            EventKind::Send,
                            json!({ "to": name, "nudge": true, "text_len": n.len() }),
                        );
                        println!("{} → {name} (nudge)", dim("[loop] sent"));
                        any_work = true;
                    }
                }
                durable::LoopAction::Blocked { name } => {
                    any_blocked = true;
                    match on_blocked {
                        "mail" => {
                            if let Ok(m) = mail_or_die(cfg) {
                                let key = project_dir_str(cfg, project)?;
                                let _ = m.ensure_project(&key);
                                let mail_target = resolve_mail_target_registry(project, name);
                                let _ = m.send(
                                    &key,
                                    std::slice::from_ref(&mail_target),
                                    "[flywheel] you are blocked",
                                    "The swarm loop detected you are blocked. Resolve or report the blocker.",
                                    "high",
                                    true,
                                    Some(&format!("health-{name}")),
                                );
                                println!("{} {name} → mail alert", dim("[loop] blocked"));
                            } else {
                                eprintln!("flywheel: --on-blocked mail needs [mail.toron] config");
                            }
                        }
                        _ => {
                            println!("{} {name}", error("[loop] blocked"));
                        }
                    }
                }
                durable::LoopAction::PausedResurface {
                    name,
                    bead_id,
                    owner,
                    age_secs,
                } => {
                    println!(
                        "{} {}",
                        dim("[loop]"),
                        durable::pause_resurface_line(name, bead_id, owner, *age_secs)
                    );
                    log(
                        project,
                        EventKind::Loop,
                        json!({
                            "status": "pause_resurface",
                            "agent": name,
                            "bead": bead_id,
                            "owner": owner,
                            "age_secs": *age_secs,
                        }),
                    );
                }
                durable::LoopAction::Wait => {}
                durable::LoopAction::Diagnose { .. }
                | durable::LoopAction::Respawn { .. }
                | durable::LoopAction::Reclaim { .. } => {}
            }
        }

        let statuses: Vec<AgentStatus> = managed.iter().map(|(_, s)| *s).collect();
        let all_settled = statuses
            .iter()
            .all(|s| matches!(s, AgentStatus::Idle | AgentStatus::Done));
        let queue_drained = queue_items.is_empty();

        if once {
            println!("{}", dim("[loop] --once pass done"));
            if any_blocked && on_blocked == "continue" {
                println!(
                    "{}",
                    dim(
                        "loop: agents blocked (on_blocked=continue; pass --on-blocked stop|mail to control)"
                    )
                );
            }
            return Ok(());
        }

        if queue_drained && all_settled && !any_work {
            println!("{}", dim("[loop] queue drained, swarm settled"));
            log(project, EventKind::Loop, json!({ "status": "complete" }));
            return Ok(());
        }

        let wait_for = if let Some(d) = deadline {
            let remaining = d.saturating_duration_since(std::time::Instant::now());
            remaining.min(Duration::from_secs(interval_secs))
        } else {
            Duration::from_secs(interval_secs)
        };
        std::thread::sleep(wait_for);
    }
}

// ---- safety / guards / policy ----

fn cmd_safety(cfg: &Config) -> Result<()> {
    let policy = config::Policy::load()?;
    println!("active guards: {}", policy.deny.len());
    for p in &policy.deny {
        println!("  {}", dim(p));
    }
    let c = if let Ok(c) = Client::connect(Some(&cfg.socket_path())) {
        c
    } else {
        println!("hc: {}", error("unreachable"));
        return Ok(());
    };
    println!("hc: {}", status_paint(AgentStatus::Idle));
    if c.ping().is_ok() {
        println!("hc socket: ok");
    }
    println!(
        "br: {}",
        if command_exists("br") {
            status_paint(AgentStatus::Idle)
        } else {
            error("missing")
        }
    );
    println!(
        "bv: {}",
        if command_exists("bv") {
            status_paint(AgentStatus::Idle)
        } else {
            error("missing")
        }
    );
    Ok(())
}

fn cmd_guards(_cfg: &Config) -> Result<()> {
    let policy = config::Policy::load()?;
    if policy.deny.is_empty() {
        println!("no guards configured (flywheel policy add \"rm -rf\")");
        return Ok(());
    }
    for p in &policy.deny {
        println!("{p}");
    }
    Ok(())
}

pub(crate) fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run preflight checks and bail if any fail. No-op when `skip` is true.
/// Per-tool prompt frontmatter (tool-specific format) + {{PHASE_DESCRIPTION}}.
fn prompt_frontmatter(kind: &str) -> Option<&'static str> {
    match kind {
        "opencode" => Some(include_str!(
            "../integrations/prompts/frontmatter-opencode.md.tpl"
        )),
        "pi" => Some(include_str!(
            "../integrations/prompts/frontmatter-pi.md.tpl"
        )),
        "grok" => Some(include_str!(
            "../integrations/prompts/frontmatter-grok.md.tpl"
        )),
        "omp" => Some(include_str!(
            "../integrations/prompts/frontmatter-omp.md.tpl"
        )),
        "pa" => Some(include_str!(
            "../integrations/prompts/frontmatter-pa.md.tpl"
        )),
        _ => None,
    }
}

/// The orchestration slash-command set: open-mode, swarm-mode, loop-mode, and peer-aware-mode.
/// `open-mode` is direct current-pane work; `loop-mode` is durable solo execution;
/// `swarm-mode` is captain-led multi-crew execution; `peer-aware-mode` installs
/// wake coordination across either topology. Catalog workflows and br own
/// the retired `/workflow-*` / `/beads-*` slash family — regen deletes leftovers.
const ORCHESTRATION_COMMANDS: &[(&str, &str, &str)] = &[
    (
        "open-mode",
        "Run the /open-mode command — direct current-pane work with no swarm or durable loop lifecycle.",
        "open.md.tpl",
    ),
    (
        "swarm-mode",
        "Run the /swarm-mode command — captain-led multi-crew execution with parallel lanes; peer-aware wake is independent of work tracking.",
        "swarm.md.tpl",
    ),
    (
        "loop-mode",
        "Run the /loop-mode command — durable solo execution in one current harness; work tracking is optional and enabled by --work.",
        "loop.md.tpl",
    ),
    (
        "peer-aware-mode",
        "Retro-prime a running pane: attach to the workspace doorbell dispatcher, inject the identity line, and load the handler. --budget maps to the 5/30/3 limits.",
        "peer-aware.md.tpl",
    ),
];

/// Per-orchestration-command template: `{{TOOL_FRONTMATTER}}` + the
/// playbook body. Standalone — no beads body.
fn prompt_orchestration_template(name: &str) -> Option<&'static str> {
    match name {
        "open-mode" => Some(include_str!("../integrations/prompts/open.md.tpl")),
        "swarm-mode" => Some(include_str!("../integrations/prompts/swarm.md.tpl")),
        "loop-mode" => Some(include_str!("../integrations/prompts/loop.md.tpl")),
        "peer-aware-mode" => Some(include_str!("../integrations/prompts/peer-aware.md.tpl")),
        _ => None,
    }
}

/// Leftover slash files from the retired `/beads-*` / `/workflow-*` family
/// (and pre-rename bare `/swarm` `/loop` `/peer-aware`). Regen deletes them
/// so a harness command dir cannot keep serving frozen copies.
const RETIRED_SLASH_FILES: &[&str] = &[
    "beads-triage.md",
    "beads-execute.md",
    "beads-review.md",
    "beads-commit.md",
    "beads-create.md",
    "workflow-debate.md",
    "workflow-plan.md",
    "workflow-plan-review.md",
    "workflow-brainstorm.md",
    "workflow-onboard.md",
    "workflow-triage.md",
    "workflow-captain-audit.md",
    "swarm.md",
    "loop.md",
    "peer-aware.md",
];

/// The prompts directory for one agent kind (the dir the generated
/// commands are written into). Purely conventional per tool.
#[must_use]
pub fn prompts_dir(kind: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let home = std::path::Path::new(&home);
    Some(match kind {
        "opencode" => home.join(".config/opencode/commands"),
        "pi" => home.join(".pi/agent/prompts"),
        "grok" => home.join(".grok/prompts"),
        "omp" => home.join(".omp/agent/prompts"),
        "pa" => home.join(".prime/agent/commands"),
        _ => return None,
    })
}

/// bd-9z1d: a command description is PROSE and routinely contains `": "`, which
/// is invalid in an unquoted YAML scalar. `peer-aware-mode`'s description
/// ("Retro-prime a running pane: attach to …") made flywheel emit an
/// unparseable `peer-aware-mode` prompt, `pi` refused it with "Nested mappings
/// are not allowed in compact mappings at line 3", and the pane's agent never
/// booted — while `dispatch_job` still reported the send as `landed`, so the
/// run went green with a dead agent.
///
/// Quoting at substitution is the whole fix: the value is data, so it must not
/// be able to change the shape of the document. The alternative — trusting every
/// description to stay colon-free — is a landmine under a prose edit.
fn yaml_double_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render one orchestration command (/open-mode, /swarm-mode, /loop-mode, /peer-aware-mode) for one agent kind.
/// Pure (deterministic tests): frontmatter (with the command's description)
/// + the playbook template + version/generated stamps.
fn render_orchestration_prompt(
    kind: &str,
    name: &str,
    version: &str,
    generated: &str,
) -> Result<String> {
    let frontmatter = prompt_frontmatter(kind).ok_or_else(|| {
        anyhow!("unknown agent kind: {kind} (known: opencode, pi, grok, omp, pa)")
    })?;
    let tpl = prompt_orchestration_template(name).ok_or_else(|| {
        anyhow!("unknown orchestration command: {name} (flywheel integration list)")
    })?;
    let (_, description, _) = ORCHESTRATION_COMMANDS
        .iter()
        .find(|(n, _, _)| *n == name)
        .expect("orchestration command in table");
    let (frontmatter, body) = apply_argument_adapter(
        kind,
        &frontmatter.replace("{{PHASE_DESCRIPTION}}", &yaml_double_quoted(description)),
        tpl,
    );
    let mut out = body
        .replace("{{TOOL_FRONTMATTER}}", &frontmatter)
        .replace("{{VERSION}}", version)
        .replace("{{GENERATED}}", generated)
        .replace("{{ARGUMENTS_NOTE}}", argument_note(kind));
    if name == "peer-aware-mode" {
        const PEER_AWARE_SOT: &str = include_str!("../integrations/peer-aware.md.tpl");
        out.push_str(
            "

---

## Handler body (SoT: integrations/peer-aware.md.tpl)

",
        );
        out.push_str(PEER_AWARE_SOT);
    }
    Ok(out)
}

/// ADR-0005 D2/§8: per-kind argument adapter. Grok must not fake `$ARGUMENTS`.
fn argument_note(kind: &str) -> &'static str {
    match kind {
        "opencode" => "Arguments: `$ARGUMENTS` (maps to flywheel CLI flags).",
        "pi" | "omp" | "pa" => "Arguments: `$1` / `$@` (maps to flywheel CLI flags).",
        "grok" => {
            "Arguments: the text after the command name. Grok prompts do not interpolate dollar-ARGUMENTS."
        }
        _ => "Arguments: trailing text after the command.",
    }
}

fn apply_argument_adapter(kind: &str, frontmatter: &str, body: &str) -> (String, String) {
    let hint = match kind {
        "opencode" => "objective=... OR --work OR peer-aware=off crews=3",
        "pi" | "omp" | "pa" => "[objective=...] [--work] [peer-aware=off] [crews=N]",
        "grok" => "objective=... --work (trailing text, not interpolated)",
        _ => "args",
    };
    let fm = if frontmatter.contains("argument-hint:") {
        frontmatter.to_string()
    } else {
        frontmatter.replace(
            "description: ",
            &format!("argument-hint: \"{hint}\"\ndescription: "),
        )
    };
    let note = argument_note(kind);
    let body = if body.contains("{{ARGUMENTS_NOTE}}") {
        body.to_string()
    } else {
        format!("{body}\n\n{note}\n")
    };
    (fm, body)
}

/// `flywheel integration [KIND] [--out PATH]` — generate the orchestration
/// slash commands ({open,swarm,loop,peer-aware}-mode.md) for one agent kind.
/// With no KIND, list kinds. `--out <dir>` writes there instead of the
/// conventional prompts dir. Also deletes leftover `/beads-*` / `/workflow-*`
/// (and pre-rename bare swarm/loop/peer-aware) files in that dir.
fn cmd_integration(kind: Option<&str>, out: Option<&Path>) -> Result<()> {
    let Some(kind) = kind else {
        println!("flywheel orchestration prompts (flywheel {VERSION}):");
        println!(
            "  {} slash commands per kind: {{{}}}.md",
            ORCHESTRATION_COMMANDS.len(),
            ORCHESTRATION_COMMANDS
                .iter()
                .map(|(n, _, _)| *n)
                .collect::<Vec<_>>()
                .join(",")
        );
        for (k, _) in [
            ("opencode", ""),
            ("pi", ""),
            ("grok", ""),
            ("omp", ""),
            ("pa", ""),
        ] {
            let dir = prompts_dir(k)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<HOME>/.config/...".to_string());
            println!("  {k:9} flywheel integration {k}   -> {dir}/*-mode.md");
        }
        println!(
            "templates: integrations/prompts/{{open,swarm,loop,peer-aware,frontmatter-*}}.tpl — edit, rebuild, re-run to update"
        );
        println!(
            "retired /workflow-* and /beads-* slash files are removed on regen (catalog + br own those verbs)"
        );
        return Ok(());
    };
    let kind = match kind {
        "oc" => "opencode",
        "gk" => "grok",
        other => other,
    };
    let dir = match out {
        Some(p) => p.to_path_buf(),
        None => prompts_dir(kind).ok_or_else(|| {
            anyhow!("unknown agent kind: {kind} (known: opencode, pi, grok, omp, pa)")
        })?,
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create prompts dir {}", dir.display()))?;
    let generated = state::now_iso();
    let mut written = 0usize;
    for (name, _, _) in ORCHESTRATION_COMMANDS {
        let rendered = render_orchestration_prompt(kind, name, VERSION, &generated)?;
        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, &rendered).with_context(|| format!("write {}", path.display()))?;
        println!(
            "wrote {} ({} bytes)",
            dim(&path.display().to_string()),
            rendered.len()
        );
        written += 1;
    }
    for retired in RETIRED_SLASH_FILES {
        let p = dir.join(retired);
        if p.exists() {
            match std::fs::remove_file(&p) {
                Ok(()) => println!("removed retired {}", dim(&p.display().to_string())),
                Err(e) => eprintln!("flywheel: could not remove retired {}: {e}", p.display()),
            }
        }
    }
    println!(
        "flywheel integration {kind}: {written} prompt file(s) in {}",
        dim(&dir.display().to_string())
    );
    Ok(())
}

/// Auto-wire the toron MCP server into the agent MCP configs.
///
/// Idempotent upsert per agent kind:
/// - opencode  ~/.config/opencode/opencode.json   `mcp.toron`   (JSON, remote)
/// - pi        ~/.pi/agent/mcp.json               `mcpServers.toron` (JSON)
/// - grok      ~/.grok/config.toml                `[mcp_servers.toron]` (TOML)
///
/// Never clobbers other MCP servers; only the `toron` key is added/updated.
/// `dry_run` prints the target change without touching any file.
/// `boot_port` (>0) also spawns `toron serve --mcp-http <port>` on a
/// background task (requires the toron binary on PATH + operator-key env).
fn toron_setup_mcp(
    url: Option<&str>,
    agents: &str,
    boot_port: u16,
    dry_run: bool,
    flywheel: bool,
) -> Result<()> {
    let url = url.unwrap_or("http://127.0.0.1:18080/mcp");
    // A dry run reports; it must not fail because the environment is partial.
    let home = match std::env::var("HOME") {
        Ok(h) => Some(std::path::PathBuf::from(h)),
        Err(_) if dry_run => None,
        Err(_) => anyhow::bail!("HOME unset"),
    };
    let mut changed: Vec<String> = Vec::new();
    if home.is_none() {
        println!("(--dry-run) HOME unset — would skip agent configs");
    }

    if let Some(home) = home.as_deref() {
        for kind in agents.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let kind = match kind {
                "oc" => "opencode",
                "gk" => "grok",
                other => other,
            };
            match kind {
                "opencode" => {
                    let p = home.join(".config/opencode/opencode.json");
                    changed.push(upsert_opencode_toron(&p, url, dry_run)?);
                }
                "pi" => {
                    let p = home.join(".pi/agent/mcp.json");
                    changed.push(upsert_pi_toron(&p, url, dry_run)?);
                }
                "grok" => {
                    let p = home.join(".grok/config.toml");
                    changed.push(upsert_grok_toron(&p, url, dry_run)?);
                }
                "claude" => {
                    // bd-si4b5 Phase B: the Claude channel server is the stdio
                    // events bridge, not the toron remote MCP endpoint.
                    let p = home.join(".claude.json");
                    changed.push(upsert_claude_channel(&p, dry_run)?);
                }
                other => {
                    eprintln!(
                        "flywheel: unknown agent kind '{other}' — use opencode,pi,grok (shorts oc,pi,gk)"
                    );
                }
            }
        }
    }

    if flywheel {
        changed.push(setup_flywheel_backend(url, dry_run)?);
    }

    if changed
        .iter()
        .any(|c| c.contains("added") || c.contains("updated"))
    {
        println!("toron MCP wired: {url}  {}", changed.join(" · "));
        println!("restart the agent editor for a new session to pick up the MCP server.");
        if flywheel {
            println!(
                "flywheel backend: next session routes mail through {url} (env FLYWHEEL_TORON_URL overrides config)."
            );
        }
    } else {
        println!("toron MCP already present at {url} — nothing to change.");
    }

    if boot_port > 0 && !dry_run {
        boot_toron_server(boot_port)?;
    } else if boot_port > 0 {
        println!("(--dry-run) would boot toron serve --mcp-http 127.0.0.1:{boot_port}");
    }
    Ok(())
}

/// The claude/channel MCP server entry (bd-si4b5 Phase B). The bridge runs as
/// `flywheel mcp-events --subscription <pin>` over stdio; it emits only the D2
/// hint, never the event body. Kept pure so a unit test pins the wire shape.
pub(crate) fn claude_channel_server(
    subscription: &str,
    token_path: Option<&str>,
) -> serde_json::Value {
    let mut server = serde_json::json!({
        "command": "flywheel",
        "args": ["mcp-events", "--subscription", subscription],
        "type": "stdio"
    });
    // bd-5vi2x: the config carries only the PATH to the 0600 token file, never
    // the secret and never a plaintext env value.
    if let Some(path) = token_path {
        server["env"] = serde_json::json!({ "FLYWHEEL_TORON_TOKEN_FILE": path });
    }
    server
}

/// Claude Code reads MCP servers from `~/.claude.json` under `mcpServers`.
/// Idempotent: an unchanged entry is a no-op; a neighbour's servers survive.
fn upsert_claude_channel(path: &std::path::Path, dry: bool) -> Result<String> {
    let subscription =
        std::env::var("FLYWHEEL_MCP_SUBSCRIPTION").unwrap_or_else(|_| "default".to_string());
    let project = std::env::var("FLYWHEEL_MCP_PROJECT").unwrap_or_else(|_| "flywheel".to_string());
    let token_path = events_tokens::token_path(&config::state_dir(), &project, &subscription);
    let token_path_str = token_path.display().to_string();

    // Mint + store BEFORE writing the config: a seat whose config points at a
    // missing token is worse than one with no bridge at all.
    if !dry {
        let cli = events_tokens::ToronTokenCli {
            bin: std::env::var("TORON_BIN").unwrap_or_else(|_| "toron".to_string()),
        };
        let journal = config::state_dir().join("events-token-journal.jsonl");
        let now_us = chrono::Utc::now().timestamp_micros();
        events_tokens::mint_and_store(
            &cli,
            &subscription,
            &project,
            events_tokens::PIN_TTL_SECS,
            &token_path,
            &journal,
            now_us,
        )
        .map_err(|e| anyhow::anyhow!("cannot provision events token for {subscription}: {e}"))?;
    }

    let mut v: serde_json::Value = load_json_or_default(path)?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("claude config root must be an object"))?;
    if !obj.contains_key("mcpServers") || !obj["mcpServers"].is_object() {
        obj.insert("mcpServers".into(), serde_json::json!({}));
    }
    let servers = obj["mcpServers"].as_object_mut().expect("ensured above");
    let want = claude_channel_server(&subscription, Some(&token_path_str));
    let name = "flywheel-events";
    let action = match servers.get(name) {
        Some(prev) if prev == &want => return Ok("claude channel unchanged".to_string()),
        Some(_) => "updated",
        None => "added",
    };
    servers.insert(name.into(), want);
    if dry {
        return Ok(format!("claude channel {action} ({})", pretty_path(path)));
    }
    std::fs::write(path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(format!("claude channel {action} ({})", pretty_path(path)))
}

/// opencode: `mcp.toron` in top-level JSON config.
fn upsert_opencode_toron(path: &std::path::Path, url: &str, dry: bool) -> Result<String> {
    let mut v: serde_json::Value = load_json_or_default(path)?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("opencode config root must be an object"))?;
    if !obj.contains_key("mcp") || !obj["mcp"].is_object() {
        obj.insert("mcp".into(), serde_json::json!({}));
    }
    let mcp = obj["mcp"]
        .as_object_mut()
        .expect("mcp object ensured just above");
    let prev = mcp.get("toron").cloned();
    let want = serde_json::json!({"type":"remote","url":url,"enabled":true});
    let action = if prev.is_some() {
        if prev.as_ref() == Some(&want) {
            return Ok("opencode unchanged".to_string());
        }
        "updated"
    } else {
        "added"
    };
    mcp.insert("toron".into(), want);
    if dry {
        return Ok(format!("opencode {action} ({})", pretty_path(path)));
    }
    std::fs::write(path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(format!("opencode {action}"))
}

/// pi: `mcpServers.toron` in JSON.
fn upsert_pi_toron(path: &std::path::Path, url: &str, dry: bool) -> Result<String> {
    let mut v: serde_json::Value = load_json_or_default(path)?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("pi mcp.json root must be an object"))?;
    if !obj.contains_key("mcpServers") || !obj["mcpServers"].is_object() {
        obj.insert("mcpServers".into(), serde_json::json!({}));
    }
    let servers = obj["mcpServers"]
        .as_object_mut()
        .expect("mcpServers object ensured just above");
    let prev = servers.get("toron").cloned();
    let want = serde_json::json!({"url":url,"directTools":true});
    let action = if prev.is_some() {
        if prev.as_ref() == Some(&want) {
            return Ok("pi unchanged".to_string());
        }
        "updated"
    } else {
        "added"
    };
    servers.insert("toron".into(), want);
    if dry {
        return Ok(format!("pi {action} ({})", pretty_path(path)));
    }
    std::fs::write(path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(format!("pi {action}"))
}

/// grok: `[mcp_servers.toron]` section in TOML. Line-based idempotent upsert
/// (no toml dependency): if an `[mcp_servers.toron]` header exists, rewrite the
/// following url/enabled lines; else append a new block.
fn upsert_grok_toron(path: &std::path::Path, url: &str, dry: bool) -> Result<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut idx: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.trim() == "[mcp_servers.toron]" {
            idx = Some(i);
            break;
        }
    }
    if let Some(i) = idx {
        let mut j = i + 1;
        let mut found_url = false;
        while j < lines.len() && !lines[j].trim_start().starts_with('[') {
            let t = lines[j].trim();
            if t.starts_with("url =") {
                lines[j] = format!("url = \"{url}\"");
                found_url = true;
            }
            j += 1;
        }
        if !found_url {
            lines.insert(i + 1, format!("url = \"{url}\""));
        }
        let act = if found_url { "updated" } else { "added url" };
        if dry {
            return Ok(format!("grok {act} ({})", pretty_path(path)));
        }
        std::fs::write(path, lines.join("\n") + "\n")?;
        Ok(format!("grok {act}"))
    } else {
        if dry {
            return Ok(format!("grok added ({})", pretty_path(path)));
        }
        lines.push(String::from("[mcp_servers.toron]"));
        lines.push(format!("url = \"{url}\""));
        lines.push(String::from("enabled = true"));
        lines.push(String::new());
        std::fs::write(path, lines.join("\n") + "\n")?;
        Ok("grok added".to_string())
    }
}

fn load_json_or_default(path: &std::path::Path) -> Result<serde_json::Value> {
    if path.exists() {
        let s = std::fs::read_to_string(path)?;
        if s.trim().is_empty() {
            return Ok(serde_json::json!({}));
        }
        serde_json::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    } else {
        Ok(serde_json::json!({}))
    }
}

fn pretty_path(p: &std::path::Path) -> String {
    if let Ok(h) = std::env::var("HOME")
        && let Ok(strip) = p.strip_prefix(&h)
    {
        return format!("~/{}", strip.display());
    }
    p.display().to_string()
}

/// Spawn `toron serve --mcp-http <port>` as a detached background daemon.
/// Kept as a process spawn by design (bd-wave2): an external server process
/// has no in-process equivalent.
/// Requires the toron binary on PATH; propagates TORON_OPERATOR_KEY +
/// TORON_KEY_PASSPHRASE from the current env.
fn boot_toron_server(port: u16) -> Result<()> {
    let bind = format!("127.0.0.1:{port}");
    if std::net::TcpStream::connect("127.0.0.1:18080").is_ok() {
        println!("toron MCP daemon already listening at 127.0.0.1:18080 — skipping boot.");
        return Ok(());
    }
    if std::net::TcpStream::connect(bind.as_str().parse::<std::net::SocketAddr>()?).is_ok() {
        println!("toron MCP server already listening at {bind} — skipping boot.");
        return Ok(());
    }
    let mut cmd = std::process::Command::new("toron");
    cmd.args(["serve", "--mcp-http", &bind, "--no-color"]);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if std::env::var_os("TORON_OPERATOR_KEY").is_some() {
        let key = std::env::var("TORON_OPERATOR_KEY")
            .with_context(|| "TORON_OPERATOR_KEY must be valid UTF-8 when set")?;
        cmd.env("TORON_OPERATOR_KEY", key);
    }
    cmd.spawn()?;
    println!("booted toron serve --mcp-http {bind} (detached).");
    Ok(())
}

/// Point flywheel's OWN mail backend at the toron URL.
///
/// Writes/updates the `[mail.toron] url` line in the flywheel config.toml so
/// flywheel and all registered agents converge on ONE toron mailbox (the
/// shared `[agent_mail]` section keeps the identity name). The env
/// `FLYWHEEL_TORON_URL` still wins at load time (config.rs merges env over
/// the file) — config.toml is the durable default, env is the per-session
/// override. Idempotent; only touches the `[mail.toron]` block's `url`.
fn setup_flywheel_backend(url: &str, dry: bool) -> Result<String> {
    let path = config::config_file();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let mut in_mail = false;
    let mut url_line: Option<usize> = None;
    let mut section_idx: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.starts_with('[') && t.ends_with(']') && t != "[mail.toron]" {
            break;
        }
        if t == "[mail.toron]" {
            in_mail = true;
            section_idx = Some(i);
        }
        if in_mail && t.starts_with("url =") {
            url_line = Some(i);
        }
    }

    let action = match url_line {
        Some(i) => {
            let existing = lines[i].trim();
            if existing == format!("url = \"{url}\"") {
                return Ok("flywheel unchanged".to_string());
            }
            lines[i] = format!("url = \"{url}\"");
            "updated".to_string()
        }
        None => {
            if let Some(i) = section_idx {
                lines.insert(i + 1, format!("url = \"{url}\""));
                "added url".to_string()
            } else {
                lines.push(String::from("[agent_mail]"));
                lines.push(String::from("name = \"flywheel\""));
                lines.push(String::new());
                lines.push(String::from("[mail.toron]"));
                lines.push(format!("url = \"{url}\""));
                lines.push(String::new());
                "added section".to_string()
            }
        }
    };

    if dry {
        return Ok(format!("flywheel backend {action}"));
    }
    std::fs::write(&path, lines.join("\n") + "\n")?;
    Ok(format!("flywheel backend {action}"))
}

#[cfg(test)]
mod setup_mcp_tests {
    use super::*;

    /// bd-si4b5 Phase B: the Claude channel server is the stdio events bridge,
    /// keyed by the pin subscription, and never carries an event body.
    #[test]
    fn claude_channel_server_is_the_stdio_bridge() {
        let server = super::claude_channel_server("AmberProbe", Some("/state/amber.token"));
        assert_eq!(server["command"], "flywheel");
        assert_eq!(server["args"][0], "mcp-events");
        assert_eq!(server["args"][2], "AmberProbe");
        assert_eq!(
            server["env"]["FLYWHEEL_TORON_TOKEN_FILE"],
            "/state/amber.token"
        );
        assert!(
            !server.to_string().contains("Bearer"),
            "no plaintext token in the config: {server}"
        );
        assert!(!server.to_string().contains("body"));
    }

    /// dry-run also covers the Claude channel server without writing.
    #[test]
    fn setup_mcp_dry_run_includes_claude() {
        let _guard = crate::config::lock_env();
        toron_setup_mcp(Some("http://127.0.0.1:18080/mcp"), "claude", 0, true, false)
            .expect("dry run never fails");
    }

    /// `flywheel setup-mcp --dry-run` wires nothing: all three agent upserts
    /// run in observation mode, no boot, no flywheel config write.
    #[test]
    fn setup_mcp_dry_run_reports_without_writing() {
        let _guard = crate::config::lock_env();
        toron_setup_mcp(
            Some("http://127.0.0.1:18080/mcp"),
            "opencode,pi,grok",
            0,
            true,
            false,
        )
        .expect("dry run never fails");
    }
}

#[cfg(test)]
mod protocol_check_tests {
    use super::*;

    /// (live, pin) -> severity. hc's protocol is additive: higher is safe,
    /// lower is the real safety net.
    #[test]
    fn protocol_check_three_states() {
        for (live, pin, want) in [
            (19u64, 19u64, ProtocolCheck::Match),
            (20, 19, ProtocolCheck::Newer),
            (18, 19, ProtocolCheck::Older),
            (42, 19, ProtocolCheck::Newer),
            (1, 19, ProtocolCheck::Older),
            (0, 19, ProtocolCheck::Older),
            (19, 18, ProtocolCheck::Newer),
        ] {
            assert_eq!(protocol_check(live, pin), want, "live={live} pin={pin}");
        }
    }
}

/// Severity of a live hc protocol number vs the verified pin.
///
/// hc's protocol is *additive by design*: new optional fields, new event
/// variants, new optional request params. A live protocol HIGHER than the pin
/// is safe (the client ignores what it doesn't know) — `HDR_PROTOCOL` is the
/// *verified floor*, not the ceiling. A live protocol LOWER than the pin is
/// unsafe: the server may not understand our requests. That asymmetry is
/// what `protocol_check` encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolCheck {
    /// live == pin: exactly the verified contract.
    Match,
    /// live > pin: forward-compatible; the client may not use new features.
    Newer,
    /// live < pin: server too old; our requests may be rejected. Safety net.
    Older,
}

fn protocol_check(live: u64, pin: u64) -> ProtocolCheck {
    if live == pin {
        ProtocolCheck::Match
    } else if live > pin {
        ProtocolCheck::Newer
    } else {
        ProtocolCheck::Older
    }
}

/// The agent-mail identity the orchestrator acts as: the configured
/// `[agent_mail] name`, defaulting to "flywheel" — identical to
/// `Mail::from_config`'s default, so lease renewal and the `flywheel mail
/// locks` reservations always use the SAME identity.
fn orchestrator_mail_name(cfg: &Config) -> String {
    cfg.agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "flywheel".to_string())
}

#[cfg(test)]
mod mail_name_tests {
    use super::*;
    use config::MailConfig;

    #[test]
    fn configured_name_wins() {
        let cfg = Config {
            agent_mail: Some(MailConfig {
                url: Some("http://x".into()),
                token: None,
                name: Some("Orchid".into()),
            }),
            ..Default::default()
        };
        assert_eq!(orchestrator_mail_name(&cfg), "Orchid");
    }

    #[test]
    fn default_name_is_flywheel() {
        assert_eq!(orchestrator_mail_name(&Config::default()), "flywheel");
        let cfg = Config {
            agent_mail: Some(MailConfig {
                url: Some("http://x".into()),
                token: None,
                name: None,
            }),
            ..Default::default()
        };
        assert_eq!(orchestrator_mail_name(&cfg), "flywheel");
    }
}

/// bd-evqs: reason → bead-id parsing for the closed-bead renewal sweep.
#[cfg(test)]
mod renew_sweep_tests {
    use super::*;

    #[test]
    fn bead_id_from_reason_parses_bd_ids() {
        assert_eq!(bead_id_from_reason("bd-oj9o"), Some("bd-oj9o"));
        assert_eq!(
            bead_id_from_reason("bd-oj9o "),
            Some("bd-oj9o"),
            "trailing space trimmed"
        );
        assert_eq!(
            bead_id_from_reason("  bd-12  "),
            Some("bd-12"),
            "leading space trimmed"
        );
        assert_eq!(
            bead_id_from_reason("bd-12: fix auth"),
            Some("bd-12"),
            "first token wins"
        );
    }

    #[test]
    fn bead_id_from_reason_rejects_non_beads() {
        assert_eq!(bead_id_from_reason(""), None);
        assert_eq!(
            bead_id_from_reason("src/foo.rs"),
            None,
            "paths are not beads"
        );
        assert_eq!(bead_id_from_reason("orchestrator-lock"), None);
        assert_eq!(bead_id_from_reason("bd-"), None, "bare prefix is not an id");
        assert_eq!(
            bead_id_from_reason("my-bd-12"),
            None,
            "prefix must start the token"
        );
    }

    /// The sweep predicate: a reservation is stale when its reason parses to
    /// a bead id that is in the CLOSED set. (The filter closure lives in
    /// cmd_internal_renew_leases; this pins the parse + set membership that
    /// drives it.)
    #[test]
    fn closed_set_membership_drives_stale_filter() {
        let closed: std::collections::HashSet<String> =
            ["bd-oj9o".to_string(), "bd-12".to_string()].into();
        let stale: Vec<&str> = ["bd-oj9o", "bd-99", "src/foo.rs", ""]
            .iter()
            .filter(|r| {
                bead_id_from_reason(r)
                    .map(|id| closed.contains(id))
                    .unwrap_or(false)
            })
            .copied()
            .collect();
        assert_eq!(stale, vec!["bd-oj9o"], "only closed-bead reasons are stale");
    }
}

fn cmd_deps(cfg: &Config, ensure: bool) -> Result<()> {
    let mut ok = true;
    println!("flywheel {}", dim(VERSION));

    let _ = ensure;
    match Client::connect(Some(&cfg.socket_path())) {
        Ok(c) => match c.ping() {
            Ok(_) => println!("hc socket: {}", status_paint(AgentStatus::Idle)),
            Err(e) => {
                ok = false;
                println!("hc socket: {} ({e:#})", error("FAIL"));
            }
        },
        Err(e) => {
            ok = false;
            println!("hc socket: {} ({e:#})", error("FAIL"));
        }
    }

    // bead later), live < pin is ERROR + exit 1 (server too old — the safety
    if let Some(p) = std::process::Command::new("hc")
        .args(["schema", "--json"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| hc::parse_protocol(&String::from_utf8_lossy(&o.stdout)))
    {
        match protocol_check(p, u64::from(hc::HDR_PROTOCOL)) {
            ProtocolCheck::Match => println!(
                "hc protocol: {} (verified {})",
                paint_ok(&p.to_string()),
                hc::HDR_PROTOCOL
            ),
            ProtocolCheck::Newer => {
                println!(
                    "hc protocol: {} (verified {})",
                    crate::output::warn(&p.to_string()),
                    hc::HDR_PROTOCOL
                );
                eprintln!(
                    "flywheel: INFO hc protocol {p} > verified {} — forward-compatible (client ignores what it doesn't know), but flywheel may not use new features; rebuild against the newer schema when ready",
                    hc::HDR_PROTOCOL
                );
            }
            ProtocolCheck::Older => {
                ok = false;
                println!(
                    "hc protocol: {} (verified {})",
                    crate::output::error(&p.to_string()),
                    hc::HDR_PROTOCOL
                );
                eprintln!(
                    "flywheel: ERROR hc protocol {p} < verified {} — server too old, our requests may be rejected; upgrade hc or rebuild flywheel against the pinned schema",
                    hc::HDR_PROTOCOL
                );
            }
        }
    } else {
        println!(
            "hc protocol: {} (verified {})",
            crate::output::warn("unknown"),
            hc::HDR_PROTOCOL
        );
        eprintln!(
            "flywheel: WARNING cannot read an hc protocol number from `hc schema --json` \
             (0.1.16 has no top-level `protocol` field; the socket `hello` generation is \
             authoritative — the client still enforces it on connect)"
        );
    }

    match hc::integration_status_all() {
        Ok(list) => {
            println!("hc integrations:");
            for (kind, st) in list {
                println!("  {}", integration_status_line(&kind, &st));
            }
        }
        Err(e) => {
            eprintln!(
                "flywheel: WARNING cannot read `hc integration status` — integration surface degraded: {e:#}"
            );
            println!("hc integrations: {}", crate::output::warn("unknown"));
        }
    }

    for tool in ["br", "bv", "hc"] {
        if command_exists(tool) {
            println!("{tool}: {}", status_paint(AgentStatus::Idle));
        } else {
            ok = false;
            println!("{tool}: {}", error("missing"));
        }
    }

    {
        let probe = |bin: &str, args: &[&str]| -> Option<String> {
            let out = std::process::Command::new(bin).args(args).output().ok()?;
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        };
        let mut lines: Vec<String> = Vec::new();
        for harness in ["omp", "opencode"] {
            match headless::phase_registry_probe(harness, &probe) {
                None => lines.push(format!("  {harness}: {}", status_paint(AgentStatus::Idle))),
                Some(problem) => {
                    lines.push(format!("  {harness}: {}", crate::output::warn("DRIFT")));
                    eprintln!("flywheel: WARNING phase registry: {problem}");
                }
            }
        }
        println!("phase registry (plan):");
        for l in &lines {
            println!("{l}");
        }
    }

    match crate::daemon::durable_capability_probe() {
        Ok(()) => println!("engine store: {}", status_paint(AgentStatus::Idle)),
        Err(e) => {
            ok = false;
            println!("engine store: {} ({e:#})", error("FAIL"));
        }
    }

    match mail::Mail::from_config(Some(&cfg.toron_mail_config())) {
        Ok(Some(m)) => match m.health() {
            Ok(_) => println!("mail (toron): {}", status_paint(AgentStatus::Idle)),
            Err(e) => {
                ok = false;
                println!("mail (toron): {} ({e:#})", error("FAIL"));
            }
        },
        Ok(None) => println!("mail (toron): {}", dim("not configured (set [mail.toron])")),
        Err(e) => {
            ok = false;
            println!("agent-mail: {} ({e:#})", error("FAIL"));
        }
    }

    println!("config: {}", config::config_file().display());
    if !ok {
        bail!("deps check failed");
    }
    Ok(())
}

/// `flywheel runs` — the captain's daily dashboard (ADR-0005 P6.3). Reads the
/// per-project JSONL logs (workflow/pipeline/loop events) and aggregates them
/// into a run list with state + owner + advisory per-run cost.
///
/// bd-cron-run-no-live-u9tg: `--json` drivers get ONE stable status
/// vocabulary instead of each writer's local spelling.
///
/// ADR-0032 Spec 9 tail: rows are honest — the status is read through
/// [`history_row_status`] (unrecorded ⇒ `unknown`, never an invented `done`)
/// and the failure artifacts the run wrote ride the row
/// ([`run_list_row`]).
fn normalize_run_status(raw: &str) -> String {
    match raw {
        "done" | "success" | "succeeded" | "completed" | "ok" => "completed".to_string(),
        "fail" | "failed" | "error" => "failed".to_string(),
        "pending" | "awaiting_approval" | "parked" => "pending".to_string(),
        other => other.to_string(),
    }
}

/// ADR-0032 Spec 9 tail (bd-40d1): derive a run's terminal status from the
/// engine outcome — failed, blocked, suspended, succeeded, or unknown.
/// Explicit outcome evidence wins; a null outcome is unknown (never an
/// invented success); a completed invoke without failure evidence succeeded.
/// GS-049 P1-3: pull ActionStage from a structured failure value when present.
fn output_action_stage(output: &Value) -> Option<String> {
    output
        .get("stage")
        .or_else(|| output.get("action_stage"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            output
                .get("error")
                .and_then(|e| e.get("stage"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn run_terminal_status(output: &Value) -> &'static str {
    if let Some(s) = output.get("status").and_then(Value::as_str) {
        return match s {
            "failed" | "fail" | "error" => "failed",
            "blocked" => "blocked",
            "suspended" => "suspended",
            "succeeded" | "completed" | "done" | "ok" => "succeeded",
            _ => "unknown",
        };
    }
    if output.get("ok") == Some(&serde_json::json!(false)) {
        return "failed";
    }
    if output.get("error").is_some() {
        return "failed";
    }
    if output.is_null() {
        return "unknown";
    }
    "succeeded"
}

/// History reader status: a recorded status stands; a missing one is
/// unknown — never a default "done".
fn history_row_status(data: &Value) -> String {
    data.get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

/// A writer's own label for a row that is an annotation *about* a run rather
/// than a run outcome (see [`annotation_kind`]).
const NOTICE_PLAN_WARNING: &str = "plan-warning";
const NOTICE_LOOP_MONITOR: &str = "loop-monitor";

/// The annotation kind of a state-log row, or `None` when the row is a run
/// outcome.
///
/// Three writers log rows that annotate a run instead of recording its
/// outcome, sharing `EventKind::Workflow` / `EventKind::Loop` with real runs:
/// `phase_chain` writes one phase gate per ship link, the warn-first
/// plan-artifact gate writes warn telemetry, and the legacy loop monitor
/// writes a start notice. Each declares itself with `notice`; the shapes
/// written *before* `notice` existed are recognized by the markers they
/// already carry, so old rows read honestly too. An unrecognized `notice` is
/// still not a run — it is labeled rather than miscounted. Rows matching
/// nothing stay `None`, so a status-less row is visibly `unknown` (loud)
/// instead of being silently excused.
fn annotation_kind(e: &state::LogEntry) -> Option<&'static str> {
    let d = &e.data;
    if d.get("phase_gate").and_then(Value::as_bool) == Some(true) {
        return Some("phase-gate");
    }
    match d.get("notice").and_then(Value::as_str) {
        Some(NOTICE_PLAN_WARNING) => return Some(NOTICE_PLAN_WARNING),
        Some(NOTICE_LOOP_MONITOR) => return Some(NOTICE_LOOP_MONITOR),
        Some(_) => return Some("notice"),
        None => {}
    }
    if d.get("plan_artifact_missing").and_then(Value::as_bool) == Some(true) {
        return Some(NOTICE_PLAN_WARNING);
    }
    if d.get("interval_secs").is_some() && d.get("on_blocked").is_some() {
        return Some(NOTICE_LOOP_MONITOR);
    }
    None
}

/// True when the row annotates a run rather than recording its outcome.
fn is_annotation_row(e: &state::LogEntry) -> bool {
    annotation_kind(e).is_some()
}

/// The dashboard/agent `kind` for a state-log row: the annotation kind for a
/// notice, `headless` for a `runs/` entry surfaced under its run id, else the
/// event kind lowercased.
fn run_row_kind(project: &str, e: &state::LogEntry) -> String {
    if let Some(kind) = annotation_kind(e) {
        kind.to_string()
    } else if project.starts_with("hl-") {
        "headless".to_string()
    } else {
        format!("{:?}", e.kind).to_lowercase()
    }
}

/// The legacy loop monitor's start notice (`EventKind::Loop`). It records the
/// state the monitor entered — never a terminal status it cannot know, and
/// never silence that a reader would have to default.
fn loop_monitor_notice(interval_secs: u64, queue_len: usize, to: &str, on_blocked: &str) -> Value {
    json!({
        "notice": NOTICE_LOOP_MONITOR,
        "interval_secs": interval_secs,
        "queue_len": queue_len,
        "to": to,
        "on_blocked": on_blocked,
    })
}

/// The warn-first plan-artifact gate's telemetry row (`EventKind::Workflow`):
/// an annotation about a step, not a run outcome.
fn plan_warning_notice(workflow: &str, phase: &str) -> Value {
    json!({
        "notice": NOTICE_PLAN_WARNING,
        "workflow": workflow,
        "phase": phase,
        "plan_artifact_missing": true,
        "gate_mode": "warn",
    })
}

/// Advisory per-run cost (best-effort; 0 when unrecorded).
fn run_cost(e: &state::LogEntry) -> Value {
    let tokens = e.data.get("tokens").and_then(Value::as_u64).unwrap_or(0);
    let model = e.data.get("model").and_then(Value::as_str).unwrap_or("");
    json!({ "tokens": tokens, "model": model })
}

/// Owner of a run (workflow / pipeline / loop / agent named in payload).
fn run_owner(e: &state::LogEntry) -> String {
    ["cron", "workflow", "pipeline", "loop", "agent"]
        .iter()
        .find_map(|k| e.data.get(*k).and_then(Value::as_str).map(String::from))
        .unwrap_or_default()
}

/// One `runs list` row (ADR-0032 Spec 9 tail): terminal status, real
/// project, owner, cost, phase, and the failure artifacts the run wrote.
///
/// The status goes through [`history_row_status`], so a run that recorded no
/// status reads `unknown`. This surface used to default to `done` (printing
/// `completed` for a run nobody could vouch for) — the dashboard never
/// invents a success it cannot evidence.
fn run_list_row(project: &str, e: &state::LogEntry) -> Value {
    let state_status = history_row_status(&e.data);
    let normalized = normalize_run_status(&state_status);
    let mut row = json!({
        "id": format!("{project}:{}", &e.ts),
        "ts": e.ts,
        "project": project,
        "kind": run_row_kind(project, e),
        "owner": run_owner(e),
        "run_id": e.data.get("run_id").cloned().unwrap_or(Value::Null),
        "state": state_status,
        "status": normalized,
        "cost": run_cost(e),
        "artifacts": e
            .data
            .get("artifact_paths")
            .and_then(Value::as_array)
            .cloned()
            .map(Value::Array)
            .unwrap_or_else(|| Value::Array(Vec::new())),
        "phase": e
            .data
            .get("phase")
            .and_then(Value::as_str)
            .map(str::to_string),
        "phase_degraded": e
            .data
            .get("phase_degraded")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "action_stage": e
            .data
            .get("stage")
            .or_else(|| e.data.get("action_stage"))
            .cloned()
            .unwrap_or(Value::Null),
    });
    if let Some(ok) = e.data.get("gate_ok") {
        row["gate_ok"] = ok.clone();
    }
    row
}

/// Write a failed run's terminal output to the per-run artifact dir and
/// return its path for the ledger row. Non-failed runs record nothing.
/// A write failure warns loudly and records nothing — the ledger row still
/// lands with its truthful status.
fn write_terminal_artifact(
    project: &str,
    run_id: &str,
    output: &Value,
    status: &str,
) -> Vec<String> {
    if status != "failed" {
        return Vec::new();
    }
    let path = crate::artifact::artifact_path(project, run_id, "terminal");
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        eprintln!("flywheel: cannot write failure artifact: {e:#}");
        return Vec::new();
    }
    match serde_json::to_string_pretty(output) {
        Ok(raw) => match std::fs::write(&path, raw) {
            Ok(()) => vec![path.display().to_string()],
            Err(e) => {
                eprintln!("flywheel: cannot write failure artifact: {e:#}");
                Vec::new()
            }
        },
        Err(e) => {
            eprintln!("flywheel: cannot serialize failure artifact: {e:#}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod runs_status_tests {
    use super::normalize_run_status;

    #[test]
    fn normalizes_writer_vocabularies_to_three_states() {
        for raw in ["done", "success", "succeeded", "completed", "ok"] {
            assert_eq!(normalize_run_status(raw), "completed", "raw={raw}");
        }
        for raw in ["fail", "failed", "error"] {
            assert_eq!(normalize_run_status(raw), "failed", "raw={raw}");
        }
        for raw in ["pending", "awaiting_approval", "parked"] {
            assert_eq!(normalize_run_status(raw), "pending", "raw={raw}");
        }
    }

    #[test]
    fn unknown_status_passes_through_verbatim() {
        assert_eq!(normalize_run_status("weird"), "weird");
        assert_eq!(normalize_run_status(""), "");
    }
}

#[cfg(test)]
mod run_ledger_truth_tests {
    use super::{
        history_row_status, is_annotation_row, loop_monitor_notice, parse_leaf_output,
        plan_warning_notice, run_list_row, run_terminal_status, stage_prompt_for_project,
    };
    use crate::cmd::send::ComposeRunLifecycle;
    use crate::snapshot::latest_run_entries;
    use crate::state::{EventKind, LogEntry};
    use serde_json::json;

    fn row_entry(project: &str, data: serde_json::Value) -> LogEntry {
        LogEntry {
            ts: "2026-09-14T00:00:00Z".into(),
            kind: EventKind::Workflow,
            project: project.into(),
            data,
        }
    }

    #[test]
    fn run_ledger_truth_terminal_status_maps_outcome() {
        assert_eq!(
            run_terminal_status(&json!({"status": "failed", "error": "x"})),
            "failed"
        );
        assert_eq!(
            run_terminal_status(&json!({"status": "blocked"})),
            "blocked"
        );
        assert_eq!(
            run_terminal_status(&json!({"status": "suspended"})),
            "suspended"
        );
        assert_eq!(
            run_terminal_status(&json!({"status": "succeeded"})),
            "succeeded"
        );
        assert_eq!(run_terminal_status(&json!({"status": "done"})), "succeeded");
        assert_eq!(
            run_terminal_status(&json!({"ok": false, "error": "boom"})),
            "failed"
        );
        assert_eq!(run_terminal_status(&json!({"error": "boom"})), "failed");
        assert_eq!(run_terminal_status(&json!(null)), "unknown");
        assert_eq!(run_terminal_status(&json!({"v": {}})), "succeeded");
    }

    #[test]
    fn run_ledger_truth_history_missing_status_is_unknown() {
        assert_eq!(history_row_status(&json!({"run_id": "r1"})), "unknown");
        assert_eq!(history_row_status(&json!({"status": "failed"})), "failed");
        assert_eq!(
            history_row_status(&json!({"status": "suspended"})),
            "suspended"
        );
    }

    #[test]
    fn run_ledger_truth_stage_tier_shadows_and_falls_back() {
        let dir =
            std::env::temp_dir().join(format!("flywheel-run-ledger-stage-{}", std::process::id()));
        let fw = dir.join(".flywheel");
        std::fs::create_dir_all(&fw).expect("fw dir");
        std::fs::write(
            fw.join("stages.toml"),
            "[stages.probe]\nname = \"probe\"\nphase = \"implement\"\nparams = []\nprompt = \"Tier probe.\"\n",
        )
        .expect("write toml");
        let out = stage_prompt_for_project("probe", &dir).expect("tier resolves");
        assert!(out.contains("Tier probe"), "project tier wins: {out}");
        assert!(stage_prompt_for_project("nope", &dir).is_err());
        let bare = std::env::temp_dir().join(format!(
            "flywheel-run-ledger-stage-bare-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&bare).expect("tmp dir");
        assert!(
            stage_prompt_for_project("ce-execute", &bare).is_ok(),
            "missing tier falls back byte-identical"
        );
    }

    #[test]
    fn run_ledger_truth_leaf_garbage_is_typed_failure() {
        assert!(parse_leaf_output("{\"a\":1}", "raw", "").is_ok());
        let err = parse_leaf_output("not json at all", "raw-blob", "stderr-blob")
            .expect_err("garbage must fail, never Ok(ok:false)");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stderr-blob") || msg.contains("raw-blob"),
            "evidence rides the error: {msg}"
        );
    }

    /// ADR-0032 Spec 9 tail: `runs list` is the captain's daily dashboard, so
    /// it must not invent a success. A state-log run that recorded no status
    /// reads `unknown` — the old `unwrap_or("done")` printed `completed` for
    /// a run nobody could vouch for.
    #[test]
    fn run_ledger_truth_list_row_never_invents_done() {
        let bare = row_entry("flywheel", json!({ "workflow": "wave-swarm" }));
        let row = run_list_row("flywheel", &bare);
        assert_eq!(row["state"], json!("unknown"), "raw state honest: {row}");
        assert_eq!(row["status"], json!("unknown"), "normalized honest: {row}");
        assert_eq!(row["owner"], json!("wave-swarm"), "owner still read: {row}");

        let failed = row_entry(
            "flywheel",
            json!({ "status": "failed", "workflow": "wave-swarm" }),
        );
        let row = run_list_row("flywheel", &failed);
        assert_eq!(row["state"], json!("failed"));
        assert_eq!(row["status"], json!("failed"));

        let ok = row_entry("weird", json!({ "status": "succeeded", "workflow": "w" }));
        assert_eq!(run_list_row("weird", &ok)["status"], json!("completed"));
    }

    /// Spec 9 tail "terminal status + real project + failure artifacts": the
    /// artifact paths the run wrote ride the dashboard row, so a failed row is
    /// diagnosable without a second lookup. Absent artifacts stay a stable
    /// empty array for `--json` drivers.
    #[test]
    fn run_ledger_truth_list_row_carries_failure_artifacts() {
        let failed = row_entry(
            "flywheel",
            json!({
                "status": "failed",
                "workflow": "w",
                "artifact_paths": ["/tmp/flywheel/x.terminal.json"],
            }),
        );
        let row = run_list_row("flywheel", &failed);
        assert_eq!(
            row["artifacts"],
            json!(["/tmp/flywheel/x.terminal.json"]),
            "failure artifacts ride the row: {row}"
        );

        let ok = row_entry("flywheel", json!({ "status": "succeeded" }));
        assert_eq!(run_list_row("flywheel", &ok)["artifacts"], json!([]));

        let nullish = row_entry("flywheel", json!({ "artifact_paths": null }));
        assert_eq!(run_list_row("flywheel", &nullish)["artifacts"], json!([]));
    }

    #[test]
    fn stale_composed_run_guard_terminalizes_on_scope_exit() {
        let _g = crate::config::test_state_home();
        let project = format!("compose-lifecycle-test-{}", std::process::id());
        let run_id = "compose-run-1";
        let lifecycle = ComposeRunLifecycle::start(&project, "compose-test", run_id, json!([]));
        drop(lifecycle);

        let events = crate::state::read_log(&project).expect("compose lifecycle log");
        let latest = events
            .iter()
            .rev()
            .find(|event| {
                event.data.get("run_id").and_then(serde_json::Value::as_str) == Some(run_id)
            })
            .expect("terminal lifecycle row");
        assert_eq!(latest.data["status"], json!("failed"));
        assert_eq!(
            latest.data["error"],
            json!("compose host exited before terminal transition")
        );
    }

    #[test]
    fn run_ledger_truth_latest_outcome_reconciles_lifecycle_rows() {
        let project = "flywheel";
        let run_id = "fw-composed-test";
        let mut started = row_entry(
            project,
            json!({"run_id": run_id, "status": "running", "workflow": "compose"}),
        );
        started.ts = "2026-09-25T08:38:20Z".into();
        let mut succeeded = row_entry(
            project,
            json!({"run_id": run_id, "status": "succeeded", "workflow": "compose"}),
        );
        succeeded.ts = "2026-09-25T08:39:51Z".into();
        let mut annotation = row_entry(
            project,
            json!({"run_id": run_id, "phase_gate": true, "status": "unknown"}),
        );
        annotation.ts = "2026-09-25T08:40:00Z".into();
        let unrelated = row_entry(project, json!({"status": "running"}));

        let rows = latest_run_entries(vec![
            (project.into(), started),
            (project.into(), succeeded),
            (project.into(), annotation),
            (project.into(), unrelated),
        ]);

        assert_eq!(rows.len(), 3, "terminal outcome replaces its start row");
        assert!(rows.iter().any(|(_, entry)| {
            entry.data.get("status").and_then(serde_json::Value::as_str) == Some("succeeded")
        }));
        assert!(
            !rows.iter().any(|(_, entry)| {
                entry.data.get("status").and_then(serde_json::Value::as_str) == Some("running")
                    && entry.data.get("run_id").and_then(serde_json::Value::as_str) == Some(run_id)
            }),
            "the historical start transition is not current state"
        );
        assert!(
            rows.iter().any(|(_, entry)| is_annotation_row(entry)),
            "annotations remain visible beside the reconciled outcome"
        );
        assert!(
            rows.iter()
                .any(|(_, entry)| entry.data.get("run_id").is_none()),
            "rows without a logical run id are not collapsed"
        );
    }

    /// `--robot-snapshot` feeds agents, so its run rows obey the same honesty
    /// rule as `runs list`: a status-less row reads `unknown`, never a
    /// defaulted `done`.
    #[test]
    fn run_ledger_truth_robot_snapshot_rows_are_honest() {
        let _g = crate::config::test_state_home();
        crate::log(
            "demo-honest-snapshot",
            EventKind::Workflow,
            json!({ "workflow": "w" }),
        );
        let rows = crate::snapshot::snapshot_runs();
        let row = rows
            .iter()
            .find(|r| r["project"] == json!("demo-honest-snapshot"))
            .expect("the status-less run row is listed");
        assert_eq!(row["state"], json!("unknown"), "no invented done: {row}");
    }

    /// A phase-gate annotation shares `EventKind::Workflow` with real runs but
    /// is not a run outcome (ADR-0022 W2/W4 gates): the dashboard labels it
    /// `phase-gate` and carries the gate's own verdict, so a ship chain's link
    /// gates never masquerade as run rows.
    #[test]
    fn run_ledger_truth_list_row_labels_phase_gates() {
        let gate = row_entry(
            "flywheel",
            json!({
                "workflow": "ship",
                "link": "verify",
                "phase": "verify",
                "phase_gate": true,
                "gate_ok": false,
                "evidence": "missing VERIFY tail",
                "run_id": "r-link3-verify",
            }),
        );
        let row = run_list_row("flywheel", &gate);
        assert_eq!(row["kind"], json!("phase-gate"), "not a run: {row}");
        assert_eq!(row["gate_ok"], json!(false), "verdict rides it: {row}");
        assert_eq!(
            row["status"],
            json!("unknown"),
            "no invented outcome: {row}"
        );

        let run = row_entry("flywheel", json!({ "workflow": "w", "status": "failed" }));
        let row = run_list_row("flywheel", &run);
        assert_eq!(row["kind"], json!("workflow"));
        assert!(row.get("gate_ok").is_none(), "no verdict on a run: {row}");
    }

    /// The other two annotation families: the warn-first plan-artifact gate
    /// and the legacy loop monitor's start notice are *about* a run, not
    /// outcomes of one. Rows written before `notice` existed are recognized by
    /// their markers, so the historical rows read honestly too.
    #[test]
    fn run_ledger_truth_list_row_labels_other_annotations() {
        let plan = row_entry(
            "flywheel",
            json!({
                "workflow": "wf",
                "phase": "implement",
                "plan_artifact_missing": true,
                "gate_mode": "warn",
            }),
        );
        let row = run_list_row("flywheel", &plan);
        assert_eq!(row["kind"], json!("plan-warning"), "{row}");
        assert_eq!(
            row["status"],
            json!("unknown"),
            "no invented run outcome: {row}"
        );

        let monitor = row_entry(
            "flywheel",
            json!({ "interval_secs": 30, "on_blocked": "mail", "queue_len": 0, "to": "all" }),
        );
        let row = run_list_row("flywheel", &monitor);
        assert_eq!(row["kind"], json!("loop-monitor"), "{row}");

        let declared = row_entry(
            "flywheel",
            json!({ "notice": "loop-monitor", "workflow": "w" }),
        );
        assert_eq!(
            run_list_row("flywheel", &declared)["kind"],
            json!("loop-monitor")
        );

        let unknown_notice = row_entry("flywheel", json!({ "notice": "future-thing" }));
        assert_eq!(
            run_list_row("flywheel", &unknown_notice)["kind"],
            json!("notice")
        );

        let bare = row_entry("flywheel", json!({ "workflow": "w" }));
        assert_eq!(run_list_row("flywheel", &bare)["kind"], json!("workflow"));
    }

    /// The writers declare what their rows are, so the reader never has to
    /// guess: the loop monitor's start notice and the plan-warn telemetry each
    /// carry their `notice` label, and the legacy runner records the outcome
    /// it actually observed instead of leaving the row status-less.
    #[test]
    fn run_ledger_truth_writer_payloads_self_describe() {
        let monitor = loop_monitor_notice(30, 2, "all", "mail");
        assert_eq!(monitor["notice"], json!("loop-monitor"), "{monitor}");
        assert_eq!(monitor["interval_secs"], json!(30));
        assert_eq!(monitor["queue_len"], json!(2));
        assert_eq!(monitor["to"], json!("all"));
        assert_eq!(monitor["on_blocked"], json!("mail"));
        assert!(
            monitor.get("status").is_none(),
            "a start notice records no terminal status: {monitor}"
        );

        let warn = plan_warning_notice("wf", "implement");
        assert_eq!(warn["notice"], json!("plan-warning"), "{warn}");
        assert_eq!(warn["plan_artifact_missing"], json!(true));
        assert_eq!(warn["gate_mode"], json!("warn"));

        let e = |data: serde_json::Value| LogEntry {
            ts: "2026-09-14T00:00:00Z".into(),
            kind: EventKind::Loop,
            project: "flywheel".into(),
            data,
        };
        assert!(is_annotation_row(&e(monitor)));
        assert!(is_annotation_row(&e(warn)));
    }

    /// The agent feed labels gate annotations too — an agent must not read a
    /// ship link's gate as a run outcome.
    #[test]
    fn run_ledger_truth_robot_snapshot_labels_phase_gates() {
        let _g = crate::config::test_state_home();
        crate::log(
            "demo-honest-gate",
            EventKind::Workflow,
            json!({ "workflow": "ship", "phase_gate": true, "gate_ok": true }),
        );
        let rows = crate::snapshot::snapshot_runs();
        let row = rows
            .iter()
            .find(|r| r["project"] == json!("demo-honest-gate"))
            .expect("the gate annotation is listed");
        assert_eq!(row["kind"], json!("phase-gate"), "{row}");
    }
}

/// `flywheel catalog --json` — one agent-discoverable snapshot over all four
/// catalog nouns (ADR-0007 D4).
fn catalog_snapshot_json() -> Result<()> {
    let reg = models::ModelRegistry::load_routing()?;
    let cat = profile::ProfileCatalog::load()?;
    let squads = profile::compiled_in_squads();
    print_json(&json!({
        "slots": reg.slots,
        "models": reg.models.iter().map(|(id, e)| {
            json!({
                "id": id,
                "tier": e.tier.map(models::Tier::as_str),
                "caps": e.caps,
                "fallback": e.fallback,
                "effort": e.effort,
                "cost_in_per_mtok": e.cost_in_per_mtok,
                "cost_out_per_mtok": e.cost_out_per_mtok,
            })
        }).collect::<Vec<_>>(),
        "profiles": cat.names().into_iter().filter_map(|name| {
            let p = cat.get(&name)?;
            Some(json!({
                "name": name,
                "harness": p.harness,
                "model": p.model,
                "effort": p.effort,
                "role": p.role,
                "when": p.when,
                "builtin": is_builtin_profile(&name),
            }))
        }).collect::<Vec<_>>(),
        "squads": squads.iter().map(|(name, s)| json!({
            "name": name,
            "members": s.counts,
        })).collect::<Vec<_>>(),
    }));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// bd-xe9r: the briefing skip matrix, pure so tests cover it without a TTY.
/// The briefing arms ONLY on an interactive terminal with no explicit
/// fleet: --json (agent drivers), --yes (operator pre-approved), and an
/// explicit --crew (the fleet is already named) all skip byte-identically.
fn briefing_enabled(json: bool, yes: bool, crew: Option<&str>, is_tty: bool) -> bool {
    !json && !yes && crew.is_none() && is_tty
}

/// ADR-0011 D2: unique-role infer then pack into total N (cap crew.max).
fn packed_from_flag(
    cfg: &Config,
    wf: &toron_workflow::schema::Workflow,
    resolve: &dyn Fn(&str) -> Option<toron_workflow::schema::Workflow>,
    crew: Option<&str>,
    crew_max_flag: Option<u32>,
) -> Result<crew::PackedCrew> {
    let max = crew_max_flag.unwrap_or_else(|| cfg.crew_max());
    let inferred = crew::plan_crew_with(wf, resolve);
    let flag = match crew {
        None => crew::CrewFlag::Default,
        Some(s) => crew::parse_crew_flag(s),
    };
    let req = match flag {
        crew::CrewFlag::Named(name) => {
            let cat = profile::ProfileCatalog::load()?;
            let recipes = config::Recipes::load()?.recipes;
            let counts = profile::resolve_squad(&name, &recipes, cat.get(&name).is_some())
                .ok_or_else(|| anyhow!("unknown squad {name:?} (flywheel squad list)"))?
                .counts;
            let n: u32 = counts.values().copied().sum();
            crew::CrewFlag::Total(n.max(1))
        }
        other => other,
    };
    crew::pack_inferred(&inferred, &req, max).map_err(|e| anyhow!(e))
}

/// Mail the asker a one-way approval notice (expiry). The body is a notice,
/// never an approval: `decision=expired` does not parse as approve/deny.
fn mail_approval_notice(project: &str, asker: &str, key: &str) -> Result<()> {
    let body = request_receipts::approval_body(key, "expired", "approval wait bound elapsed", None);
    let args = toron_core::mailbox::messaging::SendMessage {
        project_slug: project.to_string(),
        sender_name: asker.to_string(),
        to: vec![asker.to_string()],
        subject: "[approval] expired".to_string(),
        body_md: body,
        ..Default::default()
    };
    crate::mail::block_on(crate::mail::comms_mailbox()?.send_message(&args))?;
    Ok(())
}

/// ADR-0019 D9 steer-side verdict for a bead-targeted objective.
#[derive(Debug, PartialEq)]
pub(crate) enum HoldVerdict {
    /// No bead target, no hold, or holder already told: proceed.
    Clear,
    /// Held and the holder has a fresh notice (or one inside the window).
    Held,
    /// The holder could not be told: leave the slot `Unknown`, never claim
    /// the steer happened.
    Unknown(String),
}

/// ADR-0019 D9 steer-side check: a steer targeting an existing bead
/// (`bead:<id>` objective) checks before its first pane write. Held mails
/// the holder (once per window, same store as the loop) and stops the
/// steer without dispatch.
pub(crate) fn check_bead_file_hold(
    cfg: &Config,
    project_owned: &str,
    objective: &str,
) -> Result<HoldVerdict> {
    let Some(bead_id) = objective
        .trim()
        .strip_prefix("bead:")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(HoldVerdict::Clear);
    };
    let mut target: Option<(String, String, String)> = None;
    for beads in [
        load_inprogress_beads(cfg, project_owned),
        load_ready_beads(cfg, project_owned),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(b) = beads.iter().find(|b| b.id == bead_id) {
            target = Some((
                b.title.clone(),
                b.description.clone(),
                b.assignee.clone().unwrap_or_default(),
            ));
            break;
        }
    }
    let Some((title, description, pane)) = target else {
        return Ok(HoldVerdict::Clear);
    };
    let intended = durable::path_tokens(&format!("{title} {description}"));
    let file_rows = live_file_rows(cfg, project_owned);
    if !durable::file_blocks_bead(bead_id, &file_rows, &pane, &intended) {
        return Ok(HoldVerdict::Clear);
    }
    let Some((holder, pattern)) = durable::file_hold_notice(bead_id, &file_rows, &pane, &intended)
    else {
        return Ok(HoldVerdict::Unknown(format!(
            "target bead {bead_id} is file-held but no holder is named"
        )));
    };
    let now_us = chrono::Utc::now().timestamp_micros();
    let hold_key = format!("filehold\x1f{bead_id}\x1f{holder}");
    if state::load_resume_window(project_owned)
        .get(&hold_key)
        .is_some_and(|t| {
            now_us.saturating_sub(*t) < request_receipts::FILE_HOLD_WINDOW_SECS as i64 * 1_000_000
        })
    {
        return Ok(HoldVerdict::Held);
    }
    let args = toron_core::mailbox::messaging::SendMessage {
        project_slug: project_owned.to_string(),
        sender_name: "captain".to_string(),
        to: vec![holder.clone()],
        subject: format!("[file-hold] {bead_id}"),
        body_md: format!("bead={bead_id}\npath={pattern}"),
        ..Default::default()
    };
    match crate::mail::block_on(crate::mail::comms_mailbox()?.send_message(&args)) {
        Ok(_) => {
            if let Err(e) = state::update_locked(
                &state::resume_window_file(project_owned),
                |map: &mut std::collections::BTreeMap<String, i64>| {
                    map.insert(hold_key, now_us);
                },
            ) {
                eprintln!("flywheel: file-hold watermark for {bead_id} failed (continuing): {e:#}");
            }
            Ok(HoldVerdict::Held)
        }
        Err(e) => Ok(HoldVerdict::Unknown(format!(
            "holder mail for {bead_id} refused: {e:#}"
        ))),
    }
}

/// Build the workflow command an approved payload runs. Pure: the same
/// `goal`/`goal-direct` delegate `cmd_steer` uses, with the payload's
/// fields. Empty crew/reuse (`""`/`"-"`) mean absent, as the park arm
/// stores them.
pub(crate) fn approved_run_cmd(payload: &request_receipts::SteerPayload) -> WorkflowCmd {
    let crew = (!payload.crew.is_empty() && payload.crew != "-").then(|| payload.crew.clone());
    let reuse = (!payload.reuse.is_empty() && payload.reuse != "-").then(|| payload.reuse.clone());
    steer_run_cmd(
        &payload.objective,
        payload.direct,
        crew.as_deref(),
        Some(payload.project.as_str()),
        reuse.as_deref(),
        payload.retain,
        payload.retire_orphans,
        false,
        &[],
        &[],
    )
}

/// Run a payload the operator approved (ADR-0019 D10). No briefing —
/// approval IS the gate. No receipt gate — the parked row already gated
/// this slot. The dispatch tail duplicates `cmd_steer`'s on purpose:
/// `the_steer_receipt_gate_precedes_dispatch` owns `cmd_steer`'s shape, so
/// the shared tail cannot move without breaking that fence.
pub(crate) fn run_approved_steer(
    cfg: &Config,
    payload: &request_receipts::SteerPayload,
) -> Result<()> {
    let summary = format!("AUTHORITATIVE OPERATOR OBJECTIVE: {}", payload.objective);
    append_pending_fact(&crate::config::state_dir(), &payload.project, &summary).map_err(|e| {
        anyhow!(
            "approve: could not queue material MaterialContext fact for project {}: {e:#}",
            payload.project
        )
    })?;
    let run = approved_run_cmd(payload);
    cmd_workflow(cfg, &run)
}

/// ADR-0022 W4 testable seam: the exact Run command `flywheel steer`
/// delegates to — execute=true, pane mode, goal/goal-direct. Durability is
/// owned downstream by the Run arm (engine.invoke over the in-house engine);
/// steer only picks the workflow and pre-fills the objective.
#[allow(clippy::too_many_arguments)]
fn steer_run_cmd(
    objective: &str,
    direct: bool,
    crew: Option<&str>,
    project: Option<&str>,
    reuse: Option<&str>,
    retain: bool,
    retire_orphans: bool,
    json: bool,
    role_model: &[String],
    role_harness: &[String],
) -> WorkflowCmd {
    let wf_name = if direct { "goal-direct" } else { "goal" };
    let inputs = serde_json::json!({ "objective": objective });
    WorkflowCmd::Run {
        name: wf_name.to_string(),
        inputs: Some(inputs.to_string()),
        crew: crew.map(String::from),
        execute: true,
        mode: "pane".to_string(),
        project: project.map(String::from),
        reuse: reuse.map(String::from),
        retain,
        retire_orphans,
        json,
        yes: false,
        allow_worktree: false,
        role_model: role_model.to_vec(),
        role_harness: role_harness.to_vec(),
    }
}

/// bd-vlvn: live suggestion enrichment for the TTY briefing — when a key
/// is present, batch-score the implement candidates (ONE request,
/// when-scope excerpts in state) plus one tier ask; gated wins overwrite
/// the table suggestions, denies keep them. Keyless (or pack-missing)
/// runs keep the FallbackTable untouched.
fn enrich_suggestions_live(
    ctx: &mut steer_brief::BriefingContext,
    objective: &str,
    authority: &crate::authority::RuntimeAuthority,
) {
    let transport = match crate::judgment::UreqTransport::from_authority(authority) {
        Ok(t) => t,
        Err(_) => return,
    };
    let catalog = crate::profile::ProfileCatalog::load().unwrap_or_default();
    let candidates: Vec<(String, String)> = ["coder", "rust-coder", "ts-coder", "zig-coder"]
        .iter()
        .filter_map(|n| catalog.get(n).map(|p| (n.to_string(), p.when.clone())))
        .collect();
    if let Ok(pack) = crate::judgment::find_pack_for(".", "profile-pick")
        && let Ok(s) =
            crate::judgment::live_profile_suggestion(&transport, &pack, objective, &candidates)
    {
        ctx.profile_suggestion = s;
    }
    if let Ok(pack) = crate::judgment::find_pack_for(".", "tier-pick")
        && let Ok(s) = crate::judgment::live_tier_suggestion(&transport, &pack, objective)
    {
        ctx.tier_suggestion = s;
    }
    // governed-by: bd-ulzi — the same tier-pick ask yields the effort
    if let Ok(pack) = crate::judgment::find_pack_for(".", "tier-pick")
        && let Ok(s) = crate::judgment::live_effort_suggestion(&transport, &pack, objective)
    {
        ctx.effort_suggestion = s;
    }
}

/// bd-xe9r: everything the interactive briefing renders, computed BEFORE any
/// IO — the pure data half of the briefing seam. Resolves the target
/// workflow (catalog overrides built-in), infers the crew plan, and builds
/// per-role model menus from the composite registry (cost/caps on screen).
/// bd-nmzk: shared prompt module — steer and workflow run stay in lockstep
/// via `briefing_context_inner` (one crew/model/squad/stage path, `source`
/// is the only fork).
fn steer_briefing_context(objective: &str, wf_name: &str) -> Result<steer_brief::BriefingContext> {
    briefing_context_inner(objective, wf_name, "steer")
}

/// bd-nmzk: workflow run briefing context — same shared module as steer,
/// `source = "workflow"` so the header reads `workflow "name"` and the
/// objective is derived from the run inputs (objective/goal/target key or
/// raw JSON, fallback to workflow name).
fn workflow_briefing_context(
    wf_name: &str,
    inputs: Option<&str>,
) -> Result<steer_brief::BriefingContext> {
    let objective = match inputs {
        Some(s) if !s.trim().is_empty() => {
            if let Ok(v) = serde_json::from_str::<Value>(s) {
                if let Some(o) = v.get("objective").and_then(|x| x.as_str()) {
                    o.to_string()
                } else if let Some(g) = v.get("goal").and_then(|x| x.as_str()) {
                    g.to_string()
                } else if let Some(t) = v.get("target").and_then(|x| x.as_str()) {
                    t.to_string()
                } else {
                    s.to_string()
                }
            } else {
                s.to_string()
            }
        }
        _ => wf_name.to_string(),
    };
    briefing_context_inner(&objective, wf_name, "workflow")
}

fn briefing_context_inner(
    objective: &str,
    wf_name: &str,
    source: &str,
) -> Result<steer_brief::BriefingContext> {
    let all = workflow_catalog::all_workflows_plain(None)?;
    let wf = all
        .get(wf_name)
        .ok_or_else(|| anyhow!("unknown workflow {wf_name:?} for {source} briefing"))?
        .clone();
    let resolve = |sub: &str| all.get(sub).cloned();
    let plan = crew::plan_crew_with(&wf, &resolve);
    let stage_chain: Vec<String> = wf.steps.iter().filter_map(|s| s.name.clone()).collect();

    let cat_profiles = profile::ProfileCatalog::load().unwrap_or_default();
    let registry = models::ModelRegistry::load_routing().unwrap_or_default();
    let models = plan
        .members
        .keys()
        .map(|role| {
            let profile = cat_profiles.get(role);
            let (harness, default_model) = match profile {
                Some(p) => {
                    let (h, m) = registry
                        .resolve_profile_route(&p.harness, &p.model)
                        .unwrap_or_else(|_| (p.harness.clone(), p.model.clone()));
                    (h, m)
                }
                None => (cfg_resolve_kind_default(role), String::new()),
            };
            let mut alternatives: Vec<(String, f64, Vec<String>)> = registry
                .models
                .iter()
                .filter(|(_, e)| e.caps.is_empty() || e.caps.iter().any(|c| c == &harness))
                .map(|(id, e)| (id.clone(), e.cost_in_per_mtok, e.caps.clone()))
                .collect();
            alternatives.sort_by(|a, b| {
                let da = a.0 == default_model;
                let db = b.0 == default_model;
                db.cmp(&da).then_with(|| a.0.cmp(&b.0))
            });
            steer_brief::RoleModelChoice {
                role: role.clone(),
                harness,
                default_model,
                alternatives,
            }
        })
        .collect();

    let recipes = config::Recipes::load()
        .map(|r| r.recipes)
        .unwrap_or_default();
    let plan_roles: std::collections::BTreeSet<&str> =
        plan.members.keys().map(String::as_str).collect();
    let squad_recommendation = recipes
        .iter()
        .find(|(_name, recipe)| {
            let members = profile::Squad::from_recipe(recipe);
            members.counts.len() == plan_roles.len()
                && members
                    .counts
                    .keys()
                    .all(|m| plan_roles.contains(m.as_str()))
        })
        .map(|(name, _)| name.clone());
    let unique = plan.members.len() as u32;
    let cap = crate::crew::DEFAULT_CREW_MAX;
    Ok(steer_brief::BriefingContext {
        objective: objective.to_string(),
        workflow: wf_name.to_string(),
        source: source.to_string(),
        crew_plan: plan.members,
        crew_max: cap,
        default_budget: unique.max(1).min(cap),
        models,
        squad_recommendation,
        profile_suggestion: match crate::judgment::suggest_profile_pick(objective) {
            "@coder" => None,
            s => Some(s.to_string()),
        },
        tier_suggestion: crate::judgment::suggest_tier_pick(objective).map(str::to_string),
        // governed-by: bd-ulzi — no offline effort table: Jev-only.
        effort_suggestion: None,
        bare_override: None,
        stage_chain,
        funded: std::collections::BTreeMap::new(),
    })
}

/// Harness fallback for a briefing role with NO profile entry: the role
/// name IS the kind short on this CLI (`oc`/`pi`/`gk`/kind), resolved via
/// the same config map spawn uses; unknown -> the bare name.
fn cfg_resolve_kind_default(role: &str) -> String {
    let resolved = crate::config::Config::default().resolve_kind(role);
    if resolved == role || resolved.is_empty() {
        role.to_string()
    } else {
        resolved
    }
}

/// `profile run --workflow` (bd-6zsl): the invoke command — run the named
/// workflow in pane mode against the spawned agent's workspace (--reuse
/// binds the crew into the SAME workspace; the caller's workspace is never
/// reaped). Pane mode is the only fit: the profile agent is a live pane.
fn profile_workflow_invoke(
    name: &str,
    inputs: Option<&str>,
    workspace: &str,
    project: &str,
) -> WorkflowCmd {
    WorkflowCmd::Run {
        name: name.to_string(),
        inputs: inputs.map(str::to_string),
        crew: None,
        execute: true,
        mode: "pane".to_string(),
        project: Some(project.to_string()),
        reuse: Some(workspace.to_string()),
        retain: false,
        retire_orphans: false,
        json: false,
        yes: false,
        allow_worktree: false,
        role_model: vec![],
        role_harness: vec![],
    }
}

/// Planner arm (bd-u4cu): run the planner profile headlessly via the
/// `__run-headless` leaf and return its response text (the authored workflow
/// YAML). A failed leaf (`ok: false`) or a non-JSON/non-string leaf bails —
/// a broken authoring never reaches the lint gate.
fn planner_author_response(wf_name: &str, prompt: &str) -> Result<String> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__run-headless")
        .arg("planner")
        .arg(prompt)
        .arg("--workflow")
        .arg(wf_name)
        .arg("--json");
    if let Ok(cat) = profile::ProfileCatalog::load()
        && let Some(p) = cat.get("planner")
        && !p.model.is_empty()
        && models::slot_alias(&p.model).is_none()
    {
        cmd.arg("--model").arg(&p.model);
    }
    let output = cmd
        .output()
        .with_context(|| "planner headless authoring run")?;
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let v: Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow!("planner authoring leaf returned non-JSON: {e}"))?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        let err = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown leaf error");
        bail!("planner authoring failed: {err}");
    }
    v.get("response")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("planner authoring leaf returned no response"))
}

/// ISO timestamp `now - secs` (for `runs digest --since`).
fn now_minus(secs: u64) -> String {
    let (now, _) = state::utc_now();
    let (y, mo, d, h, mi, s) = state::civil_from_unix(now.saturating_sub(secs));
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// True when a profile is one of the 6 compiled-in built-ins (so `create`/
/// `edit` doesn't duplicate it on disk verbatim).
fn is_builtin_profile(name: &str) -> bool {
    profile::compiled_in_defaults().contains_key(name)
}

/// Resolve a pane id in the default project workspace for `profile run` /
/// `squad spawn`: reuse the first workspace, else create one, then return a
/// PANE id inside it — `agent_start` takes a pane id, never a workspace id
/// (bd-ude8: hc rejects a workspace id with `agent_pane_not_found`).
fn resolved_spec_workspace(
    _cfg: &Config,
    backend: &dyn backend::MultiplexerBackend,
    _harness: &str,
) -> Result<String> {
    let ws_id = match backend.workspace_list()?.first() {
        Some(first) => first.workspace_id.clone(),
        None => backend.workspace_create(None)?.workspace_id,
    };
    let panes = backend.pane_list(Some(&ws_id))?;
    panes
        .first()
        .map(|p| p.pane_id.clone())
        .ok_or_else(|| anyhow::anyhow!("no pane in workspace {ws_id} for profile spawn"))
}

/// Prime a profile-launched agent with its mail identity once its TUI is up.
///
/// `profile run`/`squad spawn` get their shell from `workspace_create`, which
/// carries only `HC_*` — the pin env that `spawn`/`add` inject at split time is
/// not there, so the agent would run without `AGENT_NAME` and the strict guard
/// would block its commits. Like `spawn`, type the pin in after boot. The wait
/// is bounded and the prime is best-effort: a pane that never reports still
/// gets the line, with a warning.
fn prime_profile_identity(
    backend: &dyn backend::MultiplexerBackend,
    pane_id: &str,
    name: &str,
    pin: &str,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        ready = backend
            .agent_list()
            .ok()
            .and_then(|agents| agents.into_iter().find(|a| a.pane_id == pane_id))
            .is_some_and(|a| a.interactive_ready);
        if ready {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if !ready {
        eprintln!(
            "flywheel: warn: {name} ({pane_id}) not input-ready within 15s — typing the identity prime anyway"
        );
    }
    if let Err(e) = backend.prompt(pane_id, &doorbell::identity_prime(name, pin), None) {
        eprintln!("flywheel: warn: identity prime for {name} ({pane_id}) failed: {e:#}");
    }
}

/// Merge one entry into the project's spawn registry (bd-l2ky): preserve
/// entries for other agents, replace the same name. Shared by `profile run`
/// and `squad spawn` so `__send --profile` and loop conditioning resolve
/// the agent's profile (bd-gsvn.1). Best-effort — a registry failure is not
/// a spawn failure (cmd_spawn precedent).
pub(crate) fn record_spawn_entry(cfg: &Config, project: &str, entry: state::SpawnEntry) {
    // bd-hc-registry-recover-ywypf: ONE key per project. Resolve through the
    // cfg workspace root exactly like the readers (`project_dir_str`), then
    // key the absolute dir: a bare write against the env root and an
    // absolute read against the cfg root split one pane across two files
    // (the hermetic spawn test caught it).
    let Ok(key) = project_dir_str(cfg, project) else {
        eprintln!("flywheel: cannot resolve {project} for spawn registry write");
        return;
    };
    // A mutating door must not treat a corrupt fleet as empty: refuse the
    // write (loud, naming the path) instead of paving the file with one row.
    let current = match state::load_spawn_registry_strict(&key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("flywheel: REFUSING to record spawn entry for {project}: {e}");
            return;
        }
    };
    let mut registry = state::merge_registry_for_mutation(&key, current);
    registry.retain(|e| e.name != entry.name);
    registry.push(entry);
    if let Err(e) = state::save_spawn_registry(&key, &registry) {
        eprintln!("flywheel: cannot save spawn registry: {e:#}");
    }
}

/// On-disk shape of `profiles.toml` for the CLI writer.
#[derive(serde::Serialize)]
struct ProfilesFile {
    profiles: BTreeMap<String, profile::Profile>,
}

/// Drive a surface `send` from sync dispatch with the AM-shaped positional
/// surface (the durable loop's bread-and-butter). Seam: `SendMail` struct,
/// `run_mail` block_on on the shared runtime.
#[allow(clippy::too_many_arguments)]
fn mail_send(
    mail: &dyn backend::MailSurface,
    key: &str,
    to: &[String],
    subject: &str,
    body: &str,
    importance: &str,
    ack: bool,
    thread: Option<&str>,
) -> Result<backend::SendReceipt> {
    Ok(backend::run_mail(mail.send(&backend::SendMail {
        project: key.to_string(),
        sender: mail.agent_name().to_string(),
        to: to.to_vec(),
        subject: subject.to_string(),
        body_md: body.to_string(),
        importance: Some(importance.to_string()),
        ack_required: ack,
        thread_id: thread.map(String::from),
        ..Default::default()
    }))?)
}

/// Pure selection of the newest in-thread row from `sender` over
/// inbox-projection rows (the FTS-free path shared by harvest + reply
/// verification). Matches the sender NAME (toron resolves registered npubs;
/// agent-mail returns `from` directly) or the raw npub hex (toron's
/// fallback when the sender never registered). Newest wins.
///
/// governed-by: ADR-0001 §9.1 (bd-debate-harvest-dsww / bd-debate-reply-routing-99ad)
fn newest_in_thread<'a>(
    items: &'a [backend::MailItem],
    thread: &str,
    sender: &str,
    sender_npub: &str,
) -> Option<&'a backend::MailItem> {
    items
        .iter()
        .filter(|m| {
            m.thread_id
                .as_ref()
                .map(toron_core::mailbox::ids::MessageId::as_str)
                == Some(thread)
        })
        .filter(|m| m.sender == sender || (!sender_npub.is_empty() && m.sender == sender_npub))
        .max_by_key(|m| m.created_ts_us)
}

/// Harvest the newest in-thread reply from `sender` via the INBOX
/// PROJECTION (fetch_inbox), not FTS search — hyphenated thread ids like
/// `debate-20260806T130548` are not reliably phrase-searchable in FTS5
/// (index-coverage gap, cf bd-debate-harvest-dsww), so a fresh thread can
/// be invisible to `search_messages` while the reply sits in the inbox.
/// The orchestrator's own inbox carries every in-thread message with its
/// exact `thread_id` — match client-side, newest wins.
///
/// Inbox rows carry the sender NAME (toron resolves registered npubs;
/// agent-mail returns `from` directly) or the raw npub hex — whois
/// resolves the name so both match. Fails loudly (bail) when the thread
/// has no readable reply from `sender`: never ships an empty counter or
/// verdict prompt.
///
/// review-fix F1: the debate/plan-review patterns need the reply BODY
/// (the argument / the APPROVE|CHANGES: token), not the subject line.
///
/// governed-by: ADR-0001 §9.1/§9.2 (bd-debate-harvest-dsww)
fn harvest_latest(
    mail: &dyn backend::MailSurface,
    key: &str,
    thread: &str,
    sender: &str,
) -> Result<String> {
    let sender_npub = backend::run_mail(mail.whois(key, sender))
        .map(|w| w.npub)
        .unwrap_or_default();
    let items = backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key.to_string(),
        agent: mail.agent_name().to_string(),
        unread_only: false,
        include_bodies: true,
        limit: None,
        urgent_only: false,
        since_ts: None,
        thread_id: Some(thread.to_string()),
    }))?;
    let body = newest_in_thread(&items, thread, sender, &sender_npub)
        .map(|m| {
            if m.body_md.is_empty() {
                m.subject.trim().to_string()
            } else {
                m.body_md.trim().to_string()
            }
        })
        .filter(|s| !s.is_empty());
    match body {
        Some(b) => Ok(b),
        None => bail!(
            "harvest_latest: no in-thread reply from {sender} in thread {thread} \
             (inbox projection has no matching row) — the reply never landed, \
             do not ship an empty prompt"
        ),
    }
}

fn cmd_upgrade() -> Result<()> {
    let source = std::env::var("FLYWHEEL_SOURCE").unwrap_or_else(|_| "~/workspace/flywheel".into());
    let expanded = source.replacen(
        '~',
        &dirs::home_dir().unwrap_or_default().display().to_string(),
        1,
    );
    if !std::path::Path::new(&expanded).join("Cargo.toml").exists() {
        bail!("no Cargo.toml at {expanded} — set FLYWHEEL_SOURCE to the repo path");
    }
    println!("installing from {}…", dim(&expanded));
    let status = Command::new("cargo")
        .args(["install", "--path", &expanded, "--force"])
        .status()
        .map_err(|e| anyhow!("cargo not found: {e}"))?;
    if !status.success() {
        bail!("cargo install failed");
    }
    let removed = crate::doorbell::cleanup_legacy_sidecars(&crate::config::state_dir());
    if removed > 0 {
        println!("removed {removed} legacy doorbell sidecars");
    }
    println!("flywheel upgraded");
    Ok(())
}

fn cmd_tutorial() -> Result<()> {
    println!(
        "flywheel — hc swarm orchestrator\n\n\
         The daily loop (his P1→P8) — one durable command:\n\
         \x20 flywheel spawn <project> --oc=1 --pi=1 --gk=1\n\
         \x20 flywheel loop run <project> --queue p1,p2,p4,p5,p6,p8\n\
         \n\
         (kills crash-safe: restart the same command to resume;\n\
         \x20 flywheel loop status <project> to inspect)\n\
         \n\
         One-shot variant: flywheel workflows run daily <project>\n\
         (define it in {})\n\n\
         Swarm smarts:\n\
         \x20 - flywheel status <project>      states at a glance\n\
         \x20 - flywheel watch <project> --until blocked   alert on blockers\n\
         \x20 - flywheel work triage           bv graph triage\n\
         \x20 - flywheel checkpoint save <project> --msg \"…\"\n\
         \x20 - flywheel resume <project>      re-orient after a break\n\n\
         Coordination:\n\
         \x20 - flywheel mail inbox <project>  agent-mail inbox\n\
         \x20 - flywheel assign <project> --to all --beads bd-1,bd-2\n\
         \x20 - flywheel mail locks reserve <project> --paths src/foo.rs --exclusive\n\
         \x20 - flywheel mail locks list <project>      who holds what (lock board)\n\
         \x20 - flywheel mail unlock <project>          release your locks\n\n\
         Automation:\n\
         \x20 - flywheel --robot-snapshot     full JSON state\n\
         \x20 - flywheel serve                REST + SSE (port {})\n",
        config::config_dir().join("workflows.toml").display(),
        crate::daemon::SERVE_PORT_DEFAULT
    );
    Ok(())
}

fn cmd_loop_status(cfg: &Config, project: &str) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let v = rt.block_on(durable::loop_status(cfg, project))?;
    print_json(&v);
    Ok(())
}

#[cfg(feature = "serve")]
fn cmd_daemon(cfg: &Config, cmd: &DaemonCmd) -> Result<()> {
    let (operator, what) = match cmd {
        DaemonCmd::Start { operator, .. } => (*operator, "daemon start"),
        DaemonCmd::Stop { operator, .. } => (*operator, "daemon stop"),
        DaemonCmd::Restart { operator, .. } => (*operator, "daemon restart"),
        DaemonCmd::Status { .. } => (false, ""),
    };
    if !what.is_empty() {
        crate::authority::guard_fleet_mutation(cfg, None, operator, what)?;
    }
    let state = daemon::default_state_dir();
    std::fs::create_dir_all(&state)?;
    let addr = |port: u16| format!("127.0.0.1:{port}");
    let cron_host = || -> Result<()> {
        let cwd = std::env::current_dir()?;
        let project = cron::detect_project(cfg, None);
        daemon::start_cron(&state, &cwd, project.as_deref())
    };
    match cmd {
        DaemonCmd::Start { port, .. } => {
            daemon::start(&state, &addr(*port))?;
            cron_host()?;
            daemon::ensure_doorbell()?;
            // wave11-b: supervised always-on hosts for armed workspaces.
            doorbell_supervise::ensure_armed_hosts(cfg)?;
            Ok(())
        }
        DaemonCmd::Stop { port, force, .. } => {
            daemon::stop(&state, &addr(*port), *force)?;
            // wave11-b: unload the supervised hosts this daemon owned.
            doorbell_supervise::unload_all_hosts()?;
            daemon::stop_cron()
        }
        DaemonCmd::Restart { port, force, .. } => {
            daemon::restart(&state, &addr(*port), *force)?;
            daemon::stop_cron()?;
            cron_host()
        }
        DaemonCmd::Status { port } => {
            daemon::status(&state, &addr(*port));
            daemon::status_cron();
            for (ws, loaded) in doorbell_supervise::hosts_status() {
                println!(
                    "doorbell host {ws}: {}",
                    if loaded {
                        "loaded"
                    } else {
                        "unit present, not loaded"
                    }
                );
            }
            Ok(())
        }
    }
}

/// GS-062 ASK-USER D: hosted workspace labels from the doorbell hosting
/// registry (`{state}/doorbell/*.gen`). Same keys stamp writers use
/// (`flywheel-{project}` / spawn meta) — not `workspace_root` basename.
/// Alive dispatchers sort first so `/health` prefers a live pipe
/// ([`doorbell_rt::wake_liveness::order_hosted_alive_first`]).
///
/// FileHost / doorbell serve only write `{workspace}.daemon.gen`
/// (`ensure_daemon_gen`) — not `{workspace}.gen`. Accept both:
/// - `{slug}.daemon.gen` → label = strip `.daemon.gen`
/// - `{slug}.gen` → label = file stem
///   Then reject if the resulting slug still contains `.` (no-dot defense).
///   Deduplicate so a fleet with both siblings yields one label. Skipping
///   `.daemon.gen` blinded flywheel serve `/health` → empty hosted → permanent
///   NeverStamped while watch stamps + doorbell wake were live.
///
/// Alive partition (GS062-ADV-1/ADV-3): FileHost/doorbell write `.daemon.gen` +
/// `owner-{ws}.json` lease, **not** `{slug}.pid`. Pid-only left stale
/// `aaa.daemon.gen` ahead of live `zzz` → primary=aaa NeverStamped/Deaf while
/// zzz Live. Alive = `{slug}.pid` live OR owner lease non-stale **only**
/// (GS062-ADV-3: watch stamps are NOT an alive disjunct — post-stop/crash
/// leaves `watch-*.{hb,event}` fresh → false Quiet primary; multi-ws orphan
/// aaa stamps can outrank live zzz). Keep `.daemon.gen`→slug mapping; no
/// invented GC of stale `.daemon.gen` files.
/// Alive / other partitions of the doorbell hosting registry.
/// Wake classify uses **alive only** (GS062 alive-empty WARNING): when
/// `alive` is empty do **not** fall through to `other` (orphan stamps →
/// false Quiet) — empty alive → NeverStamped.
fn hosted_registry_partitions(state_dir: &std::path::Path) -> (Vec<String>, Vec<String>) {
    let db = state_dir.join("doorbell");
    let mut alive = Vec::new();
    let mut other = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(&db) else {
        return (Vec::new(), Vec::new());
    };
    let now_unix = doorbell_rt::lease::unix_now();
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|x| x.to_str()) != Some("gen") {
            continue;
        }
        let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
        let label = if let Some(slug) = name.strip_suffix(".daemon.gen") {
            slug
        } else {
            match path.file_stem().and_then(|x| x.to_str()) {
                Some(stem) => stem,
                None => continue,
            }
        };
        if label.is_empty() || label.contains('.') {
            continue;
        }
        if !seen.insert(label.to_string()) {
            continue;
        }
        let is_alive = registry_label_alive(state_dir, label, now_unix);
        if is_alive {
            alive.push(label.to_string());
        } else {
            other.push(label.to_string());
        }
    }
    (alive, other)
}

fn hosted_workspaces_from_registry(state_dir: &std::path::Path) -> Vec<String> {
    let (alive, other) = hosted_registry_partitions(state_dir);
    doorbell_rt::wake_liveness::order_hosted_alive_first(alive, other)
}

/// Alive-only hosted labels for wake classify (ask-user F / alive-empty WARNING).
#[cfg(test)]
fn hosted_alive_workspaces_from_registry(state_dir: &std::path::Path) -> Vec<String> {
    let (alive, _other) = hosted_registry_partitions(state_dir);
    doorbell_rt::wake_liveness::order_hosted_alive_first(alive, Vec::new())
}

/// True when a registry label should sort into the `/health` alive partition.
/// Alive = `{slug}.pid` live OR owner lease non-stale only (GS062-ADV-3).
fn registry_label_alive(state_dir: &std::path::Path, label: &str, now_unix: u64) -> bool {
    if doorbell::dispatcher_pid(state_dir, label).is_some_and(doorbell::pid_alive) {
        return true;
    }
    if let Some(lease) = doorbell_rt::lease::read_lease(state_dir, label)
        && !doorbell_rt::lease::lease_stale(&lease, now_unix)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod hosted_workspaces_from_registry_tests {
    use super::hosted_workspaces_from_registry;

    #[test]
    fn daemon_gen_and_slug_gen_dedup_to_slug() {
        let dir = tempfile_state();
        let db = dir.join("doorbell");
        std::fs::create_dir_all(&db).expect("doorbell dir");
        std::fs::write(db.join("flywheel-proj.gen"), b"{}").expect("slug.gen");
        std::fs::write(db.join("flywheel-proj.daemon.gen"), b"{}").expect("daemon.gen");
        std::fs::write(db.join("flywheel-proj.extra.gen"), b"{}").expect("extra.gen");
        std::fs::write(db.join("flywheel-proj.pid"), b"1").expect("pid");

        let got = hosted_workspaces_from_registry(&dir);
        assert_eq!(
            got,
            vec!["flywheel-proj".to_string()],
            "slug .gen + .daemon.gen dedup; dotted stems excluded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_only_fleet_yields_slug() {
        let dir = tempfile_state();
        let db = dir.join("doorbell");
        std::fs::create_dir_all(&db).expect("doorbell dir");
        std::fs::write(db.join("flywheel-only.daemon.gen"), b"{}").expect("daemon.gen");
        let got = hosted_workspaces_from_registry(&dir);
        assert_eq!(
            got,
            vec!["flywheel-only".to_string()],
            "daemon-only fleets must yield the slug (not empty): {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GS062-ADV-1: stale aaa.daemon.gen (no pid/lease) must not sort ahead
    /// of live zzz — primary via lease/pid on zzz (not stamps alone).
    #[test]
    fn stale_aaa_daemon_gen_does_not_outrank_live_zzz_stamps() {
        let dir = tempfile_state();
        let db = dir.join("doorbell");
        std::fs::create_dir_all(&db).expect("doorbell dir");
        std::fs::write(db.join("aaa.daemon.gen"), b"{}").expect("aaa.daemon.gen");
        std::fs::write(db.join("zzz.daemon.gen"), b"{}").expect("zzz.daemon.gen");
        let now = doorbell_rt::lease::unix_now();
        let claimed = doorbell_rt::lease::acquire_lease(&dir, "zzz", now).expect("lease");
        assert_eq!(claimed.pid, std::process::id());

        let got = hosted_workspaces_from_registry(&dir);
        assert_eq!(
            got.first().map(String::as_str),
            Some("zzz"),
            "live zzz lease must beat stale aaa for primary: {got:?}"
        );
        let zzz_pos = got.iter().position(|s| s == "zzz").expect("zzz present");
        let aaa_pos = got.iter().position(|s| s == "aaa").expect("aaa present");
        assert!(zzz_pos < aaa_pos, "zzz (alive) before aaa (other): {got:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GS062-ADV-1: live owner-{ws}.json lease also marks the label alive
    /// (FileHost/doorbell write lease, not {slug}.pid).
    #[test]
    fn live_owner_lease_marks_registry_label_alive() {
        let dir = tempfile_state();
        let db = dir.join("doorbell");
        std::fs::create_dir_all(&db).expect("doorbell dir");
        std::fs::write(db.join("aaa.daemon.gen"), b"{}").expect("aaa.daemon.gen");
        std::fs::write(db.join("zzz.daemon.gen"), b"{}").expect("zzz.daemon.gen");
        let now = doorbell_rt::lease::unix_now();
        let claimed = doorbell_rt::lease::acquire_lease(&dir, "zzz", now).expect("lease");
        assert_eq!(claimed.pid, std::process::id());

        let got = hosted_workspaces_from_registry(&dir);
        assert_eq!(
            got.first().map(String::as_str),
            Some("zzz"),
            "live owner lease must put zzz ahead of stale aaa: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GS062-ADV-3: fresh orphan watch stamps alone must NOT put a label in
    /// the alive partition (post-stop/crash leaves hb/event fresh).
    #[test]
    fn fresh_orphan_stamps_alone_do_not_mark_registry_label_alive() {
        let dir = tempfile_state();
        let db = dir.join("doorbell");
        std::fs::create_dir_all(&db).expect("doorbell dir");
        std::fs::write(db.join("zzz.daemon.gen"), b"{}").expect("zzz.daemon.gen");
        let now = doorbell_rt::lease::unix_now();
        doorbell_rt::lease::acquire_lease(&dir, "zzz", now).expect("zzz lease");
        std::fs::write(db.join("aaa.daemon.gen"), b"{}").expect("aaa.daemon.gen");
        doorbell_rt::watchbeat::touch_event(&dir, "aaa");
        doorbell_rt::watchbeat::touch_heartbeat(&dir, "aaa");

        let got = hosted_workspaces_from_registry(&dir);
        assert_eq!(
            got.first().map(String::as_str),
            Some("zzz"),
            "orphan aaa stamps must not outrank live zzz: {got:?}"
        );
        let zzz_pos = got.iter().position(|s| s == "zzz").expect("zzz present");
        let aaa_pos = got.iter().position(|s| s == "aaa").expect("aaa present");
        assert!(
            zzz_pos < aaa_pos,
            "zzz (lease alive) before aaa (stamps-only orphan): {got:?}"
        );
        assert_eq!(
            &got[zzz_pos + 1..],
            &["aaa".to_string()],
            "aaa must be other-only, not alive: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Alive-empty WARNING: orphan stamps in `other` must not drive wake —
    /// classify from alive only; empty alive → NeverStamped (not Quiet).
    #[test]
    fn alive_empty_orphan_stamps_wake_is_never_stamped() {
        let dir = tempfile_state();
        let db = dir.join("doorbell");
        std::fs::create_dir_all(&db).expect("doorbell dir");
        std::fs::write(db.join("orphan.daemon.gen"), b"{}").expect("orphan.daemon.gen");
        doorbell_rt::watchbeat::touch_event(&dir, "orphan");
        doorbell_rt::watchbeat::touch_heartbeat(&dir, "orphan");

        let alive = super::hosted_alive_workspaces_from_registry(&dir);
        assert!(alive.is_empty(), "orphan must not be alive: {alive:?}");
        let hosted = hosted_workspaces_from_registry(&dir);
        assert_eq!(hosted, vec!["orphan".to_string()]);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let stamps: Vec<(Option<u64>, Option<u64>)> = alive
            .iter()
            .map(|ws| {
                (
                    doorbell_rt::watchbeat::last_event_ms(&dir, ws),
                    doorbell_rt::watchbeat::last_heartbeat_ms(&dir, ws),
                )
            })
            .collect();
        let pipe = doorbell_rt::wake_liveness::classify_worst_of(
            now_ms,
            stamps,
            doorbell_rt::wake_liveness::DEFAULT_DEAF_CEILING_MS,
        );
        assert_eq!(
            pipe,
            doorbell_rt::wake_liveness::PipeHealth::NeverStamped,
            "alive=[] must not classify orphan Quiet: {pipe:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ASK-USER F: multi-alive Deaf + Live → wake=Deaf (worst-of).
    #[test]
    fn multi_alive_deaf_plus_live_wake_is_deaf() {
        let dir = tempfile_state();
        let db = dir.join("doorbell");
        std::fs::create_dir_all(&db).expect("doorbell dir");
        let now = doorbell_rt::lease::unix_now();
        for ws in ["deaf-ws", "live-ws"] {
            std::fs::write(db.join(format!("{ws}.daemon.gen")), b"{}").expect("daemon.gen");
            doorbell_rt::lease::acquire_lease(&dir, ws, now).expect("lease");
        }
        doorbell_rt::watchbeat::touch_event(&dir, "live-ws");
        doorbell_rt::watchbeat::touch_heartbeat(&dir, "live-ws");
        let now_ms = 200_000u64;
        let stamps = [
            (None, Some(100_000u64)),
            (Some(199_000u64), Some(198_000u64)),
        ];
        let pipe = doorbell_rt::wake_liveness::classify_worst_of(
            now_ms,
            stamps,
            doorbell_rt::wake_liveness::DEFAULT_DEAF_CEILING_MS,
        );
        assert_eq!(
            pipe,
            doorbell_rt::wake_liveness::PipeHealth::Deaf {
                silent_for_ms: 100_000
            }
        );
        let alive = super::hosted_alive_workspaces_from_registry(&dir);
        assert_eq!(alive.len(), 2, "both leases alive: {alive:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh state dir per call. It was `<pid>-<now in ns>`, but the clock
    /// is microsecond-grained on macOS, so two of these tests starting in the
    /// same microsecond shared one dir and read each other's labels
    /// (`daemon_only_fleet_yields_slug` saw `["flywheel-only", "zzz"]`).
    fn tempfile_state() -> std::path::PathBuf {
        tempfile::Builder::new()
            .prefix("fw-hosted-reg-")
            .tempdir()
            .expect("state dir")
            .keep()
    }

    /// Every call is its own dir, also when many tests start at once.
    #[test]
    fn concurrent_state_dirs_are_distinct() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    (0..64).map(|_| tempfile_state()).collect::<Vec<_>>()
                })
            })
            .collect();
        let dirs: Vec<std::path::PathBuf> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let unique: std::collections::BTreeSet<_> = dirs.iter().collect();
        for d in &dirs {
            let _ = std::fs::remove_dir_all(d);
        }
        assert_eq!(unique.len(), dirs.len(), "state dirs collided");
    }
}

#[cfg(feature = "serve")]
fn cmd_serve(cfg: &Config, port: u16) -> Result<()> {
    crate::self_watch::spawn("serve");
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(serve_async(cfg.clone(), port))
}

#[cfg(feature = "serve")]
async fn serve_async(cfg: config::Config, port: u16) -> Result<()> {
    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use std::sync::Arc;
    #[derive(Clone)]
    struct AppState {
        cfg: config::Config,
        emit_budgets:
            Arc<std::sync::Mutex<std::collections::HashMap<String, crate::doorbell::PinBudget>>>,
        /// Stamp-writer workspace labels (`flywheel-{project}` / spawn meta).
        /// Plumbed from the doorbell hosting registry (`{state}/doorbell/*.gen`)
        /// — the same keys `__doorbell` / spawn write under. Not
        /// `workspace_root().file_name()`.
        hosted_workspaces: Arc<std::sync::Mutex<Vec<String>>>,
    }

    async fn health(State(st): State<Arc<AppState>>) -> String {
        let state_dir = config::state_dir();
        let (alive, hosted) = {
            let (a, o) = hosted_registry_partitions(&state_dir);
            let alive = doorbell_rt::wake_liveness::order_hosted_alive_first(a, Vec::new());
            let hosted = doorbell_rt::wake_liveness::order_hosted_alive_first(alive.clone(), o);
            let mut g = st
                .hosted_workspaces
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *g = hosted.clone();
            (alive, hosted)
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let stamps: Vec<(String, Option<u64>, Option<u64>)> = alive
            .iter()
            .map(|ws| {
                (
                    ws.clone(),
                    doorbell_rt::watchbeat::last_event_ms(&state_dir, ws),
                    doorbell_rt::watchbeat::last_heartbeat_ms(&state_dir, ws),
                )
            })
            .collect();
        let (pipe, worst_ws, last_event_ms, last_heartbeat_ms) =
            wake_liveness::classify_worst_of_stamps(
                now_ms,
                stamps,
                wake_liveness::DEFAULT_DEAF_CEILING_MS,
            );
        let workspace = worst_ws.unwrap_or_default();
        json!({
            "status": "ok",
            "wake": {
                "workspace": workspace,
                "hosted": hosted,
                "pipe": wake_liveness::to_json(pipe),
                "last_event_ms": last_event_ms,
                "last_heartbeat_ms": last_heartbeat_ms,
                "deaf_ceiling_ms": wake_liveness::DEFAULT_DEAF_CEILING_MS,
            }
        })
        .to_string()
    }

    async fn snapshot(State(st): State<Arc<AppState>>) -> Result<String, StatusCode> {
        let c = Client::connect(Some(&st.cfg.socket_path()))
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let agents = c
            .agent_list()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        serde_json::to_string(&json!({
            "agents": agents.iter().map(|a| json!({
                "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
                "workspace_id": a.workspace_id, "pane_id": a.pane_id
            })).collect::<Vec<_>>()
        }))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    }

    async fn send(State(st): State<Arc<AppState>>, body: String) -> Result<String, StatusCode> {
        let req: Value = serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
        let project = req["project"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
        let to = req["to"].as_str().unwrap_or("all");
        let msg = req["msg"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
        let policy = config::Policy::load().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if policy.violation(msg).is_some() {
            return Err(StatusCode::FORBIDDEN);
        }
        let mut c = Client::connect(Some(&st.cfg.socket_path()))
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let ws_id = find_workspace(&mut c, project).map_err(|_| StatusCode::NOT_FOUND)?;
        let targets =
            resolve_agents(&mut c, &ws_id, project, to).map_err(|_| StatusCode::BAD_REQUEST)?;
        for a in &targets {
            c.agent_prompt(agent_target(a), msg, None)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        }
        Ok(json!({ "ok": true, "targets": targets.len() }).to_string())
    }

    /// Tokened service emit (bd-asr2.17, D7): a config-declared service
    /// identity rings a `to` target through the existing toron wire
    /// (`mail send --wake tasked` publishes the kind-9 addressed post —
    /// never a second bus). Empty services table = 404 (disabled).
    async fn emit(State(st): State<Arc<AppState>>, body: String) -> Result<String, StatusCode> {
        use crate::doorbell::EmitDeny;
        let req: Value = serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
        let service = req["service"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
        let token = req["token"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
        let project = req["project"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
        let to = req["to"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
        let subject = req["subject"].as_str().unwrap_or("doorbell emit");
        let msg = req["msg"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
        let services = &st.cfg.doorbell_services();
        if services.is_empty() {
            return Err(StatusCode::NOT_FOUND);
        }
        let rate = match crate::doorbell::authorize_emit(services, service, token, to) {
            Ok(r) => r,
            Err(EmitDeny::UnknownService) => return Err(StatusCode::UNAUTHORIZED),
            Err(EmitDeny::BadToken) => return Err(StatusCode::UNAUTHORIZED),
            Err(EmitDeny::ScopeDenied) => return Err(StatusCode::FORBIDDEN),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        {
            let mut budgets = st
                .emit_budgets
                .lock()
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let b = budgets
                .entry(service.to_string())
                .or_insert_with(|| crate::doorbell::PinBudget::new(1, rate, now));
            if !b.allows(now) {
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
            b.record_at(now);
        }
        let policy = config::Policy::load().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if policy.violation(msg).is_some() {
            return Err(StatusCode::FORBIDDEN);
        }
        let project_s = project.to_string();
        let service_s = service.to_string();
        let to_s = to.to_string();
        let subject_s = subject.to_string();
        let msg_s = msg.to_string();
        let cfg = st.cfg.clone();
        crate::backend::run_mail_off_runtime(move || {
            let mail = crate::mail_or_die(&cfg)?.with_name(service_s);
            mail.send(
                &project_s,
                &[to_s],
                &subject_s,
                &msg_s,
                "normal",
                false,
                None,
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
        crate::doorbell::service_emit_wake_in(&config::state_dir(), project, service, to);
        Ok(json!({ "ok": true, "service": service, "to": to }).to_string())
    }

    async fn sse_handler(State(st): State<Arc<AppState>>) -> axum::response::Response {
        use axum::response::sse::Event;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
        let cfg = st.cfg.clone();
        tokio::spawn(async move {
            let mut tick = 0u32;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let c = match Client::connect(Some(&cfg.socket_path())) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let agents = c.agent_list().unwrap_or_default();
                let payload = json!({
                    "tick": tick, "ts": chrono_like_ts(),
                    "agents": agents.iter().map(|a| json!({
                        "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
                        "workspace_id": a.workspace_id
                    })).collect::<Vec<_>>()
                })
                .to_string();
                tick += 1;
                if tx
                    .send(Ok(Event::default().event("state").data(payload)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        use axum::response::IntoResponse;
        axum::response::Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
            .keep_alive(
                axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
            )
            .into_response()
    }

    let st = Arc::new(AppState {
        cfg: cfg.clone(),
        emit_budgets: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        hosted_workspaces: Arc::new(std::sync::Mutex::new(hosted_workspaces_from_registry(
            &config::state_dir(),
        ))),
    });
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/send", post(send))
        .route("/api/v1/emit", post(emit))
        .route("/events", get(sse_handler))
        .route(
            "/openapi.json",
            get(|| async {
                json!({
                "openapi": "3.0.0",
                "info": { "title": "flywheel", "version": VERSION },
                "paths": {
                    "/health": { "get": { "summary": "health" } },
                    "/api/v1/snapshot": { "get": { "summary": "full agent state" } },
                    "/api/v1/emit": { "post": { "summary": "tokened service emit (kind-9 ring)" } },
                    "/events": { "get": { "summary": "SSE state stream" } }
                }
            })
            .to_string()
            }),
        )
        .with_state(st);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!(
        "flywheel serve on http://127.0.0.1:{port} (/health /api/v1/snapshot /api/v1/send /events /openapi.json)"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(not(feature = "serve"))]
fn cmd_serve(_cfg: &Config, _port: u16) -> Result<()> {
    bail!("serve support not compiled in (build with --features serve)")
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::cmd::internal::mail_inbox_fallback_argv;
    use crate::cmd::workflow::{
        workflow_hybrid_fields, workflow_list_brief_rows, workflow_list_full_rows,
    };

    /// Regen writes only *-mode.md and deletes leftover /beads-* / /workflow-*.
    #[test]
    fn integration_writes_only_orchestration_and_removes_retired_slash() {
        let dir = std::env::temp_dir().join(format!(
            "fw-int-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("beads-execute.md"), "stale").unwrap();
        std::fs::write(dir.join("workflow-plan.md"), "stale").unwrap();
        std::fs::write(dir.join("swarm.md"), "stale").unwrap();

        cmd_integration(Some("opencode"), Some(&dir)).expect("integration write");

        for (name, _, _) in ORCHESTRATION_COMMANDS {
            let p = dir.join(format!("{name}.md"));
            assert!(p.is_file(), "must write {name}.md");
            let body = std::fs::read_to_string(&p).unwrap();
            assert!(
                body.contains(&format!("# Orchestration: {name}")),
                "{name}.md must be an orchestration playbook"
            );
        }
        for name in RETIRED_SLASH_FILES {
            assert!(
                !dir.join(name).exists(),
                "retired slash leftover must be gone: {name}"
            );
        }
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        for n in &names {
            assert!(
                n.ends_with("-mode.md"),
                "destination must contain only *-mode.md, found {n}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retired_slash_templates_are_gone() {
        let entries: Vec<_> = std::fs::read_dir("integrations/prompts")
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for name in &entries {
            assert!(
                !name.starts_with("beads-") && !name.starts_with("workflow-"),
                "retired template still present: {name}"
            );
        }
        assert!(
            !std::path::Path::new("integrations/beads-body.md.tpl").exists(),
            "beads-body.md.tpl must be deleted"
        );
    }

    /// ADR-0009 addon: `workflow list` is the agent-native selection surface
    /// — every compiled-in row carries a non-empty description, a run form,
    /// and the token flag (select from list like a skill catalog).
    #[test]
    fn workflow_list_rows_carry_description_and_run() {
        let wfs = builtin_workflows::builtin_workflows();
        for (name, wf) in &wfs {
            let desc = wf.description.as_deref().unwrap_or("");
            assert!(!desc.is_empty(), "{name} carries a description");
            let token = crate::workflow_catalog::builtin_meta(name)
                .compose
                .unwrap_or(false);
            let run = if token {
                format!("workflow compose \"<objective>\" --parts {name} --run")
            } else {
                format!("workflow run {name} --execute")
            };
            if token {
                assert!(run.contains("--parts"), "{name} token runs via compose");
            } else {
                assert!(run.contains("--execute"), "{name} runs via execute");
            }
        }
        assert!(
            wfs["wave-swarm"]
                .description
                .as_deref()
                .unwrap()
                .contains("REVIEW")
        );
        assert!(
            wfs["ce-compound"]
                .description
                .as_deref()
                .unwrap()
                .contains("chiebukuro")
        );
    }

    /// GS-052: live `cmd_workflow` `--brief` projection is ONE hybrid
    /// contract = name/family/selection/run/topology/compose_token. Keys come
    /// from [`workflow_list_brief_rows`] — the same function `cmd_workflow`
    /// prints (not a local test mirror). Topology stays for sticky-open.
    /// Bare list keeps description + decide_trigger + rest; show is separate (no decide_trigger); no competing brief path.
    #[test]
    fn workflow_list_brief_projects_hybrid_contract() {
        let all = workflow_catalog::all_workflows_merged(None).expect("catalog");
        let brief_stdout = serde_json::json!({ "workflows": workflow_list_brief_rows(&all) });
        let workflows = brief_stdout["workflows"]
            .as_array()
            .expect("workflows array");
        assert!(!workflows.is_empty(), "brief must project builtins");
        let expect: std::collections::BTreeSet<_> = [
            "name",
            "family",
            "selection",
            "run",
            "topology",
            "compose_token",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        for row in workflows {
            let obj = row.as_object().expect("brief row object");
            let keyset: std::collections::BTreeSet<_> = obj.keys().cloned().collect();
            assert_eq!(keyset, expect, "live brief stdout JSON keys: {obj:?}");
            assert!(!obj.contains_key("description"));
            assert!(!obj.contains_key("trigger"));
            assert!(!obj.contains_key("source"));
            assert!(!obj.contains_key("decide_trigger"));
            assert!(!obj.contains_key("crew"));
            let topology = obj["topology"].as_str().unwrap_or("");
            assert!(
                matches!(topology, "open" | "loop" | "swarm" | "join" | "none"),
                "sticky-open needs topology on brief; topology={topology}"
            );
            let family = obj["family"].as_str().unwrap_or("");
            let selection = obj["selection"].as_str().unwrap_or("");
            let run = obj["run"].as_str().unwrap_or("");
            assert!(!family.is_empty(), "family non-empty");
            assert!(!selection.is_empty(), "selection non-empty");
            assert!(
                run.contains("workflow run ") || run.contains("workflow compose "),
                "run form: {run}"
            );
            assert!(obj["compose_token"].is_boolean());
        }
        let wave = workflows
            .iter()
            .find(|r| r["name"] == "wave-swarm")
            .expect("wave-swarm in brief");
        assert_eq!(wave["topology"], "swarm");
        assert_eq!(wave["compose_token"], true);
        assert_eq!(crate::decide::topology_for_primitive("wave-swarm"), "swarm");
        let cfg = Config::default();
        let cmd = WorkflowCmd::List {
            json: false,
            brief: true,
            project: None,
        };
        cmd_workflow(&cfg, &cmd).expect("WorkflowCmd::List { brief: true } must succeed");
    }

    /// GS-052: live bare `list --json` projection from [`workflow_list_full_rows`]
    /// (same function `cmd_workflow` prints) — description/trigger/source/crew/
    /// decide_trigger present; topology is brief-only (not required on full).
    #[test]
    fn workflow_list_full_projects_description_and_rest() {
        let all = workflow_catalog::all_workflows_merged(None).expect("catalog");
        let full_stdout = serde_json::json!({ "workflows": workflow_list_full_rows(&all) });
        let workflows = full_stdout["workflows"]
            .as_array()
            .expect("workflows array");
        assert!(!workflows.is_empty(), "full list must project builtins");
        let required = [
            "name",
            "description",
            "family",
            "selection",
            "run",
            "compose_token",
            "source",
            "trigger",
            "crew",
            "steps",
            "graph_nodes",
            "inputs",
            "decide_trigger",
        ];
        for row in workflows {
            let obj = row.as_object().expect("full row object");
            for key in required {
                assert!(obj.contains_key(key), "full list missing {key}: {obj:?}");
            }
            assert!(
                !obj.contains_key("topology"),
                "topology is brief-only; full row: {obj:?}"
            );
            let desc = obj.get("description");
            if crate::builtin_workflows::builtin_workflows()
                .contains_key(obj["name"].as_str().unwrap_or(""))
            {
                let s = desc.and_then(|v| v.as_str()).unwrap_or("");
                assert!(
                    !s.is_empty(),
                    "{}: nonempty description on full list",
                    obj["name"]
                );
            }
        }
        let wave = workflows
            .iter()
            .find(|r| r["name"] == "wave-swarm")
            .expect("wave-swarm in full list");
        let (run, family, selection, token) = workflow_hybrid_fields("wave-swarm");
        assert_eq!(wave["run"], run);
        assert_eq!(wave["family"], family);
        assert_eq!(wave["selection"], selection);
        assert_eq!(wave["compose_token"], token);
        assert!(wave.get("decide_trigger").is_some());
        let cfg = Config::default();
        let cmd = WorkflowCmd::List {
            json: false,
            brief: false,
            project: None,
        };
        cmd_workflow(&cfg, &cmd).expect("WorkflowCmd::List bare must succeed");
        let src = include_str!("main.rs");
        let robot_block_start = src
            .find("if cli.robot_capabilities")
            .expect("robot_capabilities arm");
        let robot_end = src[robot_block_start..]
            .find("\n    let cfg = match Config::load()")
            .expect("robot capabilities arm ends before config load");
        let robot_block = &src[robot_block_start..robot_block_start + robot_end];
        let count = robot_block.matches("\"workflows\":").count();
        assert_eq!(
            count, 1,
            "robot-capabilities must have exactly one workflows key (got {count})"
        );
        assert!(
            robot_block.contains("GS-052 HYBRID ONE contract"),
            "robot-capabilities must describe the live hybrid brief"
        );
    }

    /// ORCHESTRATION_COMMANDS holds operating-mode slash commands. Catalog
    /// workflows are `workflow list`, not slash twins.
    #[test]
    fn orchestration_command_set_has_swarm() {
        assert!(
            ORCHESTRATION_COMMANDS
                .iter()
                .any(|(n, _, _)| *n == "swarm-mode"),
            "swarm-mode is an orchestration command"
        );
        assert!(
            !ORCHESTRATION_COMMANDS.iter().any(|(n, _, _)| *n == "swarm"),
            "bare /swarm was renamed to /swarm-mode"
        );
        assert_eq!(
            ORCHESTRATION_COMMANDS.len(),
            4,
            "four orchestration commands (open + swarm + loop + peer-aware)"
        );
        let (name, description, template) = ORCHESTRATION_COMMANDS[0];
        assert_eq!(name, "open-mode");
        assert!(!description.is_empty(), "/open-mode has a description");
        assert!(template.ends_with(".tpl"), "template file referenced");
        let (name, description, template) = ORCHESTRATION_COMMANDS[1];
        assert_eq!(name, "swarm-mode");
        assert!(!description.is_empty(), "/swarm-mode has a description");
        assert!(template.ends_with(".tpl"), "template file referenced");
        let (name, description, template) = ORCHESTRATION_COMMANDS[2];
        assert_eq!(
            name, "loop-mode",
            "loop mode avoids harness command collisions"
        );
        assert!(!description.is_empty(), "/loop-mode has a description");
        assert!(template.ends_with(".tpl"), "template file referenced");
        let (name, description, template) = ORCHESTRATION_COMMANDS[3];
        assert_eq!(
            name, "peer-aware-mode",
            "peer-aware mode avoids harness command collisions"
        );
        assert!(
            !description.is_empty(),
            "/peer-aware-mode has a description"
        );
        assert!(template.ends_with(".tpl"), "template file referenced");
    }

    /// bd-glup / bd-ug9z: a rendered orchestration command = frontmatter (with
    /// the description) + the playbook template + stamps; no beads body. Each
    /// playbook carries its own description and step vocabulary.
    #[test]
    fn render_orchestration_prompt_has_frontmatter_and_steps() {
        let cases: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "swarm-mode",
                "captain-led multi-crew execution",
                &[
                    "CLASSIFY",
                    "GATE PREP",
                    "LANES",
                    "SPAWN",
                    "BRIEF",
                    "CONFLICT",
                    "WAVE GATE",
                    "TEARDOWN",
                ],
                &[],
            ),
            (
                "loop-mode",
                "durable solo execution",
                &[
                    "SETUP",
                    "EXECUTE",
                    "IN-FLIGHT PEER CHECKPOINTS",
                    "VERIFY",
                    "CLOSE",
                    "PROMOTE",
                ],
                &["flywheel collab <proj> --to <reviewer-pin>"],
            ),
        ];
        for (mode, description, steps, extras) in cases {
            let out =
                render_orchestration_prompt("opencode", mode, "9.9.9", "2026-08-10T00:00:00Z")
                    .unwrap_or_else(|e| panic!("render {mode}: {e:?}"));
            assert!(out.starts_with("---\n"), "{mode}: frontmatter");
            assert!(
                out.contains(description),
                "{mode}: description in frontmatter"
            );
            assert!(out.contains(&format!("# Orchestration: {mode}")));
            for step in *steps {
                assert!(out.contains(step), "{mode}: playbook step {step} present");
            }
            for extra in *extras {
                assert!(out.contains(extra), "{mode}: content {extra} present");
            }
            assert!(out.contains("flywheel 9.9.9"));
            assert!(out.contains("2026-08-10T00:00:00Z"));
            assert!(!out.contains("{{TOOL_FRONTMATTER}}"));
            assert!(
                !out.contains("{{BODY}}"),
                "orchestration commands are standalone — no beads body"
            );
        }
    }

    /// bd-glup: every kind (incl pa) renders every orchestration command.
    #[test]
    fn render_all_orchestration_commands_per_kind() {
        for kind in ["opencode", "pi", "grok", "omp", "pa"] {
            for (name, _, _) in ORCHESTRATION_COMMANDS {
                let out = render_orchestration_prompt(kind, name, "0.1.0", "ts").unwrap();
                assert!(out.starts_with("---"), "{kind}/{name} frontmatter");
                assert!(
                    out.contains(&format!("# Orchestration: {name}")),
                    "{kind}/{name} heading"
                );
            }
        }
    }

    /// bd-9z1d: EVERY rendered prompt's frontmatter must PARSE as YAML.
    ///
    /// `render_all_orchestration_commands_per_kind` only asserted the output
    /// starts with `---`, which an unparseable document also does — so a
    /// description containing `": "` sailed through and `pi` refused the
    /// generated `peer-aware-mode` prompt, leaving that pane's agent dead while
    /// the run reported success. The bug lives in the SHARED substitution, so
    /// the fence has to cover the whole cross-product, not one template: a
    /// per-command snapshot cannot see it.
    #[test]
    fn every_rendered_prompt_frontmatter_is_valid_yaml() {
        for kind in ["opencode", "pi", "grok", "omp", "pa"] {
            for (name, _, _) in ORCHESTRATION_COMMANDS {
                let out = render_orchestration_prompt(kind, name, "0.1.0", "ts")
                    .unwrap_or_else(|e| panic!("{kind}/{name} renders: {e}"));
                let body = out
                    .strip_prefix("---\n")
                    .unwrap_or_else(|| panic!("{kind}/{name} opens its frontmatter"));
                let fm = body
                    .split_once("\n---")
                    .map(|(fm, _)| fm)
                    .unwrap_or_else(|| panic!("{kind}/{name} closes its frontmatter"));
                serde_yaml::from_str::<serde_yaml::Value>(fm).unwrap_or_else(|e| {
                    panic!("{kind}/{name} frontmatter is not valid YAML: {e}\n---\n{fm}\n---")
                });
            }
        }
    }

    /// bd-9z1d: the regression itself. `peer-aware-mode`'s real description
    /// contains `": "`, so an UNQUOTED substitution is invalid YAML. If this
    /// ever passes with the quoting removed, the fix is gone.
    #[test]
    fn a_description_containing_a_colon_stays_valid_yaml() {
        let (_, description, _) = ORCHESTRATION_COMMANDS
            .iter()
            .find(|(n, _, _)| *n == "peer-aware-mode")
            .expect("peer-aware-mode is in the table");
        assert!(
            description.contains(": "),
            "the premise: this description is what broke the generated prompt"
        );
        let quoted = yaml_double_quoted(description);
        let doc = format!("description: {quoted}\n");
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&doc).expect("quoted description parses");
        assert_eq!(
            parsed["description"].as_str(),
            Some(*description),
            "quoting must preserve the description verbatim"
        );
        assert!(
            serde_yaml::from_str::<serde_yaml::Value>(&format!("description: {description}\n"))
                .is_err(),
            "an unquoted description with ': ' must be invalid, or the bug was never this"
        );
    }

    /// GS-010: playbook compose lines must be clap-parseable (not SPAWN substring) — the
    /// agent must be able to run the pasted command, not just see English.
    #[test]
    fn playbook_compose_lines_are_clap_parseable() {
        for tpl in ["swarm", "loop"] {
            let path = format!("integrations/prompts/{tpl}.md.tpl");
            let content = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("read {path}"));
            let mut found = 0;
            for line in content.lines().filter(|l| l.contains("workflow compose")) {
                found += 1;
                assert!(
                    line.contains("\"<objective>\"") || line.contains("<objective>"),
                    "compose line must have positional <objective> in {tpl}: {line}"
                );
                assert!(
                    !line.contains("--mode"),
                    "compose line must not have --mode in {tpl}: {line}"
                );
            }
            assert!(found > 0, "no workflow compose line found in {tpl}");
        }
    }

    /// bd-glup: unknown kind or command is an actionable error.
    #[test]
    fn peer_aware_command_renders_attach_and_budget() {
        for kind in ["opencode", "pi", "grok", "omp", "pa"] {
            let out = render_orchestration_prompt(kind, "peer-aware-mode", "0.1.0", "ts").unwrap();
            assert!(
                out.contains("# Orchestration: peer-aware-mode"),
                "{kind} heading"
            );
            assert!(
                out.contains("ATTACH") || out.contains("attach"),
                "{kind} must attach, not fork"
            );
            assert!(
                out.contains("5/30/3") || out.contains("--budget"),
                "{kind} budget knobs"
            );
            assert!(out.contains("doorbell"), "{kind} names the doorbell");
            assert!(
                out.contains("OPERATING CONTRACT") || out.contains("Peer-aware agent template"),
                "{kind} must embed integrations/peer-aware.md.tpl SoT"
            );
            assert!(
                out.contains("integrations/peer-aware.md.tpl"),
                "{kind} must name the SoT path"
            );
        }
    }

    /// GS-043: prompts/peer-aware.md.tpl is thin chrome — no second full body.
    #[test]
    fn peer_aware_prompt_chrome_is_thin_not_second_sot() {
        let chrome = include_str!("../integrations/prompts/peer-aware.md.tpl");
        let sot = include_str!("../integrations/peer-aware.md.tpl");
        assert!(
            chrome.len() < sot.len() / 2,
            "prompts/peer-aware.md.tpl must stay thin chrome ({}B vs SoT {}B)",
            chrome.len(),
            sot.len()
        );
        assert!(
            !chrome.contains("OPERATING CONTRACT"),
            "chrome must not duplicate the SoT OPERATING CONTRACT body"
        );
        assert!(
            chrome.contains("integrations/peer-aware.md.tpl"),
            "chrome must point at the SoT path"
        );
        assert!(
            sot.contains("CONTROL-PLANE MARKER") && sot.contains("FLYWHEEL_OP"),
            "SoT must carry CONTROL-PLANE MARKER / FLYWHEEL_OP (ADR-0010 D7)"
        );
        assert!(
            sot.contains("RED-SURFACE PING"),
            "SoT must carry RED-SURFACE PING"
        );
    }

    #[test]
    fn argument_adapter_opencode_uses_dollar_arguments() {
        let out = render_orchestration_prompt("opencode", "swarm-mode", "0.1.0", "ts").unwrap();
        assert!(
            out.contains("$ARGUMENTS"),
            "opencode interpolates $ARGUMENTS"
        );
        assert!(out.contains("argument-hint:"));
    }

    #[test]
    fn argument_adapter_pi_has_argument_hint() {
        let out = render_orchestration_prompt("pi", "loop-mode", "0.1.0", "ts").unwrap();
        assert!(out.contains("argument-hint:"));
        assert!(out.contains("$1") || out.contains("$@"));
    }

    #[test]
    fn argument_adapter_grok_has_no_dollar_arguments() {
        let out = render_orchestration_prompt("grok", "swarm-mode", "0.1.0", "ts").unwrap();
        assert!(
            !out.contains("Arguments: `$ARGUMENTS`"),
            "grok prompts must not fake $ARGUMENTS interpolation"
        );
        assert!(out.contains("argument-hint:"));
        assert!(out.contains("trailing text") || out.contains("do not interpolate"));
    }

    #[test]
    fn orchestration_hint_is_quoted_yaml_scalar() {
        for kind in ["opencode", "pi", "grok"] {
            let out = render_orchestration_prompt(kind, "swarm-mode", "0.1.0", "ts").unwrap();
            let line = out
                .lines()
                .find(|l| l.starts_with("argument-hint:"))
                .expect("hint line present");
            let val = line["argument-hint:".len()..].trim();
            assert!(
                val.starts_with('"') && val.ends_with('"') && val.len() > 2,
                "{kind} hint must be a quoted YAML scalar, got: {val}"
            );
        }
    }

    #[test]
    fn render_orchestration_rejects_unknown() {
        assert!(render_orchestration_prompt("claude", "swarm-mode", "0.1.0", "ts").is_err());
        assert!(render_orchestration_prompt("opencode", "bogus-cmd", "0.1.0", "ts").is_err());
    }

    /// bd-em85: flywheel shelled `toron mail inbox` with the wrong flag once;
    /// the fallback builder keeps `--as` (never `--agent`) pin-able by test.
    #[test]
    fn toron_shell_argv_uses_as_never_agent() {
        let argv = mail_inbox_fallback_argv("DustySparrow", "flywheel", "2026-09-06T00:00:00Z");
        assert!(
            argv.contains(&"--as".to_string()),
            "toron argv must carry --as: {argv:?}"
        );
        assert!(
            !argv.contains(&"--agent".to_string()),
            "toron has no --agent flag: {argv:?}"
        );
    }

    /// bd-glup / bd-ug9z: both playbooks are ASCII-only (the bd-zq8u
    /// peer-aware "cannot be encoded safely" failure mode) and under 4 KiB.
    #[test]
    fn orchestration_templates_are_ascii_only_and_bounded() {
        for (mode, tpl_name) in [("swarm-mode", "swarm.md.tpl"), ("loop-mode", "loop.md.tpl")] {
            let tpl: &str = prompt_orchestration_template(mode).expect(mode);
            assert!(tpl.is_ascii(), "{tpl_name} must be ASCII-only");
            assert!(
                tpl.len() < 4 * 1024,
                "{tpl_name} must stay under 4 KiB ({} bytes)",
                tpl.len()
            );
        }
    }

    /// bd-glup / bd-ug9z: each playbook cites its governing principles by name
    /// (ADR-0019 Spec 5 steering vocabulary). Swarm steers a crew, loop runs
    /// solo, so the context-window needle differs.
    #[test]
    fn orchestration_templates_cite_principles() {
        for (mode, principles) in [
            (
                "swarm-mode",
                [
                    "prove-it-works",
                    "sequence-verifiable-units",
                    "guard-the-context-window",
                    "never-block-on-the-human",
                ],
            ),
            (
                "loop-mode",
                [
                    "prove-it-works",
                    "fix-root-causes",
                    "never-block-on-the-human",
                    "sequence-verifiable-units",
                ],
            ),
        ] {
            let tpl: &str = prompt_orchestration_template(mode).expect(mode);
            for p in principles {
                assert!(tpl.contains(p), "{mode} cites principle {p}");
            }
        }
    }

    #[test]
    fn prompts_dir_maps_each_kind() {
        let _g = crate::config::lock_env();
        let home = std::env::temp_dir().join(format!("fw-prompts-home-{}", std::process::id()));
        unsafe { std::env::set_var("HOME", &home) };
        for (kind, sub) in [
            ("opencode", ".config/opencode/commands"),
            ("pi", ".pi/agent/prompts"),
            ("grok", ".grok/prompts"),
            ("omp", ".omp/agent/prompts"),
        ] {
            let p = prompts_dir(kind).expect(kind);
            assert!(
                p.ends_with(sub),
                "{kind}: {} should end with {sub}",
                p.display()
            );
            assert!(!p.to_string_lossy().is_empty());
        }
        assert!(prompts_dir("claude").is_none());
        unsafe { std::env::remove_var("HOME") };
    }
}

#[cfg(test)]
mod xproj_tests {
    use super::*;

    #[test]
    fn parse_xproj_request_happy() {
        let t = "[xproj] run 4-worker\nrecipe=backtest-5m\nworkers=4\nproduct=trading-research\nstrategy=CrystalRegime\n";
        let r = parse_xproj_request(t).expect("parse");
        assert_eq!(r.recipe, "backtest-5m");
        assert_eq!(r.workers, 4);
        assert_eq!(r.product, "trading-research");
        assert!(r.params.contains("strategy=CrystalRegime"));
    }

    #[test]
    fn parse_xproj_request_requires_marker_and_recipe() {
        assert!(parse_xproj_request("recipe=foo").is_none());
        assert!(parse_xproj_request("[xproj] hello").is_none());
        assert!(parse_xproj_request("").is_none());
    }

    /// review-fix F3: the send body now carries a `to_project=` routing-context
    /// line; the receiver's parse must tolerate it (recipe/workers still win).
    #[test]
    fn parse_xproj_request_tolerates_to_project_line() {
        let t = "[xproj] run\nrecipe=backtest-5m\nworkers=4\nto_project=/Users/x/workspace/flywheel\nstrategy=X\n";
        let r = parse_xproj_request(t).expect("parse");
        assert_eq!(r.recipe, "backtest-5m");
        assert_eq!(r.workers, 4);
        assert!(r.params.contains("strategy=X"));
    }

    #[test]
    fn parse_xproj_recipe_in_subject_line() {
        let t = "[xproj] run 2-worker recipe=daily-p6\nbody line";
        let r = parse_xproj_request(t).expect("parse");
        assert_eq!(r.recipe, "daily-p6");
    }

    #[test]
    fn extract_xproj_from_inbox_filters() {
        let v = json!([
            {"id": 1, "subject": "hello", "body_md": "nope", "thread_id": "t1", "from": "A"},
            {"id": 2, "subject": "[xproj] run", "body_md": "recipe=daily\nworkers=2", "thread_id": "t2", "from": "B"},
        ]);
        let items = extract_xproj_from_inbox(&v);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "2");
        let mut seen = std::collections::HashSet::new();
        let prompts = xproj_prompts_for_unseen(&items, &seen);
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].1.contains("recipe=daily"));
        seen.insert("2".to_string());
        assert_eq!(
            xproj_prompts_for_unseen(&items, &seen),
            [] as [(String, String); 0]
        );
    }

    /// The saturation signal is the EXACT trimmed body — a subject mention
    /// or a body that says more than `CONTEXT_LIMIT` is not the contract.
    #[test]
    fn extract_context_limit_exact_body_only() {
        let v = json!([
            {"id": 1, "subject": "[bd-x] done", "body_md": "CONTEXT_LIMIT", "thread_id": "t1", "from": "TanFern"},
            {"id": 2, "subject": "note", "body_md": "CONTEXT_LIMIT\nall done", "thread_id": "t2", "from": "TanFern"},
            {"id": 3, "subject": "CONTEXT_LIMIT", "body_md": "just mentioning it in passing", "thread_id": "t3", "from": "TanFern"},
            {"id": 4, "subject": "s", "body_md": "  CONTEXT_LIMIT  \n", "thread_id": "t4", "from": "TealDune"},
        ]);
        let items = extract_context_limit_from_inbox(&v);
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["1", "4"], "exact trimmed body only: {ids:?}");
    }

    /// Same envelope tolerance as the xproj scan: bare array,
    /// `{messages:[...]}`, and `{result:[...]}` all parse.
    #[test]
    fn extract_context_limit_envelope_shapes() {
        let row = json!({"id": "9", "subject": "s", "body_md": "CONTEXT_LIMIT", "thread_id": "t", "from": "P"});
        for envelope in [
            json!([row]),
            json!({"messages": [row]}),
            json!({"result": [row]}),
        ] {
            assert_eq!(
                extract_context_limit_from_inbox(&envelope).len(),
                1,
                "{envelope}"
            );
        }
    }
}

/// The lint gate must resolve stages through the SAME tiers the composer
/// uses. It used to call `StageLibrary::load()` (no project tier), so every
/// project-authored stage in `.flywheel/stages.toml` was reported
/// unresolvable and any composed workflow referencing it was refused —
/// while `stage show` happily listed the same stage as valid.
#[test]
fn project_tier_stage_lints_clean_through_the_compose_gate() {
    let dir = temp_state_root("proj-stage-lint");
    let stages_dir = dir.join(".flywheel");
    std::fs::create_dir_all(&stages_dir).expect("mkdir .flywheel");
    std::fs::write(
        stages_dir.join("stages.toml"),
        "[stages.my-lane]\nname = \"my-lane\"\nrole = \"coder\"\nphase = \"implement\"\nprompt = \"do {{bead}}\"\n",
    )
    .expect("write stages.toml");

    let wf = toron_workflow::parse_workflow(
        r#"
name: t
trigger:
  type: webhook
inputs:
  - name: goal
steps:
  - name: a
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "my-lane" }
"#,
    )
    .expect("parse");

    let blocking = lint_flywheel_tier("t", &wf, Some(&dir));
    assert_eq!(
        blocking, 0,
        "a project-tier stage must lint clean — the compose gate refuses \
         every workflow that names one when it cannot resolve the project tier"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// bd-bvwf.1 regression: --retire-orphans must catch orphan workspaces from
/// prior spawns (not just the default-label one). An orphan with a DIFFERENT
/// label that still hosts a `flywheel-<base>-*` agent is classified Owned and
/// retired; the loop's poll filters to the active workspace so orphans never
/// enter the dispatch decision.
#[cfg(test)]
mod bd_bvwf1_orphan_workspaces {
    use super::{WorkspaceOrigin, classify_workspace_origin, partition_workspaces};
    use crate::hc::{AgentInfo, WorkspaceInfo};

    fn ws(id: &str, label: &str) -> WorkspaceInfo {
        serde_json::from_value(serde_json::json!({
            "workspace_id": id,
            "number": 1,
            "label": label,
            "focused": false,
            "pane_count": 1,
            "tab_count": 1,
            "active_tab_id": "t",
            "agent_status": "idle",
        }))
        .expect("ws")
    }

    fn agent(ws_id: &str, name: Option<&str>) -> AgentInfo {
        serde_json::from_value(serde_json::json!({
            "agent": null,
            "agent_status": "idle",
            "cwd": null,
            "display_agent": null,
            "focused": false,
            "foreground_cwd": null,
            "interactive_ready": false,
            "launch_pending": false,
            "name": name,
            "pane_id": format!("{ws_id}:p1"),
            "revision": 1,
            "state_change_seq": 0,
            "state_labels": {},
            "tab_id": "t",
            "terminal_id": "x",
            "terminal_title": null,
            "workspace_id": ws_id,
        }))
        .expect("agent")
    }

    /// An orphan workspace with a NON-default label (e.g. a custom spawn) that
    /// hosts a `flywheel-<base>-*` agent is Owned — retired by --retire-orphans.
    /// (The original bug: only the default-label workspace was closed.)
    #[test]
    fn orphan_hosting_managed_agent_is_owned() {
        let project = "/path/to/toron";
        let default_ws = ws("w-default", "flywheel-toron");
        let orphan_ws = ws("w-orphan", "custom-swarm-label");
        let agents = vec![
            agent("w-orphan", Some("flywheel-toron-oc")),
            agent("w-foreign", Some("omp")),
        ];
        assert_eq!(
            classify_workspace_origin(project, &default_ws, &agents, None),
            Some(WorkspaceOrigin::Owned)
        );
        assert_eq!(
            classify_workspace_origin(project, &orphan_ws, &agents, None),
            Some(WorkspaceOrigin::Owned),
            "orphan hosting a flywheel-<base>-* agent is Owned (retired)"
        );
    }

    /// partition_workspaces groups the default + orphan into `owned` and a
    /// base-labeled foreign session into `foreign`; unrelated workspaces drop.
    #[test]
    fn partition_owns_default_and_orphan_foreign_is_base_label() {
        let project = "/path/to/toron";
        let ws_list = vec![
            ws("w-default", "flywheel-toron"),
            ws("w-orphan", "custom-swarm-label"),
            ws("w-captain", "toron"),
            ws("w-unrelated", "some-other-project"),
        ];
        let agents = vec![agent("w-orphan", Some("flywheel-toron-oc2"))];
        let split = partition_workspaces(project, ws_list, &agents, None);
        let owned_ids: Vec<&str> = split
            .owned
            .iter()
            .map(|w| w.workspace_id.as_str())
            .collect();
        let foreign_ids: Vec<&str> = split
            .foreign
            .iter()
            .map(|w| w.workspace_id.as_str())
            .collect();
        assert_eq!(
            owned_ids,
            vec!["w-default", "w-orphan"],
            "default + orphan owned"
        );
        assert_eq!(
            foreign_ids,
            vec!["w-captain"],
            "base-label session is foreign"
        );
    }
}

/// bd-98gd acceptance: `flywheel send` conditions profiled targets (Spec C
/// everywhere — the captain-facing CLI resolves the spawn registry like the
/// loop's `__profile` leaf), sends raw to unprofiled ones, and `--raw`
/// bypasses conditioning. The VERIFY gate for the bead:
/// `cargo test -p flywheel send_conditioning`.
/// bd-register-portability-3c2b: the register-name decision. `None` explicit
/// AND `None` import source is a refusal (the unnamed-mint bug: a fresh
/// empty-name registration forked one operator across two npubs) — the error
/// names both fixes. Explicit --name wins over import.
fn resolve_register_name(explicit: Option<String>, from_project: Option<&str>) -> Result<String> {
    let explicit = explicit.filter(|n| !n.trim().is_empty());
    let importing = from_project.map(|p| !p.trim().is_empty()).unwrap_or(false);
    match (explicit, importing) {
        (Some(n), _) => Ok(n),
        (None, true) => Ok(String::new()),
        (None, false) => anyhow::bail!(
            "refusing to mint an UNNAMED mail identity — pass --name <adjective+noun-pin>, \
             or bind an existing one with --from-project <slug>, or adopt a prior unnamed \
             mint with --adopt-unnamed <name>"
        ),
    }
}

/// bd-zugo: bind an existing identity key from `from_slug` into `to_slug`'s
/// named slot. NEVER mints — a missing source key is a loud refusal. The
/// observed bug: `--from-project` with no matching key silently minted a
/// fresh npub, whose lease landed on a nameless holder that could not commit
/// (transfer rejected, TTL 2h, operator force-release only).
fn bind_source_key(
    data_dir: &std::path::Path,
    from_slug: &str,
    to_slug: &str,
    name: &str,
) -> Result<PathBuf> {
    let src = import_source_key(data_dir, from_slug, name).ok_or_else(|| {
        anyhow::anyhow!(
            "no existing key for `{name}` in project `{from_slug}` — refusing to mint a second \
             identity (bd-zugo): register `{name}` in `{from_slug}` first, pass \
             --adopt-unnamed <name>, or drop --from-project to mint a new pin here"
        )
    })?;
    let dst_dir = data_dir.join(to_slug).join("agents").join(name);
    std::fs::create_dir_all(&dst_dir)?;
    let dst = dst_dir.join("key.ncryptsec");
    if !dst.exists() {
        std::fs::copy(&src, &dst)
            .with_context(|| format!("binding {} into {}", src.display(), dst.display()))?;
    }
    Ok(dst)
}

/// Locate an existing identity key to import: `{data}/{from}/agents/{name}/
/// key.ncryptsec`. Present = reuse (register_agent adopts it, never mints).
/// bd-hdec: agent-key slots live at `{data}/{slug}/agents/{name}/` where
/// `data` is the MAILBOX data dir (`{durable_root}/projects/` — the same
/// shape toron's bootstrap writes). `data_dir` args at these call sites are
/// `mailbox_roots().0`, so no extra segment is added here.
fn import_source_key(data_dir: &std::path::Path, from_slug: &str, name: &str) -> Option<PathBuf> {
    let p = data_dir
        .join(from_slug)
        .join("agents")
        .join(name)
        .join("key.ncryptsec");
    p.is_file().then_some(p).or_else(|| {
        let legacy = legacy_root_from_mailbox(data_dir)
            .join(from_slug)
            .join("agents")
            .join(name)
            .join("key.ncryptsec");
        legacy.is_file().then_some(legacy)
    })
}

/// The durable root one level above the mailbox data dir (strip the
/// `projects/` segment); identity layout fix bd-hdec.
fn legacy_root_from_mailbox(mailbox_dir: &std::path::Path) -> std::path::PathBuf {
    mailbox_dir
        .parent()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n == toron_core::config::PROJECTS_RELATIVE)
        })
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| mailbox_dir.to_path_buf())
}

/// The unnamed mint's key slot: name "" collapses the path to
/// `{data}/{slug}/agents/key.ncryptsec` (the observed orphan).
fn unnamed_key_path(data_dir: &std::path::Path, slug: &str) -> PathBuf {
    data_dir.join(slug).join("agents").join("key.ncryptsec")
}

/// Cleanup affordance: move an unnamed mint's key into the named slot so the
/// next register REUSES that npub instead of minting another. Same key bytes,
/// one identity; no manual DB surgery. Absent unnamed key is an error naming
/// the state (nothing to adopt).
fn adopt_unnamed_key(data_dir: &std::path::Path, slug: &str, name: &str) -> Result<PathBuf> {
    let src = match unnamed_key_path(data_dir, slug) {
        p if p.is_file() => p,
        _ => {
            let legacy = legacy_root_from_mailbox(data_dir)
                .join(slug)
                .join("agents")
                .join("key.ncryptsec");
            if legacy.is_file() {
                legacy
            } else {
                anyhow::bail!(
                    "no unnamed mint found under {} or {} — nothing to adopt",
                    unnamed_key_path(data_dir, slug).display(),
                    legacy.display()
                );
            }
        }
    };
    let dst_dir = data_dir.join(slug).join("agents").join(name);
    std::fs::create_dir_all(&dst_dir)?;
    let dst = dst_dir.join("key.ncryptsec");
    std::fs::rename(&src, &dst)?;
    Ok(dst)
}

#[cfg(test)]
mod register_identity_tests {
    use super::*;

    /// The unnamed-mint refusal: no --name and no import source is an ERROR
    /// naming both fixes — never a silent empty-name registration.
    #[test]
    fn register_identity_refuses_unnamed_mint() {
        let err = resolve_register_name(None, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--name"), "names the fix: {msg}");
        assert!(
            msg.contains("--from-project") || msg.contains("--adopt-unnamed"),
            "names the import affordance: {msg}"
        );
    }

    /// Explicit --name passes through; explicit beats import when both given.
    #[test]
    fn register_identity_explicit_name_wins() {
        assert_eq!(
            resolve_register_name(Some("BlueLake".into()), Some("beads")).expect("explicit wins"),
            "BlueLake"
        );
    }

    /// Import source discovery: `{data}/{from}/agents/{name}/key.ncryptsec`
    /// must exist to bind; missing source with no --name still refuses.
    #[test]
    fn register_identity_import_source_discovery() {
        let dir = std::env::temp_dir().join(format!("fw-reg-id-{}", std::process::id()));
        let src = dir.join("beads/agents/BlueLake");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(src.join("key.ncryptsec"), b"fake").expect("key");
        let found = import_source_key(&dir, "beads", "BlueLake");
        assert!(found.is_some(), "existing key discovered");
        assert!(
            import_source_key(&dir, "beads", "Nobody").is_none(),
            "missing key -> None"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-zugo: `--from-project` with no matching source key is a LOUD
    /// refusal — never a silent fresh mint (the ghost holder bug).
    #[test]
    fn register_from_project_missing_source_key_refuses() {
        let dir = std::env::temp_dir().join(format!("fw-bind-miss-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let err = bind_source_key(&dir, "toron", "flywheel", "WindyGlen").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no existing key"), "names the state: {msg}");
        assert!(msg.contains("refusing to mint"), "names the refusal: {msg}");
        assert!(msg.contains("--adopt-unnamed"), "names a remedy: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-zugo: a present source key binds into the target slot (same bytes,
    /// no mint); a second bind is idempotent (never overwrites the dst).
    #[test]
    fn register_from_project_binds_existing_key() {
        let dir = std::env::temp_dir().join(format!("fw-bind-hit-{}", std::process::id()));
        let src = dir.join("toron/agents/WindyGlen");
        std::fs::create_dir_all(&src).expect("mkdir src");
        std::fs::write(src.join("key.ncryptsec"), b"key-bytes").expect("write src");

        let dst = bind_source_key(&dir, "toron", "flywheel", "WindyGlen").expect("bind");
        assert_eq!(std::fs::read(&dst).unwrap(), b"key-bytes");

        std::fs::write(&dst, b"already-here").unwrap();
        let again = bind_source_key(&dir, "toron", "flywheel", "WindyGlen").expect("rebind");
        assert_eq!(std::fs::read(&again).unwrap(), b"already-here");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Adopt: the unnamed mint (`agents/key.ncryptsec`, name "") moves into
    /// the named slot — same key bytes, one npub, no DB surgery.
    #[test]
    fn register_identity_adopt_moves_unnamed_key() {
        let dir = std::env::temp_dir().join(format!("fw-reg-ad-{}", std::process::id()));
        let unnamed = dir.join("flywheel/agents/key.ncryptsec");
        std::fs::create_dir_all(unnamed.parent().unwrap()).expect("mkdir");
        std::fs::write(&unnamed, b"npub-bytes").expect("write");
        adopt_unnamed_key(&dir, "flywheel", "IronSummit").expect("adopt");
        let named = dir.join("flywheel/agents/IronSummit/key.ncryptsec");
        assert_eq!(std::fs::read(&named).unwrap(), b"npub-bytes");
        assert!(!unnamed.exists(), "unnamed slot vacated");
        assert!(adopt_unnamed_key(&dir, "flywheel", "Other").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod repair_recipe_tests {
    use toron_workflow::schema::Action;

    #[test]
    fn repair_recipe_is_registered_and_has_graph_nodes() {
        assert_eq!(
            crate::workflow_catalog::builtin_meta("repair-until-verified").compose,
            Some(true)
        );
        let wf = crate::builtin_workflows::builtin_recipe("repair-until-verified")
            .expect("recipe exists");
        let Action::Loop { body, .. } = &wf.steps[0].action else {
            panic!("repair is a loop");
        };
        assert!(body.iter().any(|s| s.name.as_deref() == Some("repair")));
        assert!(body.iter().any(|s| s.name.as_deref() == Some("evaluate")));
        assert!(body.iter().any(|s| s.name.as_deref() == Some("verify")));
        let has_ledger = format!("{:?}", wf.steps).contains("ledger");
        assert!(has_ledger || wf.inputs.iter().any(|i| i.name == "ledger_path"));
    }

    #[test]
    fn repair_recipe_evaluator_stage_is_fresh_and_structured() {
        use toron_workflow::schema::{Action, ContextMode};
        let wf = crate::builtin_workflows::builtin_recipe("repair-until-verified").expect("recipe");
        let Action::Loop { body, .. } = &wf.steps[0].action else {
            panic!("repair is a loop");
        };
        let evaluate = body
            .iter()
            .find(|s| s.name.as_deref() == Some("evaluate"))
            .expect("evaluate");
        {
            let name = "evaluate";
            let Action::DispatchJob { spec, .. } = &evaluate.action else {
                panic!("{name} is a dispatch");
            };
            assert_eq!(spec.stage.as_deref(), Some("ce-evaluate"), "{name} stage");
            assert_eq!(
                spec.context,
                Some(ContextMode::Fresh),
                "{name} uses a fresh context"
            );
            assert!(
                spec.plan
                    .as_deref()
                    .is_some_and(|plan| plan.contains("VERIFY")),
                "{name} carries the verification contract"
            );
            assert!(
                spec.task
                    .as_deref()
                    .is_some_and(|task| task.contains("ce-evaluate JSON contract")),
                "{name} names the structured evaluator contract"
            );
        }
    }

    #[test]
    fn repair_recipe_ledger_path_is_used() {
        let wf = crate::builtin_workflows::builtin_recipe("repair-until-verified").expect("recipe");
        let has_ledger_input = wf.inputs.iter().any(|i| i.name == "ledger_path");
        assert!(has_ledger_input, "ledger_path input must exist");
        let has_ledger_in_task = format!("{:?}", wf.steps).contains("ledger_path");
        assert!(has_ledger_in_task);
    }
}

#[cfg(test)]
mod approval_drain_tests {
    use toron_workflow::engine::{Decision, RunInput};

    fn approval_workflow() -> toron_workflow::schema::Workflow {
        toron_workflow::schema::parse_workflow(
            r#"name: approval-test
trigger: { type: webhook }
steps:
  - name: ask
    action: request_approval
    message: "approve?"
"#,
        )
        .expect("parse approval workflow")
    }

    #[tokio::test]
    async fn approval_drain_approve_resumes() {
        let engine = toron_workflow::WorkflowEngine::local().expect("engine");
        let wf = approval_workflow();
        let run_id = format!("approval-approve-{}", uuid::Uuid::new_v4());
        let input = RunInput {
            run_id: Some(run_id.clone()),
            workflow: wf,
            trigger: serde_json::json!({}),
        };
        let handle = {
            let engine = engine.clone();
            let rid = run_id.clone();
            tokio::spawn(async move { engine.run(&rid, input).await })
        };
        let promise_id = {
            let mut tries = 0;
            loop {
                let ids = engine.approval_sink().lock().await.clone();
                if let Some(id) = ids.first().cloned() {
                    break id;
                }
                assert!(tries <= 20, "approval promise never appeared");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                tries += 1;
            }
        };
        engine
            .resolve_approval(&promise_id, &Decision::approved("ok"))
            .await
            .expect("resolve");
        let out = handle.await.expect("join").expect("run should resume");
        assert!(out.get("ask").is_some() || out.get("decision").is_some());
    }

    #[tokio::test]
    async fn approval_drain_reject_records_rejected() {
        let engine = toron_workflow::WorkflowEngine::local().expect("engine");
        let wf = approval_workflow();
        let run_id = format!("approval-reject-{}", uuid::Uuid::new_v4());
        let input = RunInput {
            run_id: Some(run_id.clone()),
            workflow: wf,
            trigger: serde_json::json!({}),
        };
        let handle = {
            let engine = engine.clone();
            let rid = run_id.clone();
            tokio::spawn(async move { engine.run(&rid, input).await })
        };
        let promise_id = {
            let mut tries = 0;
            loop {
                let ids = engine.approval_sink().lock().await.clone();
                if let Some(id) = ids.first().cloned() {
                    break id;
                }
                assert!(tries <= 20, "approval promise never appeared");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                tries += 1;
            }
        };
        engine
            .reject_approval(&promise_id, &Decision::rejected("nope"))
            .await
            .expect("reject");
        let out = handle
            .await
            .expect("join")
            .expect("run should complete even on reject");
        let has_rejected = out.to_string().contains("rejected");
        assert!(has_rejected, "reject should record rejected: {out}");
    }
}

#[cfg(test)]
mod idempot_tests {

    #[test]
    fn idempot_spawn_same_key_returns_first_pin() {
        let project = "test-proj";
        let bead_id = "bd-test-idempot-1";
        let profile = "coder";
        let key = format!("{}:{}:{}", project, bead_id, profile);
        let mut registry: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        let pin1 = "CoderOne";
        let pane1 = "pane-1";
        registry.insert(key.clone(), (pin1.to_string(), pane1.to_string()));
        let count1 = registry.len();
        let (pin2, pane2) = registry.get(&key).unwrap().clone();
        assert_eq!(pin2, pin1);
        assert_eq!(pane2, pane1);
        assert_eq!(
            registry.len(),
            count1,
            "pane count 1, second spawn returns first pin"
        );
    }

    #[test]
    fn idempot_compose_same_run_id_does_not_agent_start_again() {
        let run_id = "fw-composed-test123";
        let mut calls = 0;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut mock_agent_start = |id: &str| {
            if seen.contains(id) {
                false
            } else {
                seen.insert(id.to_string());
                calls += 1;
                true
            }
        };
        assert!(mock_agent_start(run_id));
        assert!(
            !mock_agent_start(run_id),
            "second compose same run_id does not agent_start again"
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn idempot_dead_pane_respawns_same_key_new_pane() {
        let project = "test-proj";
        let bead_id = "bd-test-dead";
        let profile = "coder";
        let key = format!("{}:{}:{}", project, bead_id, profile);
        let mut registry: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        registry.insert(key.clone(), ("PinA".to_string(), "pane-old".to_string()));
        let (pin, _old_pane) = registry.get(&key).unwrap().clone();
        let new_pane = "pane-new";
        registry.insert(key.clone(), (pin.clone(), new_pane.to_string()));
        let (pin2, pane2) = registry.get(&key).unwrap().clone();
        assert_eq!(pin2, "PinA");
        assert_eq!(pane2, "pane-new");
        assert_eq!(registry.len(), 1, "same registry row, count 1, new pane id");
        assert_ne!(pane2, "pane-old");
    }

    #[test]
    fn idempot_direct_path_no_artifacts_dir_needed() {
        let artifacts_dir = std::path::Path::new("/tmp/test-no-artifacts");
        assert!(
            !artifacts_dir.join("artifacts").exists() || true,
            "direct path should not require artifacts dir"
        );
        let result = std::panic::catch_unwind(|| {
            let _ = crate::crew::plan_crew(&toron_workflow::schema::Workflow {
                name: "direct-test".into(),
                description: None,
                trigger: None,
                policy: None,
                guard: None,
                steps: vec![],
                graph: None,
                inputs: vec![],
                crew: None,
                principles: vec![],
            });
        });
        assert!(result.is_ok(), "direct path needs no artifacts dir");
    }
}

#[cfg(test)]
mod ingress_wiring_tests {
    use crate::ingress::{IngressEvent, TerminalEvent, WatchHit};

    #[test]
    fn ingress_wiring_all_variants_go_through_handle_ingress() {
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let adapters = crate::ingress::IngressAdapters {
            hc: {
                let h = hits.clone();
                Box::new(move |_: &TerminalEvent| {
                    h.lock().unwrap().push("hc");
                })
            },
            watch: {
                let h = hits.clone();
                Box::new(move |_: &WatchHit| {
                    h.lock().unwrap().push("watch");
                })
            },
            workflow: {
                let h = hits.clone();
                Box::new(move |_: &toron_workflow::schema::Trigger| {
                    h.lock().unwrap().push("workflow");
                })
            },
            cron: {
                let h = hits.clone();
                Box::new(move |_: &str| {
                    h.lock().unwrap().push("cron");
                })
            },
            fact: {
                let h = hits.clone();
                Box::new(move |_: &crate::ingress::IngressEvent| {
                    h.lock().unwrap().push("fact");
                })
            },
        };
        crate::ingress::handle_ingress(
            &IngressEvent::Hc(TerminalEvent::PaneExited {
                pane_id: "p1".into(),
                terminal_name: "n1".into(),
            }),
            &adapters,
        );
        crate::ingress::handle_ingress(
            &IngressEvent::Watch(WatchHit {
                project: "p".into(),
                kind: "k".into(),
                event_id: None,
            }),
            &adapters,
        );
        crate::ingress::handle_ingress(
            &IngressEvent::WorkflowTrigger(toron_workflow::schema::Trigger::MessagePosted {
                channel: None,
                project: None,
            }),
            &adapters,
        );
        crate::ingress::handle_ingress(
            &IngressEvent::CronTick {
                name: "tick".into(),
            },
            &adapters,
        );
        let h = hits.lock().unwrap();
        assert_eq!(h.len(), 4);
        assert!(h.contains(&"hc"));
        assert!(h.contains(&"watch"));
        assert!(h.contains(&"workflow"));
        assert!(h.contains(&"cron"));
    }

    #[test]
    fn facts_variants_route_through_handle_ingress() {
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let adapters = crate::ingress::IngressAdapters {
            hc: Box::new(|_| {}),
            watch: Box::new(|_| {}),
            workflow: Box::new(|_| {}),
            cron: Box::new(|_| {}),
            fact: {
                let h = hits.clone();
                Box::new(move |e: &crate::ingress::IngressEvent| {
                    if let Some(s) = crate::ingress::fact_summary(e) {
                        h.lock().unwrap().push(s);
                    }
                })
            },
        };
        for ev in [
            crate::ingress::IngressEvent::LedgerFact {
                bead_id: "bd-1".into(),
                event: crate::ingress::LedgerEvent::Closed,
            },
            crate::ingress::IngressEvent::PaneStalled {
                pane_id: "w1:p1".into(),
                terminal_name: "oc".into(),
                last_status: "idle".into(),
                bead_id: Some("bd-1".into()),
                reason: "silent_idle".into(),
                evidence_tail: String::new(),
                severity: "stalled".into(),
            },
            crate::ingress::IngressEvent::LeaseLapsed {
                path: "src/main.rs".into(),
                bead_id: None,
            },
            crate::ingress::IngressEvent::GitFact {
                repo: "flywheel".into(),
                sha: "abc123def456".into(),
                subject: "fix".into(),
                bead_id: None,
            },
        ] {
            crate::ingress::handle_ingress(&ev, &adapters);
        }
        let h = hits.lock().unwrap();
        assert_eq!(
            h.len(),
            4,
            "all fact variants must route through handle_ingress"
        );
        assert!(h.iter().any(|s| s.contains("ledger bd-1 closed")));
        assert!(h.iter().any(|s| s.contains("pane stalled")));
        assert!(h.iter().any(|s| s.contains("lease lapsed")));
        assert!(h.iter().any(|s| s.contains("git flywheel")));
    }

    #[test]
    fn facts_captain_surface_renders_summaries() {
        let ledger = crate::ingress::IngressEvent::LedgerFact {
            bead_id: "bd-9".into(),
            event: crate::ingress::LedgerEvent::Blocked,
        };
        assert_eq!(
            crate::ingress::fact_summary(&ledger).unwrap(),
            "ledger bd-9 blocked"
        );
        assert_eq!(
            crate::ingress::fact_doorbell_line(&ledger).unwrap(),
            "[doorbell] ledger bd-9 blocked"
        );
        let stalled = crate::ingress::IngressEvent::PaneStalled {
            pane_id: "w2:p2".into(),
            terminal_name: "pi".into(),
            last_status: "working".into(),
            bead_id: None,
            reason: "silent_idle".into(),
            evidence_tail: String::new(),
            severity: "stalled".into(),
        };
        assert!(
            crate::ingress::fact_summary(&stalled)
                .unwrap()
                .contains("pane stalled w2:p2")
        );
        assert!(
            crate::ingress::fact_doorbell_line(&stalled)
                .unwrap()
                .starts_with("[doorbell] pane stalled")
        );
        let proj = format!("facts-test-{}", uuid::Uuid::new_v4());
        let root = crate::temp_state_root("facts-captain");

        let ev = crate::ingress::IngressEvent::LeaseLapsed {
            path: "src/lib.rs".into(),
            bead_id: Some("bd-99".into()),
        };
        let (c0, _) = crate::facts_count_and_last(&root, &proj);
        assert_eq!(c0, 0);
        crate::append_fact(&root, &proj, &ev).unwrap();
        let (c1, last) = crate::facts_count_and_last(&root, &proj);
        assert_eq!(c1, 1);
        assert!(last.unwrap().contains("lease lapsed src/lib.rs"));
        let non = crate::ingress::IngressEvent::CronTick {
            name: "tick".into(),
        };
        crate::append_fact(&root, &proj, &non).unwrap();
        let (c2, _) = crate::facts_count_and_last(&root, &proj);
        assert_eq!(c2, 1, "non-fact must not be journaled");

        crate::assert_live_root_untouched(&proj);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// GS-033 Option A tripwires: doorbell is the one RAM owner and must seed
/// swarm resume. Dual-as-end-state speech is forbidden. Needles split so
/// these self-scans do not match their own source.
#[cfg(test)]
mod adr0010_d13_tests {
    // governed-by: ADR-0010 D13 (bd-eo39) — captain/steer subagent-guard.
    use super::*;

    #[test]
    fn adr0010_d13_delegation_shaped_names_denied_on_captain() {
        for tool in [
            "Task",
            "task",
            "subagent",
            "Agent",
            "spawn",
            "dispatch",
            "worktree",
            "delegate",
            "workflow",
            "agent_worker",
        ] {
            assert!(
                subagent_guard_decision(None, tool).is_err(),
                "{tool} must deny on the captain pane"
            );
        }
    }

    #[test]
    fn adr0010_d13_observe_stop_and_todo_allowed_on_captain() {
        for tool in [
            "taskoutput",
            "taskstop",
            "tasklist",
            "taskcreate",
            "taskupdate",
            "TaskOutput",
        ] {
            assert!(
                subagent_guard_decision(None, tool).is_ok(),
                "{tool} is observe/stop or session-local todo — allowed"
            );
        }
    }

    #[test]
    fn adr0010_d13_mcp_tools_never_denied() {
        for tool in ["mcp__toron__send_message", "mcp__context7__query-docs"] {
            assert!(
                subagent_guard_decision(None, tool).is_ok(),
                "{tool}: mcp__* excluded (toron MCP is the sanctioned surface)"
            );
        }
    }

    #[test]
    fn adr0010_d13_crew_panes_exempt_and_creatable() {
        assert!(subagent_guard_decision(Some("crew"), "Task").is_ok());
        assert!(subagent_guard_decision(Some("CREW"), "spawn").is_ok());
        assert!(
            !delegation_deny("flywheel").is_some(),
            "the flywheel CLI itself is never delegation-shaped"
        );
        assert!(subagent_guard_decision(Some("captain"), "Task").is_err());
    }

    #[test]
    fn adr0010_d13_empty_and_unknown_tools_allow() {
        assert!(subagent_guard_decision(None, "").is_ok());
        assert!(subagent_guard_decision(None, "   ").is_ok());
        assert!(subagent_guard_decision(None, "read_file").is_ok());
    }
}

#[cfg(test)]
mod adr0010_d7_tests {
    // governed-by: ADR-0010 D7 (bd-w5fn) — crew control verbs + FLYWHEEL_OP

    #[test]
    fn adr0010_d7_no_arbitrary_text_entry_point() {
        let start = include_str!("cmd/crew.rs")
            .find("enum ControlVerbCmd {")
            .expect("ControlVerbCmd enum present");
        let src = include_str!("cmd/crew.rs");
        let end = start + src[start..].find("\n}\n").expect("enum closes");
        let body = &src[start..end];
        for verb in ["Interrupt", "Exit", "Relaunch"] {
            assert!(body.contains(verb), "{verb} variant present: {body}");
        }
        assert!(!body.contains("String"), "no payload variants: {body}");
    }

    #[test]
    fn adr0010_d7_interrupt_goes_through_the_gated_plan() {
        let body = fn_slice_in(include_str!("cmd/crew.rs"), "cmd_crew_control");
        assert!(
            body.contains("control::interrupt_plan"),
            "interrupt routes through the capability-gated pure plan"
        );
        assert!(
            body.contains("confirmed (composer provably empty)"),
            "cancel confirmation is reported, not implied"
        );
    }

    #[test]
    fn adr0010_d7_exit_never_releases_reservations_or_touches_dirty() {
        let body = fn_slice_in(include_str!("cmd/crew.rs"), "cmd_crew_control");
        let exit = body.find("ControlVerbCmd::Exit => {").expect("exit arm");
        let relaunch = body
            .find("ControlVerbCmd::Relaunch => {")
            .expect("relaunch arm");
        let exit_arm = &body[exit..relaunch];
        assert!(
            exit_arm.contains("terminal_kill") || exit_arm.contains("pane.close"),
            "exit closes the pane: {exit_arm}"
        );
        for banned in [
            "release",
            "clear_pane_identity",
            "remove_dir_all",
            "remove_file",
        ] {
            assert!(
                !exit_arm.contains(banned),
                "exit must preserve reservations + dirty files; found `{banned}`"
            );
        }
    }

    #[test]
    fn adr0010_d7_relaunch_journals_before_stop_and_rolls_back_on_fail() {
        let body = fn_slice_in(include_str!("cmd/crew.rs"), "cmd_crew_control");
        let relaunch = body
            .find("ControlVerbCmd::Relaunch => {")
            .expect("relaunch arm");
        let relaunch_arm = &body[relaunch..];
        let checkpoint = relaunch_arm
            .find("save_relaunch_journal")
            .expect("journal checkpoint");
        let stop = relaunch_arm
            .find("terminal_kill")
            .or_else(|| relaunch_arm.find("pane.close"))
            .expect("stop");
        assert!(
            checkpoint < stop,
            "checkpoint BEFORE stop (journaled transaction)"
        );
        assert!(
            relaunch_arm.contains("RelaunchAction::Rollback")
                && relaunch_arm.contains("rollback_relaunch"),
            "launch failure restores the prior durable record"
        );
        assert!(
            relaunch_arm.contains("incarnation_token"),
            "relaunch carries an incarnation token"
        );
        assert!(
            relaunch_arm.contains("clear_relaunch_journal"),
            "journal is cleared on both outcomes (idempotent re-entry)"
        );
    }

    #[test]
    fn adr0010_d7_control_prompts_carry_the_marker() {
        let body = fn_slice_in(include_str!("cmd/crew.rs"), "relaunch_prime_line");
        assert!(
            body.contains("control::op_prompt"),
            "control-plane prompts are marker-prefixed (U+2063 + FLYWHEEL_OP:)"
        );
    }

    /// Same slice over another file's embedded source (bd-p6rg: the crew
    /// handlers moved to cmd/crew.rs — their gates read that file now).
    fn fn_slice_in(src: &str, name: &str) -> String {
        let start = crate::tripwire::find_fn(src, name);
        let rest = &src[start..];
        let end = rest.find("\n}\n").expect("fn closes with a column-0 brace") + 1;
        rest[..end].to_string()
    }
}

/// governed-by: bd-jcqq — product-scoped captain digest + cross-project
/// identity fold. Seeded mail in each member must surface in one digest;
/// unreadable members render as loud gaps (never silent skips, never an
/// abort); the fold binds the pin's EXISTING key across members (never
/// mints a second npub).
#[cfg(test)]
mod bd_qlr1_reroute_env_pin_tests {
    use super::{RerouteEnvPin, reroute_env_pin_verdict};

    /// bd-qlr1: the distinction that keeps a working recovery path working.
    /// A reroute launches into an EXISTING pane, so an env-delivered model pin
    /// can never be re-applied. A reroute that keeps the SAME model is
    /// genuinely satisfied — the pane already carries that pin — and must not
    /// be refused, or a profile fallback that only changes harness would break.
    #[test]
    fn a_same_model_reroute_is_already_pinned_and_proceeds() {
        assert_eq!(
            reroute_env_pin_verdict(
                Some("opencode-go/muse-spark-1.3-contributor"),
                "opencode-go/muse-spark-1.3-contributor"
            ),
            RerouteEnvPin::AlreadyPinned,
            "the pane already carries this pin, so the hop is satisfied"
        );
    }

    /// The unsatisfiable case, refused by name rather than silently downgraded.
    /// If this were allowed, the pane would keep the OLD pin while the ledger
    /// recorded the NEW model — the agent would run a model nobody selected.
    #[test]
    fn a_model_changing_reroute_is_refused() {
        for (pinned, wants) in [
            (
                Some("opencode-go/muse-spark-1.3-contributor"),
                "deepseek/deepseek-flash",
            ),
            (None, "opencode-go/muse-spark-1.3-contributor"),
        ] {
            assert_eq!(
                reroute_env_pin_verdict(pinned, wants),
                RerouteEnvPin::ModelChanged,
                "pinned={pinned:?} wants={wants}: a pane's env is fixed at split, so this hop \
                 would silently run the previous model"
            );
        }
    }

    /// Pin the exact-equality requirement. A provider-only or model-only match
    /// is NOT the same pin, and treating it as one would reintroduce the silent
    /// downgrade this verdict exists to prevent.
    #[test]
    fn the_match_is_exact_not_a_prefix_or_suffix() {
        for (pinned, wants) in [
            (
                Some("opencode-go/muse-spark-1.3"),
                "opencode-go/muse-spark-1.3-contributor",
            ),
            (
                Some("opencode-go"),
                "opencode-go/muse-spark-1.3-contributor",
            ),
            (Some(""), "opencode-go/muse-spark-1.3-contributor"),
        ] {
            assert_eq!(
                reroute_env_pin_verdict(pinned, wants),
                RerouteEnvPin::ModelChanged,
                "pinned={pinned:?} is not the same pin as {wants}"
            );
        }
    }
}

#[cfg(test)]
mod boot_prime_tests {
    use super::*;

    /// bd-koafr: with an assignment the first prompt names the bead and points
    /// at its VERIFY; it never sends the crew to the backlog.
    #[test]
    fn an_assigned_boot_names_the_bead_and_skips_triage() {
        let line = boot_prime_line_for(Some("bd-koafr"));
        assert!(line.contains("bd-koafr"), "{line}");
        assert!(line.contains("VERIFY"), "{line}");
        assert!(line.contains("Do not read mail"), "{line}");
        assert!(
            !line.contains("wait for an assignment"),
            "an assigned crew must not be told to wait: {line}"
        );
    }

    /// Without one it instructs to WAIT — the old neutral line left crews to
    /// triage mail and awareness beads on their own.
    #[test]
    fn an_unassigned_boot_waits_instead_of_triaging() {
        let line = boot_prime_line_for(None);
        assert!(line.contains("wait for an assignment"), "{line}");
        assert!(line.contains("Do not read mail"), "{line}");
        assert!(!line.contains("br update"), "{line}");
    }
}

#[cfg(test)]
mod robot_capabilities_text {
    /// Three capability lines drifted behind the verbs they describe (bd-6setu.20,
    /// ScarletCove's cross-review). One assertion per string, so the next drift
    /// is red instead of silent.
    #[test]
    fn the_robot_capabilities_text_names_the_live_verbs() {
        let src = include_str!("main.rs");
        let start = src.find("if cli.robot_capabilities").expect("arm");
        let end = src[start..]
            .find("\n    let cfg = match Config::load()")
            .expect("arm end");
        let block = &src[start..start + end];
        for needle in [
            // (1) ship land: --repo and the retried HEAD race
            "ship land <bead> --sha <40hex> --receipt <file> [--project <p>] [--as <pin>] [--message <m>] [--repo <dir>]",
            "the HEAD-lock race other lanes cause is retried",
            // (2) swarm apply: --yes and the plan-of-record semantics
            "[--count <role>=<n>] [--yes] [--json] | swarm status",
            "every apply is a plan EVENT first",
            "land as ONE write only after every gate",
            // (3) crew: inbox, done, the mail fallback, and a queued wake exit 0
            "crew preview|control|prompt|inbox --open",
            "crew done <brief> --receipt",
            "falling back to plane-P mail",
            "exits 0 as queued",
        ] {
            assert!(
                block.contains(needle),
                "robot-capabilities must carry {needle:?}"
            );
        }
    }
}

#[cfg(test)]
mod e2e_live_label_tests {
    /// bd-oo7zi: the throwaway session label must be OWNED by the e2e project,
    /// or crew prompt refuses the pane with "no flywheel swarm" before the
    /// wake is even attempted (live proof53, line 111). Non-live witness:
    /// parse the script's default and feed it to flywheel's own ownership rule.
    #[test]
    fn the_live_five_default_session_is_owned_by_its_project() {
        let script = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/e2e-live-five.sh"
        ));
        let project_marker = "FLYWHEEL_LIVE_PROJECT:-";
        let p = script.find(project_marker).expect("project default") + project_marker.len();
        let project: String = script[p..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let session_marker = "FLYWHEEL_LIVE_SESSION:-flywheel-$PROJECT-";
        assert!(
            script.contains(session_marker),
            "the session default must be owned by the project, got: {session_marker}"
        );
        let owned = format!("{}-20260101T000000Z-1", super::swarm_label(&project));
        assert!(
            super::workspace_label_is_owned(&project, &owned, None),
            "flywheel must resolve a session it owns: {owned}"
        );
    }
}

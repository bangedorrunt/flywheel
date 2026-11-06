//! `flywheel debate` + `flywheel plan-review` verbs (split from `main.rs`, bd-wave5-main-eviction-d5hht).

use anyhow::{Result, anyhow, bail};

use crate::backend;
use crate::config::Config;
use crate::{
    harvest_latest, mail_send, project_dir_str, resolve_agent_default, resolve_mail_target_registry,
};
/// `flywheel debate <proposition> [--rounds N] [--agent-a ..] [--agent-b ..]
/// [--judge ..]` — N-round A/B debate + judge (ADR-0001 §9.1).
///
/// Threaded (thread_id = debate-<ts>), ack-gated (turn advances on the
/// agent settling — done/idle — then the orchestrator harvests the speaker's
/// latest message via search_messages). Judge reads via summarize_thread.
///
/// governed-by: ADR-0001 (bd-pattern-debate-loop)
/// Dispatched from `main()`; moved from crate root (bd-wave5-main-eviction-d5hht).
pub(crate) fn cmd_debate(
    cfg: &Config,
    proposition: &str,
    rounds: u32,
    agent_a: Option<&str>,
    agent_b: Option<&str>,
    judge: Option<&str>,
) -> Result<()> {
    let rounds = rounds.max(1);
    let iso = crate::state::now_iso().replacen('-', "", 10);
    let ts: String = iso.chars().filter(char::is_ascii_alphanumeric).collect();
    let thread = format!("debate-{ts}");

    let project = if cfg.workspace_root().join(proposition).exists() {
        proposition.to_string()
    } else {
        "flywheel".to_string()
    };
    let key = project_dir_str(cfg, &project)?;

    let (a, b, j) = resolve_debate_agents(&project, agent_a, agent_b, judge)?;

    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => bail!("mail backend unavailable for debate: {e:#}"),
    };
    let mux = backend::multiplexer_backend(cfg)?;
    backend::run_mail(mail.ensure_project(&key))?;

    let mail_target = |agent: &str| resolve_mail_target_registry(&project, agent).clone();

    let opening = format!(
        "You are debating proposition: {proposition}\n\nArgue FOR it. Be concise and concise (<= 120 words). Provide your single strongest argument with a brief reason.\n\nAfter replying in this thread, acknowledge the orchestrator's opening message to signal your turn is done."
    );
    mail_send(
        &*mail,
        &key,
        &[mail_target(&a)],
        &format!("[debate] {proposition}"),
        &opening,
        "high",
        true,
        Some(&thread),
    )?;
    println!("debate {thread}: {} opens FOR (round 1)", a);

    let mut last_reply: Option<String> = None;
    for round in 1..=rounds {
        let speaker: &str = if round % 2 == 1 { &a } else { &b };
        if round > 1 {
            let reply = last_reply.clone().unwrap_or_default();
            let counter = format!(
                "You are debating proposition: {proposition}\n\nCounter the opponent's latest argument with a NEW point (do not repeat). <= 120 words. Acknowledge when done.\n\nOpponent's latest argument: {reply}"
            );
            mail_send(
                &*mail,
                &key,
                &[mail_target(speaker)],
                &format!("[debate] {proposition}"),
                &counter,
                "high",
                true,
                Some(&thread),
            )?;
        }
        settle_agent(&*mux, speaker)?;
        last_reply = Some(harvest_latest(
            mail.as_ref(),
            &key,
            &thread,
            &mail_target(speaker),
        )?);
        println!("debate {thread}: {} replied (round {})", speaker, round);
    }

    let judge_prompt = format!(
        "You are the judge of this debate on: {proposition}\n\nRead the full thread via summarize_thread. Pick a winner in <= 100 words: name the winner's argument and why it is stronger. Reply in thread and acknowledge when done."
    );
    mail_send(
        &*mail,
        &key,
        &[mail_target(&j)],
        &format!("[debate] {proposition} — judge"),
        &judge_prompt,
        "high",
        true,
        Some(&thread),
    )?;
    settle_agent(&*mux, &j)?;
    let verdict = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&j))?;
    println!("debate {thread} verdict: {verdict}");
    Ok(())
}

/// Reviewer structured tokens (leading token required).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewToken {
    Approve,
    Changes(String),
    Verified,
    Reject(String),
}

/// Parse the first structured token from a reviewer reply.
pub fn parse_review_token(text: &str) -> Option<ReviewToken> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let line = t
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(t);
    let upper = line.to_ascii_uppercase();
    if upper.starts_with("APPROVE") {
        return Some(ReviewToken::Approve);
    }
    if upper.starts_with("VERIFIED") {
        return Some(ReviewToken::Verified);
    }
    if upper.starts_with("CHANGES") {
        let notes = line
            .split_once(':')
            .map(|(_, n)| n.trim().to_string())
            .unwrap_or_default();
        return Some(ReviewToken::Changes(notes));
    }
    if upper.starts_with("REJECT") {
        let notes = line
            .split_once(':')
            .map(|(_, n)| n.trim().to_string())
            .unwrap_or_default();
        return Some(ReviewToken::Reject(notes));
    }
    None
}

/// Gate phase for the plan→review→implement→verify state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriPhase {
    AwaitingPlan,
    AwaitingPlanReview,
    AwaitingImplement,
    AwaitingVerify,
    Done,
    Escalated,
}

/// Next orchestrator action after a phase transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriAction {
    PromptPlanner { msg: String },
    PromptReviewer { msg: String },
    PromptImplementer { msg: String },
    HaltVerified,
    Escalate { reason: String },
}

/// Initial planner prompt (writes plan file + replies with path).
#[must_use]
pub fn pri_planner_prompt(feature: &str, plan_path: &str) -> String {
    format!(
        "You are the PLANNER for this feature (paper-first gate).\n\n\
         Feature:\n{feature}\n\n\
         Write a concrete implementation plan to `{plan_path}`.\n\
         When done, reply in this thread with a single line:\n\
         PLAN: {plan_path}\n\
         Do not implement code. Do not merge."
    )
}

/// Route a plan-review token at the current phase.
///
/// Returns (next_phase, action, updated_change_rounds).
/// `change_rounds` counts completed CHANGES loops; escalate when the next
/// CHANGES would exceed `max_change_rounds`.
#[must_use]
pub fn pri_route(
    phase: PriPhase,
    token: &ReviewToken,
    plan_path: &str,
    change_rounds: u32,
    max_change_rounds: u32,
) -> (PriPhase, PriAction, u32) {
    match (phase, token) {
        (PriPhase::AwaitingPlanReview, ReviewToken::Approve) => (
            PriPhase::AwaitingImplement,
            PriAction::PromptImplementer {
                msg: format!(
                    "You are the IMPLEMENTER (paper-first).\n\n\
                     Implement the plan at `{plan_path}`.\n\
                     - Reserve files via agent-mail (reason from the plan/bead).\n\
                     - Before `cargo build`/`cargo test`, acquire build-slot `cargo` \
                       (ttl ~1800s); release on completion.\n\
                     - Work on a branch — do NOT merge to main.\n\
                     - Do NOT close beads; the verifier + human own that.\n\
                     When done, reply IMPLEMENT_DONE in this thread."
                ),
            },
            change_rounds,
        ),
        (PriPhase::AwaitingPlanReview, ReviewToken::Changes(notes)) => {
            let next = change_rounds.saturating_add(1);
            if next > max_change_rounds {
                (
                    PriPhase::Escalated,
                    PriAction::Escalate {
                        reason: format!(
                            "change rounds exhausted (max {max_change_rounds}): {notes}"
                        ),
                    },
                    next,
                )
            } else {
                (
                    PriPhase::AwaitingPlan,
                    PriAction::PromptPlanner {
                        msg: format!(
                            "Changes requested (round {next}/{max_change_rounds}):\n{notes}\n\n\
                             Revise `{plan_path}`. Reply PLAN: {plan_path} when done."
                        ),
                    },
                    next,
                )
            }
        }
        (PriPhase::AwaitingPlanReview, ReviewToken::Reject(reason)) => (
            PriPhase::Escalated,
            PriAction::Escalate {
                reason: format!("plan rejected: {reason}"),
            },
            change_rounds,
        ),
        (PriPhase::AwaitingVerify, ReviewToken::Verified) => {
            (PriPhase::Done, PriAction::HaltVerified, change_rounds)
        }
        (PriPhase::AwaitingVerify, ReviewToken::Reject(reason)) => (
            PriPhase::Escalated,
            PriAction::Escalate {
                reason: format!("implementation rejected: {reason}"),
            },
            change_rounds,
        ),
        (PriPhase::AwaitingVerify, ReviewToken::Changes(notes)) => (
            PriPhase::Escalated,
            PriAction::Escalate {
                reason: format!("verify requested changes (escalate): {notes}"),
            },
            change_rounds,
        ),
        (phase, other) => (
            PriPhase::Escalated,
            PriAction::Escalate {
                reason: format!("unexpected token {other:?} in phase {phase:?}"),
            },
            change_rounds,
        ),
    }
}

/// After implementer settles: send verify prompt to reviewer.
#[must_use]
pub fn pri_after_implement(plan_path: &str) -> (PriPhase, PriAction) {
    (
        PriPhase::AwaitingVerify,
        PriAction::PromptReviewer {
            msg: format!(
                "You are the REVIEWER (verify step, read-only).\n\n\
                 Verify the implementation matches the plan at `{plan_path}`.\n\
                 Do NOT edit files. Check for mid-edit conflicts before reading.\n\
                 Reply with first token VERIFIED or REJECT:<reason>."
            ),
        },
    )
}

/// After planner settles with a plan path: send review prompt.
#[must_use]
pub fn pri_after_plan(plan_path: &str) -> (PriPhase, PriAction) {
    (
        PriPhase::AwaitingPlanReview,
        PriAction::PromptReviewer {
            msg: format!(
                "You are the REVIEWER (plan gate).\n\n\
                 Review the plan at `{plan_path}`.\n\
                 Reply with first token APPROVE or CHANGES:<notes>.\n\
                 Do not implement. Do not merge."
            ),
        },
    )
}

/// Extract `PLAN: <path>` from planner reply (best-effort).
#[must_use]
pub fn extract_plan_path(text: &str, fallback: &str) -> String {
    for line in text.lines() {
        let t = line.trim();
        let upper = t.to_ascii_uppercase();
        if let Some(rest) = upper.strip_prefix("PLAN:") {
            if let Some((_, path)) = t.split_once(':') {
                let p = path.trim();
                if !p.is_empty() {
                    return p.to_string();
                }
            }
            let p = rest.trim();
            if !p.is_empty() {
                return p.to_string();
            }
        }
    }
    fallback.to_string()
}

/// `flywheel plan-review <project> <feature> [--max-change-rounds 2] ...`
///
/// Threaded gate (thread_id = plan-<bead|ts>). Paper-first: implementer does
/// not merge; VERIFIED does not close beads.
///
/// governed-by: ADR-0001 §9.2 (bd-pattern-plan-review-implement)
/// clap command signature — params map 1:1 to CLI flags.
#[allow(clippy::too_many_arguments)]
/// Dispatched from `main()`; moved from crate root (bd-wave5-main-eviction-d5hht).
pub(crate) fn cmd_plan_review(
    cfg: &Config,
    project: &str,
    feature: &str,
    max_change_rounds: u32,
    planner: Option<&str>,
    reviewer: Option<&str>,
    implementer: Option<&str>,
    bead: Option<&str>,
) -> Result<()> {
    let max_rounds = max_change_rounds.max(1);
    let ts = crate::state::now_iso().replace(['-', ':'], "");
    let thread = match bead {
        Some(id) => format!("plan-{id}"),
        None => format!("plan-{ts}"),
    };
    let planner = resolve_agent_default(project, planner.unwrap_or("oc"));
    let reviewer = resolve_agent_default(project, reviewer.unwrap_or("pi"));
    let implementer = resolve_agent_default(project, implementer.unwrap_or("gk"));
    if planner == reviewer || planner == implementer || reviewer == implementer {
        bail!("plan-review needs three distinct agents (planner/reviewer/implementer)");
    }

    let plan_path = match bead {
        Some(id) => format!(".flywheel/plans/{id}.md"),
        None => format!(".flywheel/plans/plan-{ts}.md"),
    };

    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => bail!("mail backend unavailable for plan-review: {e:#}"),
    };
    let mux = backend::multiplexer_backend(cfg)?;
    let key = project_dir_str(cfg, project)?;
    backend::run_mail(mail.ensure_project(&key))?;

    let mail_target = |agent: &str| resolve_mail_target_registry(project, agent);
    let subject = format!("[plan-review] {project}");

    eprintln!(
        "plan-review {thread}: planner={} reviewer={} implementer={} max_change_rounds={max_rounds} paper-first",
        planner, reviewer, implementer
    );

    let mut change_rounds = 0u32;
    let mut current_path = plan_path.clone();

    let open = pri_planner_prompt(feature, &current_path);
    mail_send(
        &*mail,
        &key,
        &[mail_target(&planner)],
        &subject,
        &open,
        "high",
        true,
        Some(&thread),
    )?;
    settle_agent(&*mux, &planner)?;
    let planner_mail = mail_target(&planner);
    let plan_reply = harvest_latest(mail.as_ref(), &key, &thread, &planner_mail)?;
    current_path = extract_plan_path(&plan_reply, &current_path);
    let (_next_phase, action) = pri_after_plan(&current_path);
    let mut phase = PriPhase::AwaitingPlanReview;

    // governed-by: ADR-0001 §9.2 (bd-review-fix-epic-m0b.2)
    let mut action = action;
    loop {
        match phase {
            PriPhase::AwaitingPlan => {
                let msg = match &action {
                    PriAction::PromptPlanner { msg } => msg.clone(),
                    _ => bail!("phase AwaitingPlan without PromptPlanner action: {action:?}"),
                };
                mail_send(
                    &*mail,
                    &key,
                    &[mail_target(&planner)],
                    &subject,
                    &msg,
                    "high",
                    true,
                    Some(&thread),
                )?;
                settle_agent(&*mux, &planner)?;
                let pr = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&planner))?;
                current_path = extract_plan_path(&pr, &current_path);
                let (p2, a2) = pri_after_plan(&current_path);
                phase = p2;
                action = a2;
            }
            PriPhase::AwaitingPlanReview => {
                let msg = match &action {
                    PriAction::PromptReviewer { msg } => msg.clone(),
                    _ => {
                        bail!("phase AwaitingPlanReview without PromptReviewer action: {action:?}")
                    }
                };
                mail_send(
                    &*mail,
                    &key,
                    &[mail_target(&reviewer)],
                    &subject,
                    &msg,
                    "high",
                    true,
                    Some(&thread),
                )?;
                settle_agent(&*mux, &reviewer)?;
                let body = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&reviewer))?;
                let token = parse_review_token(&body).ok_or_else(|| {
                    anyhow!(
                        "reviewer reply missing APPROVE|CHANGES:|VERIFIED|REJECT: token: {body}"
                    )
                })?;
                let (p, a, rounds) =
                    pri_route(phase, &token, &current_path, change_rounds, max_rounds);
                phase = p;
                action = a;
                change_rounds = rounds;
            }
            PriPhase::AwaitingImplement => {
                let msg = match &action {
                    PriAction::PromptImplementer { msg } => msg.clone(),
                    _ => bail!(
                        "phase AwaitingImplement without PromptImplementer action: {action:?}"
                    ),
                };
                mail_send(
                    &*mail,
                    &key,
                    &[mail_target(&implementer)],
                    &subject,
                    &msg,
                    "high",
                    true,
                    Some(&thread),
                )?;
                settle_agent(&*mux, &implementer)?;
                let (p2, a2) = pri_after_implement(&current_path);
                phase = p2;
                action = a2;
            }
            PriPhase::AwaitingVerify => {
                let msg = match &action {
                    PriAction::PromptReviewer { msg } => msg.clone(),
                    _ => bail!("phase AwaitingVerify without PromptReviewer action: {action:?}"),
                };
                mail_send(
                    &*mail,
                    &key,
                    &[mail_target(&reviewer)],
                    &subject,
                    &msg,
                    "high",
                    true,
                    Some(&thread),
                )?;
                settle_agent(&*mux, &reviewer)?;
                let rev2 = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&reviewer))?;
                let token2 = parse_review_token(&rev2).ok_or_else(|| {
                    anyhow!("verify reply missing VERIFIED|REJECT: token: {rev2}")
                })?;
                let (p3, a3, _) =
                    pri_route(phase, &token2, &current_path, change_rounds, max_rounds);
                phase = p3;
                action = a3;
            }
            PriPhase::Done => {
                println!("plan-review {thread}: VERIFIED (paper-first — human merge/close)");
                return Ok(());
            }
            PriPhase::Escalated => {
                match &action {
                    PriAction::Escalate { reason } => {
                        eprintln!("plan-review {thread}: escalate: {reason}");
                    }
                    other => eprintln!(
                        "plan-review {thread}: escalated with unexpected action {other:?}"
                    ),
                }
                return Ok(());
            }
        }
    }
}

/// Resolve the A/B/judge agents for a debate: explicit names, or default to
/// the swarm's first agents. Returns the (a, b, j) hc agent names.
/// Resolve the A/B/judge agents for a debate: explicit names, or default to
/// the swarm's first agents. Returns the (a, b, j) hc agent names.
/// review-fix F7: bare shorts (`oc`/`pi`/`gk`) expand to the spawned
/// `flywheel-<base>-<short>` names; explicit full names pass through.
fn resolve_debate_agents<'a>(
    project: &str,
    agent_a: Option<&'a str>,
    agent_b: Option<&'a str>,
    judge: Option<&'a str>,
) -> Result<(String, String, String)> {
    let a = resolve_agent_default(project, agent_a.unwrap_or("oc"));
    let b = resolve_agent_default(project, agent_b.unwrap_or("pi"));
    let j = resolve_agent_default(project, judge.unwrap_or("gk"));
    if a == b {
        bail!("debate needs two distinct agents (A and B are both '{a}')");
    }
    Ok((a, b, j))
}

/// Wait for an agent to settle (done/idle/blocked) — the turn-finality gate.
fn settle_agent(mux: &dyn backend::MultiplexerBackend, agent: &str) -> Result<()> {
    mux.agent_wait(
        agent,
        &[
            crate::hc::AgentStatus::Done,
            crate::hc::AgentStatus::Idle,
            crate::hc::AgentStatus::Blocked,
        ],
        Some(1_800_000),
    )
    .map_err(|e| anyhow!("agent {agent} settle failed: {e}"))?;
    Ok(())
}

/// Pure extraction of the latest message's body from a `search_messages`
/// envelope. Precedence: `body_md` → `body` → `subject` (mirrors
/// `extract_xproj_from_inbox`). Trims and rejects empty. Returns None when
/// the envelope has no result or no readable text.
///
/// review-fix F1: the debate/plan-review patterns need the reply BODY (the
/// argument / the APPROVE|CHANGES: token), not the subject line.
///
/// governed-by: ADR-0001 §9.1/§9.2 (bd-review-fix-epic-m0b.1)
#[cfg(test)]
pub fn extract_latest_body(v: &serde_json::Value) -> Option<String> {
    v.get("result")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|m| {
            m.get("body_md")
                .or_else(|| m.get("body"))
                .or_else(|| m.get("subject"))
                .and_then(serde_json::Value::as_str)
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod debate_tests {
    use super::*;

    #[test]
    fn debate_resolves_default_agents() {
        let (_a, _b, j) = resolve_debate_agents("hachiko", None, None, None).unwrap();
        assert_eq!(j, "flywheel-hachiko-gk");
    }

    #[test]
    fn debate_rejects_same_agent_for_a_and_b() {
        let r = resolve_debate_agents("hachiko", Some("oc"), Some("oc"), None);
        assert!(r.is_err(), "A and B must be distinct");
    }

    #[test]
    fn debate_accepts_explicit_agents() {
        let (a, b, j) =
            resolve_debate_agents("hachiko", Some("pi"), Some("gk"), Some("oc")).unwrap();
        assert_eq!(a, "flywheel-hachiko-pi");
        assert_eq!(b, "flywheel-hachiko-gk");
        assert_eq!(j, "flywheel-hachiko-oc");
    }
}

#[cfg(test)]
mod plan_review_tests {
    use super::*;
    use crate::newest_in_thread;

    #[test]
    fn parse_review_token_approve_verified() {
        assert_eq!(parse_review_token("APPROVE"), Some(ReviewToken::Approve));
        assert_eq!(
            parse_review_token("approve\nmore text"),
            Some(ReviewToken::Approve)
        );
        assert_eq!(parse_review_token("VERIFIED"), Some(ReviewToken::Verified));
    }

    #[test]
    fn parse_review_token_changes_and_reject() {
        assert_eq!(
            parse_review_token("CHANGES: need more tests"),
            Some(ReviewToken::Changes("need more tests".into()))
        );
        assert_eq!(
            parse_review_token("REJECT: out of scope"),
            Some(ReviewToken::Reject("out of scope".into()))
        );
        assert_eq!(parse_review_token("lgtm maybe"), None);
    }

    #[test]
    fn pri_route_approve_path() {
        let (p, a, rounds) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Approve,
            ".flywheel/plans/x.md",
            0,
            2,
        );
        assert_eq!(p, PriPhase::AwaitingImplement);
        assert_eq!(rounds, 0);
        assert!(matches!(a, PriAction::PromptImplementer { .. }));
        let (p2, a2) = pri_after_implement(".flywheel/plans/x.md");
        assert_eq!(p2, PriPhase::AwaitingVerify);
        assert!(matches!(a2, PriAction::PromptReviewer { .. }));
        let (p3, a3, _) = pri_route(
            PriPhase::AwaitingVerify,
            &ReviewToken::Verified,
            ".flywheel/plans/x.md",
            0,
            2,
        );
        assert_eq!(p3, PriPhase::Done);
        assert_eq!(a3, PriAction::HaltVerified);
    }

    /// review-fix F1: harvest_latest content extraction must prefer the reply
    /// BODY over the subject. The exact agent-mail search envelope (subject +
    /// body_md) must yield the body; subject-only must fall back; empty → None.
    #[test]
    fn extract_latest_body_prefers_body_over_subject() {
        let v = serde_json::json!({
            "result": [{
                "id": 1,
                "subject": "Re: [debate] prop",
                "body_md": "ARGUMENT: buy the dip",
                "from": "TopazOx",
                "thread_id": "debate-1"
            }]
        });
        assert_eq!(
            extract_latest_body(&v),
            Some("ARGUMENT: buy the dip".to_string())
        );
    }

    /// review-fix F1: a body-less envelope falls back to the subject.
    #[test]
    fn extract_latest_body_falls_back_to_subject_when_body_absent() {
        let v = serde_json::json!({
            "result": [{
                "id": 2,
                "subject": "Re: [debate] prop",
                "from": "TopazOx"
            }]
        });
        assert_eq!(
            extract_latest_body(&v),
            Some("Re: [debate] prop".to_string())
        );
    }

    /// review-fix F1: empty result / empty text → None (no phantom "").
    #[test]
    fn extract_latest_body_empty_is_none() {
        assert_eq!(
            extract_latest_body(&serde_json::json!({"result": []})),
            None
        );
        assert_eq!(extract_latest_body(&serde_json::json!({})), None);
        let v = serde_json::json!({
            "result": [{"id": 3, "subject": "   ", "body_md": ""}]
        });
        assert_eq!(extract_latest_body(&v), None);
    }

    /// bd-debate-harvest-dsww: inbox-projection selection replaces FTS
    /// search — a hyphenated thread id (`debate-20260806T130548`) must
    /// match EXACTLY on the row's thread_id (no FTS tokenization), the
    /// newest in-thread row from the sender wins, and rows outside the
    /// thread or from other senders never leak in.
    #[test]
    fn newest_in_thread_matches_exact_thread_and_newest() {
        let mk =
            |id: &str, sender: &str, thread: Option<&str>, ts: i64, body: &str| backend::MailItem {
                id: id.parse().expect("valid message id"),
                sender: sender.to_string(),
                subject: format!("Re: [debate] prop {id}"),
                body_md: body.to_string(),
                importance: "high".to_string(),
                ack_required: true,
                thread_id: thread.map(|value| value.parse().expect("valid thread id")),
                bead: None,
                created_ts_us: ts,
                duplicate_of: None,
            };
        let thread = "debate-20260806T130548";
        let rows = vec![
            mk("o1", "CoralFinch", Some(thread), 1_000, "opening prompt"),
            mk("r1", "TopazOx", Some(thread), 2_000, "counter one"),
            mk("r2", "TopazOx", Some(thread), 3_000, "counter two"),
            mk("x1", "TopazOx", Some("debate-other"), 9_000, "other thread"),
            mk(
                "x2",
                "CoralFinch",
                Some(thread),
                9_500,
                "orchestrator prompt",
            ),
        ];
        let hit = newest_in_thread(&rows, thread, "TopazOx", "").expect("reply must be found");
        assert_eq!(hit.id.to_string(), "r2");
        assert_eq!(hit.body_md, "counter two");
        assert!(newest_in_thread(&rows, thread, "NeverReplied", "").is_none());
    }

    /// bd-debate-harvest-dsww / bd-debate-reply-routing-99ad: the sender
    /// matches by NAME (toron resolves registered npubs, agent-mail returns
    /// `from`) AND by raw npub hex (toron's fallback when the sender never
    /// registered) — both inbox projections land in the same match.
    #[test]
    fn newest_in_thread_matches_name_or_npub() {
        let mk = |id: &str, sender: &str, ts: i64| backend::MailItem {
            id: id.parse().expect("valid message id"),
            sender: sender.to_string(),
            subject: "Re: [debate] prop".to_string(),
            body_md: "arg".to_string(),
            importance: "high".to_string(),
            ack_required: true,
            thread_id: Some("debate-20260806T130548".parse().expect("valid thread id")),
            bead: None,
            created_ts_us: ts,
            duplicate_of: None,
        };
        let npub = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let rows = vec![mk("n1", "TopazOx", 1_000), mk("n3", "someone-else", 3_000)];
        let by_name = newest_in_thread(&rows, "debate-20260806T130548", "TopazOx", npub).unwrap();
        assert_eq!(by_name.id.to_string(), "n1");
        let rows = vec![mk("n2", npub, 2_000), mk("n3", "someone-else", 3_000)];
        let by_npub = newest_in_thread(&rows, "debate-20260806T130548", "TopazOx", npub).unwrap();
        assert_eq!(by_npub.id.to_string(), "n2");
        assert!(newest_in_thread(&rows, "debate-20260806T130548", "TopazOx", "").is_none());
    }

    /// review-fix F2: the gate is a real state machine, not a single pass.
    /// Walk the driver's phase machine over a scripted reviewer token sequence
    /// (planner always replies PLAN, reviewer replies the given tokens in
    /// order, implementer replies IMPLEMENT_DONE) and assert the visited phase
    /// path + final outcome. This is the test that would have caught the
    /// original never_loop defect: [Changes, Approve] must visit
    /// AwaitingPlanReview TWICE (two CHANGES cycles) before reaching
    /// AwaitingImplement → AwaitingVerify → Done.
    #[test]
    fn plan_review_gate_loops_on_changes_then_verifies() {
        let path = ".flywheel/plans/x.md";
        let mut phases: Vec<PriPhase> = Vec::new();
        let mut phase = PriPhase::AwaitingPlanReview;
        let mut action = pri_after_plan(path).1;
        let mut change_rounds = 0u32;
        let mut reviewer_tokens = [
            ReviewToken::Changes("fix a".into()),
            ReviewToken::Approve,
            ReviewToken::Verified,
        ]
        .into_iter();
        let max_rounds = 2u32;
        loop {
            phases.push(phase);
            match phase {
                PriPhase::AwaitingPlanReview => {
                    let token = reviewer_tokens
                        .next()
                        .expect("scripted reviewer ran out of tokens");
                    let (_, a, r) = pri_route(phase, &token, path, change_rounds, max_rounds);
                    action = a;
                    change_rounds = r;
                    match &action {
                        PriAction::PromptImplementer { .. } => {
                            phase = pri_after_implement(path).0;
                            action = pri_after_implement(path).1;
                        }
                        PriAction::PromptPlanner { .. } => {
                            phase = pri_after_plan(path).0;
                            action = pri_after_plan(path).1;
                        }
                        _ => panic!("unexpected action after review: {action:?}"),
                    }
                }
                PriPhase::AwaitingVerify => {
                    let token = reviewer_tokens
                        .next()
                        .expect("scripted verifier ran out of tokens");
                    let (p, a, _) = pri_route(phase, &token, path, change_rounds, max_rounds);
                    phase = p;
                    action = a;
                }
                PriPhase::Done => break,
                PriPhase::Escalated => panic!("unexpected escalation: {action:?}"),
                other => panic!("unexpected phase: {other:?}"),
            }
        }
        phases.push(PriPhase::Done);
        let reviews = phases
            .iter()
            .filter(|p| matches!(p, PriPhase::AwaitingPlanReview))
            .count();
        assert_eq!(reviews, 2, "phase path: {phases:?}");
        assert_eq!(change_rounds, 1);
        assert!(matches!(action, PriAction::HaltVerified));
    }

    /// review-fix F2: change rounds are HONORED — [Changes, Changes, Changes]
    /// with max 2 escalates on the third instead of looping forever.
    #[test]
    fn plan_review_gate_escalates_when_change_rounds_exhausted() {
        let path = ".flywheel/plans/x.md";
        let mut phase = PriPhase::AwaitingPlanReview;
        let mut action = pri_after_plan(path).1;
        let mut change_rounds = 0u32;
        let mut reviewer_tokens = [
            ReviewToken::Changes("a".into()),
            ReviewToken::Changes("b".into()),
            ReviewToken::Changes("c".into()),
        ]
        .into_iter();
        let max_rounds = 2u32;
        let mut iterations = 0u32;
        loop {
            iterations += 1;
            assert!(iterations < 20, "gate must terminate (max_change_rounds)");
            match phase {
                PriPhase::AwaitingPlanReview => {
                    let token = reviewer_tokens
                        .next()
                        .expect("scripted reviewer ran out of tokens");
                    let (p, a, r) = pri_route(phase, &token, path, change_rounds, max_rounds);
                    phase = p;
                    action = a;
                    change_rounds = r;
                    if matches!(action, PriAction::Escalate { .. }) {
                        assert_eq!(change_rounds, 3);
                        assert!(matches!(phase, PriPhase::Escalated));
                        return;
                    }
                    match &action {
                        PriAction::PromptPlanner { .. } => {
                            phase = pri_after_plan(path).0;
                            action = pri_after_plan(path).1;
                        }
                        other => panic!("unexpected: {other:?}"),
                    }
                }
                PriPhase::Escalated => {
                    assert!(matches!(action, PriAction::Escalate { .. }));
                    return;
                }
                other => panic!("unexpected phase: {other:?}"),
            }
        }
    }

    #[test]
    fn pri_route_changes_then_exhaust() {
        let (p, a, r) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Changes("fix".into()),
            "p.md",
            0,
            2,
        );
        assert_eq!(p, PriPhase::AwaitingPlan);
        assert_eq!(r, 1);
        assert!(matches!(a, PriAction::PromptPlanner { .. }));
        let (p2, _, r2) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Changes("more".into()),
            "p.md",
            1,
            2,
        );
        assert_eq!(p2, PriPhase::AwaitingPlan);
        assert_eq!(r2, 2);
        let (p3, a3, r3) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Changes("again".into()),
            "p.md",
            2,
            2,
        );
        assert_eq!(p3, PriPhase::Escalated);
        assert_eq!(r3, 3);
        assert!(matches!(a3, PriAction::Escalate { .. }));
    }

    #[test]
    fn pri_route_reject_escalates() {
        let (p, a, _) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Reject("nope".into()),
            "p.md",
            0,
            2,
        );
        assert_eq!(p, PriPhase::Escalated);
        assert!(matches!(a, PriAction::Escalate { reason } if reason.contains("rejected")));
    }

    #[test]
    fn extract_plan_path_from_reply() {
        assert_eq!(
            extract_plan_path("PLAN: .flywheel/plans/bd-1.md\nthanks", "fallback.md"),
            ".flywheel/plans/bd-1.md"
        );
        assert_eq!(extract_plan_path("no path here", "fb.md"), "fb.md");
    }
}

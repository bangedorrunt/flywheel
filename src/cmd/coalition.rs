//! `flywheel coalition` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

use anyhow::{Result, anyhow, bail};
use serde_json::json;
use std::collections::BTreeMap;

use crate::output::print_json;
use crate::{config, profile};

/// Coalition commands (ADR-0011 Spec 1, P11.3a): the steer surface + manual
/// collab. A coalition = NIP-29 channel + goal workflow run + manifest.
/// Manual collab reuses P11.2 (soft-conflict freeze + request-release) — no
/// auto-proposer here (P11.3b).
#[derive(Subcommand)]
pub(crate) enum CoalitionCmd {
    /// Register an agent identity in the coalition's toron project (members
    /// must be registered before `form`; profiles are not agents).
    Register {
        /// agent name (key name in the project's agents dir)
        name: String,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Form a captain-led coalition around a goal workflow: lint gate, start
    /// the goal run, create the NIP-29 channel, write the manifest.
    Form {
        /// goal workflow name (must pass the lint gate)
        goal: String,
        /// crew: comma-separated squads (recipes.toml) / profile names —
        /// one per project for cross-project coalitions (bd-hkw5.3)
        #[arg(long)]
        crews: String,
        /// act as this agent (the captain; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// cross-project scoping: <member>=<project> (repeatable; absent
        /// members default to --project) (bd-hkw5.3)
        #[arg(long = "member-project")]
        member_project: Vec<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// bead ids tracked for the beads-closed disband gate (comma list)
        #[arg(long)]
        beads: Option<String>,
        /// the repo whose .beads holds the tracked beads (br resolves its
        /// workspace from cwd; default: none — probe from the current dir)
        #[arg(long)]
        tracker: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Full coalition view: manifest + run + channel + intents + reservations
    /// + frozen paths + in-flight negotiations + bead gate.
    Status {
        /// coalition id (from form / list)
        id: String,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// compact one-line-per-member digest (bd-hkw5.1)
        #[arg(long)]
        digest: bool,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Declare status:intent into the coalition's intent board (P11.2).
    Intent {
        /// coalition id
        id: String,
        /// path globs the intent covers
        paths: Vec<String>,
        /// act as this agent (must be a member)
        #[arg(long = "as")]
        as_agent: String,
        #[arg(long)]
        reason: Option<String>,
        /// intent TTL in seconds (default 300)
        #[arg(long)]
        ttl: Option<u64>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Acquire leases on path globs (P11.2). A soft-conflict FREEZE against
    /// another member's intent lands in soft_conflicts — the manual-collab
    /// trigger.
    Reserve {
        /// coalition id
        id: String,
        /// path globs to reserve
        paths: Vec<String>,
        /// act as this agent (must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// lease TTL in seconds
        #[arg(long)]
        ttl: Option<u64>,
        /// exclusive lease (default: true)
        #[arg(long)]
        exclusive: bool,
        #[arg(long)]
        reason: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: ask the holder to release/defer (P11.2).
    NegotiateRequest {
        /// coalition id
        id: String,
        /// act as this agent (the requester; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the holder being asked (must be a member)
        #[arg(long)]
        target: String,
        /// reservation id (kind-30031 event id from reserve)
        #[arg(long)]
        reservation_id: String,
        /// ask: `release` | `defer:<secs>`
        #[arg(long)]
        ask: String,
        #[arg(long)]
        note: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: the holder answers (P11.2).
    NegotiateRespond {
        /// coalition id
        id: String,
        /// act as this agent (the holder; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the request message id from negotiate-request
        #[arg(long)]
        request_message: String,
        /// decision: `agreed` | `denied` | `deferred:<secs>`
        #[arg(long)]
        decision: String,
        #[arg(long)]
        note: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: negotiation resolution (pending / responded /
    /// escalation-due) (P11.2).
    NegotiateStatus {
        /// coalition id
        id: String,
        /// act as this agent (the requester; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the request message id from negotiate-request
        #[arg(long)]
        request_message: String,
        /// escalation tier window in seconds (default 900)
        #[arg(long)]
        window: Option<u64>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: escalate an unanswered request to the captain
    /// (kind-14 DM, P11.2).
    NegotiateEscalate {
        /// coalition id
        id: String,
        /// act as this agent (the requester; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the request message id from negotiate-request
        #[arg(long)]
        request_message: String,
        #[arg(long)]
        note: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// ADR-0011 D5: join an active coalition (wires CoalitionStore::join).
    Join {
        id: String,
        #[arg(long = "as")]
        as_agent: String,
        #[arg(long)]
        member_project: Option<String>,
        #[arg(long, default_value = "toron")]
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// Leave an active coalition.
    Leave {
        id: String,
        #[arg(long = "as")]
        as_agent: String,
        #[arg(long, default_value = "toron")]
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// List coalitions (id, goal, status).
    List {
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// P11.3b detector: scan the intent board for overlapping crews and
    /// propose tiered rule-splits (split-by-directory / sequence-by-
    /// dependency / merge) + flag auto-coalesce candidates (non-trivial
    /// source overlaps between unaffiliated crews).
    Detect {
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// P11.3b auto-coalesce: form an ad-hoc coalition around a goal workflow
    /// for every non-trivial overlap the detector flagged between
    /// unaffiliated crews.
    AutoCoalesce {
        /// goal workflow name (must pass the lint gate)
        goal: String,
        /// act as this agent (the captain; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Disband a coalition: beads-closed gate (when tracked), member
    /// reservations released, channel close post, manifest closed.
    Disband {
        /// coalition id
        id: String,
        /// require every tracked bead to be closed first (the primary
        /// disband path; manual disband is the auto-fallback)
        #[arg(long)]
        beads_closed: bool,
        #[arg(long)]
        reason: Option<String>,
        /// override the tracker repo recorded at form (br cwd for the
        /// beads-closed probe)
        #[arg(long)]
        tracker: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Dispatch a headless leaf worker (bd-hkw5.2): the captain pre-splits
    /// a non-overlapping scope; the leaf runs it one-shot (ADR-0010 Spec A
    /// subprocess-reuse), logs to runs/, and posts the result to the
    /// coalition channel. Leaf semantics — no intent-declare/negotiate.
    Leaf {
        /// coalition id
        id: String,
        /// the acting member (leaf worker; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// profile ref the leaf runs under (@planner)
        #[arg(long)]
        profile: String,
        /// the captain's pre-split, non-overlapping scope
        #[arg(long)]
        scope: String,
        /// per-dispatch harness override (pi|omp|opencode|grok)
        #[arg(long)]
        harness: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
}

/// `flywheel coalition ...` (ADR-0011 Spec 1, P11.3a): the steer surface +
/// manual collab baseline. All collab ops are membership-gated; P11.2's
/// soft-conflict freeze + request-release ladder are the manual resolution
/// path (the auto-proposer is P11.3b).
/// Dispatched from `main()`; moved from crate root (bd-wave5-main-eviction-d5hht).
pub(crate) fn cmd_coalition(cmd: &CoalitionCmd) -> Result<()> {
    use crate::coalition::CoalitionStore;
    use toron_core::mailbox::{ReleaseAsk, ReleaseDecision};

    /// Resolve a crew spec to member profile names (same rule as
    /// `workflow run --crew`): recipes.toml squad, else a single profile.
    /// Comma-separated crews (bd-hkw5.3): multiple squads/profiles — the
    /// cross-project coalition's crews from different repos.
    fn squad_members(crews: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for crew in crews.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            let cat = profile::ProfileCatalog::load()?;
            let recipes = config::Recipes::load()?.recipes;
            let counts = profile::resolve_squad(crew, &recipes, cat.get(crew).is_some())
                .ok_or_else(|| anyhow!("unknown squad {crew:?} (flywheel squad list)"))?
                .counts;
            for m in counts.keys() {
                if !out.contains(m) {
                    out.push(m.clone());
                }
            }
        }
        Ok(out)
    }

    fn coalitions_root() -> std::path::PathBuf {
        config::state_dir().join("coalitions")
    }

    fn parse_ask(s: &str) -> Result<ReleaseAsk> {
        if s == "release" {
            Ok(ReleaseAsk::Release)
        } else if let Some(secs) = s.strip_prefix("defer:") {
            Ok(ReleaseAsk::Defer {
                defer_secs: secs
                    .parse()
                    .map_err(|e| anyhow!("--ask defer:<secs> parse: {e}"))?,
            })
        } else {
            bail!("--ask must be `release` or `defer:<secs>`, got {s:?}")
        }
    }

    fn parse_decision(s: &str) -> Result<ReleaseDecision> {
        if s == "agreed" {
            Ok(ReleaseDecision::Agreed)
        } else if s == "denied" {
            Ok(ReleaseDecision::Denied)
        } else if let Some(secs) = s.strip_prefix("deferred:") {
            Ok(ReleaseDecision::Deferred {
                defer_secs: secs
                    .parse()
                    .map_err(|e| anyhow!("--decision deferred:<secs> parse: {e}"))?,
            })
        } else {
            bail!("--decision must be agreed|denied|deferred:<secs>, got {s:?}")
        }
    }

    match cmd {
        CoalitionCmd::Register {
            name,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            store.register_agent(name)?;
            if *json {
                print_json(&json!({ "registered": name, "project": project }));
            } else {
                println!("registered agent {name} in project {project}");
            }
            Ok(())
        }
        CoalitionCmd::Form {
            goal,
            crews,
            as_agent,
            member_project,
            project,
            beads,
            tracker,
            json,
        } => {
            let members = squad_members(crews)?;
            let member_projects: BTreeMap<String, String> = member_project
                .iter()
                .map(|kv| {
                    let (member, proj) = kv.split_once('=').ok_or_else(|| {
                        anyhow!("--member-project expects <member>=<project>, got {kv:?}")
                    })?;
                    if member.is_empty() || proj.is_empty() {
                        return Err(anyhow!(
                            "--member-project expects <member>=<project>, got {kv:?}"
                        ));
                    }
                    if !members.contains(&member.to_string()) {
                        return Err(anyhow!(
                            "--member-project member {member} is not in crew {crews:?}"
                        ));
                    }
                    Ok((member.to_string(), proj.to_string()))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let beads: Vec<String> = beads
                .as_deref()
                .map(|s| {
                    s.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let store = CoalitionStore::open(project, coalitions_root())?;
            let m = store.form(
                goal,
                as_agent,
                &members,
                &member_projects,
                &beads,
                tracker.as_deref(),
            )?;
            if *json {
                print_json(&serde_json::to_value(&m)?);
            } else {
                println!(
                    "coalition {} formed: goal {}, {} member(s), channel {} + run {}",
                    m.id,
                    m.goal,
                    m.members.len(),
                    m.channel_id,
                    m.run_id
                );
                if !m.member_projects.is_empty() {
                    for (member, proj) in &m.member_projects {
                        println!("  {member} -> project {proj}");
                    }
                }
            }
            Ok(())
        }
        CoalitionCmd::Status {
            id,
            project,
            json,
            digest,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let st = store.status(id)?;
            if *json {
                print_json(&serde_json::to_value(&st)?);
            } else if *digest {
                println!(
                    "coalition {} [{}] goal {} captain {}",
                    st.id, st.status, st.goal, st.captain
                );
                let mut by_member: std::collections::BTreeMap<String, (usize, usize, usize)> =
                    std::collections::BTreeMap::new();
                for i in &st.intents {
                    by_member.entry(i.agent_name.clone()).or_default().0 += 1;
                }
                for r in &st.reservations {
                    by_member.entry(r.agent_name.clone()).or_default().1 += 1;
                }
                for f in &st.frozen {
                    by_member.entry(f.agent.clone()).or_default().2 += 1;
                }
                for m in &st.members {
                    let (intents, held, frozen) = by_member.get(m).copied().unwrap_or((0, 0, 0));
                    println!(
                        "  {} [{}] intents {} held {} frozen {}",
                        m,
                        st.roles.get(m).map(String::as_str).unwrap_or("member"),
                        intents,
                        held,
                        frozen
                    );
                }
                println!(
                    "  negotiations: {} | channel events: {} | run: {}",
                    st.negotiations.len(),
                    st.channel.events.len(),
                    st.run
                        .as_ref()
                        .map(|r| r.status.as_str())
                        .unwrap_or("unknown")
                );
                for n in &st.negotiations {
                    println!(
                        "    {} -> {}: {} ({})",
                        n.requester, n.holder, n.path, n.status
                    );
                }
            } else {
                println!(
                    "coalition {} [{}] goal {} captain {}",
                    st.id, st.status, st.goal, st.captain
                );
                println!(
                    "  run: {} ({})",
                    st.run_id,
                    st.run
                        .as_ref()
                        .map(|r| r.status.as_str())
                        .unwrap_or("unknown")
                );
                for (member, proj) in &st.member_projects {
                    println!("  {member} -> project {proj}");
                }
                println!(
                    "  channel: {} ({} event(s) on bus)",
                    st.channel.channel_id,
                    st.channel.events.len()
                );
                println!(
                    "  intents: {} | reservations: {} | frozen: {} | negotiations: {}",
                    st.intents.len(),
                    st.reservations.len(),
                    st.frozen.len(),
                    st.negotiations.len()
                );
                for i in &st.intents {
                    let ttl = i
                        .expires_ts_us
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "none".into());
                    println!(
                        "    intent {}: {} @ {} (ttl {}s, expires {})",
                        i.agent_name, i.path_pattern, i.reason, i.ttl_secs, ttl
                    );
                }
                for r in &st.reservations {
                    println!(
                        "    held {}: {} (expires {})",
                        r.agent_name,
                        r.path_pattern,
                        r.expires_ts_us
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "none".into())
                    );
                }
                for f in &st.frozen {
                    println!(
                        "    FROZEN {}: {} (overlaps {})",
                        f.agent,
                        f.path,
                        f.holders.join(", ")
                    );
                }
                for n in &st.negotiations {
                    println!(
                        "    negotiate {} -> {}: {} ({})",
                        n.requester, n.holder, n.path, n.status
                    );
                }
                for b in &st.beads {
                    println!("    bead {}: {}", b.id, b.status);
                }
            }
            Ok(())
        }
        CoalitionCmd::Intent {
            id,
            paths,
            as_agent,
            reason,
            ttl,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let res = store.declare_intent(id, as_agent, paths, reason.as_deref(), *ttl)?;
            if *json {
                print_json(&serde_json::to_value(&res)?);
            } else {
                println!(
                    "intent declared: {} path(s) by {as_agent}",
                    res.declared.len()
                );
            }
            Ok(())
        }
        CoalitionCmd::Reserve {
            id,
            paths,
            as_agent,
            ttl,
            exclusive,
            reason,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let res = store.reserve(id, as_agent, paths, *ttl, *exclusive, reason.as_deref())?;
            if *json {
                print_json(&serde_json::to_value(&res)?);
            } else {
                println!(
                    "reserve: {} granted, {} hard conflict(s), {} SOFT conflict(s) (freeze)",
                    res.granted.len(),
                    res.conflicts.len(),
                    res.soft_conflicts.len()
                );
            }
            Ok(())
        }
        CoalitionCmd::NegotiateRequest {
            id,
            as_agent,
            target,
            reservation_id,
            ask,
            note,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let res = store.negotiate_request(
                id,
                as_agent,
                target,
                reservation_id,
                parse_ask(ask)?,
                note.as_deref(),
            )?;
            if *json {
                print_json(&serde_json::to_value(&res)?);
            } else {
                println!(
                    "request-release sent to {} for {} (message {})",
                    res.holder, res.path_pattern, res.message_id
                );
            }
            Ok(())
        }
        CoalitionCmd::NegotiateRespond {
            id,
            as_agent,
            request_message,
            decision,
            note,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let res = store.negotiate_respond(
                id,
                as_agent,
                request_message,
                parse_decision(decision)?,
                note.as_deref(),
            )?;
            if *json {
                print_json(&json!({
                    "deliveries": res.deliveries.iter().map(|d| json!({
                        "recipient": d.payload.recipient,
                        "event_id": d.payload.event_id,
                    })).collect::<Vec<_>>(),
                    "count": res.count,
                }));
            } else {
                println!(
                    "response delivered to {} recipient(s)",
                    res.deliveries.len()
                );
            }
            Ok(())
        }
        CoalitionCmd::NegotiateStatus {
            id,
            as_agent,
            request_message,
            window,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let st = store.negotiate_status(id, as_agent, request_message, *window)?;
            if *json {
                print_json(&serde_json::to_value(&st)?);
            } else {
                println!("{st:?}");
            }
            Ok(())
        }
        CoalitionCmd::NegotiateEscalate {
            id,
            as_agent,
            request_message,
            note,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            store.negotiate_escalate(id, as_agent, request_message, note.as_deref())?;
            if *json {
                print_json(&json!({ "escalated": true }));
            } else {
                println!("request {request_message} escalated to the captain");
            }
            Ok(())
        }
        CoalitionCmd::Join {
            id,
            as_agent,
            member_project,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let m = store.join(id, as_agent, member_project.as_deref())?;
            eprintln!(
                "JOINING — not spawning  coalition={} members={}",
                m.id,
                m.members.len()
            );
            if *json {
                print_json(&serde_json::to_value(&m)?);
            } else {
                println!("joined {} as {as_agent} members {}", m.id, m.members.len());
            }
            Ok(())
        }
        CoalitionCmd::Leave {
            id,
            as_agent,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let m = store.leave(id, as_agent)?;
            if *json {
                print_json(&serde_json::to_value(&m)?);
            } else {
                println!("left {} as {as_agent} members {}", m.id, m.members.len());
            }
            Ok(())
        }
        CoalitionCmd::List { project, json } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let list = store.list()?;
            if *json {
                print_json(&serde_json::to_value(&list)?);
            } else {
                for m in &list {
                    println!(
                        "{} [{}] goal {} members {}",
                        m.id,
                        m.status,
                        m.goal,
                        m.members.len()
                    );
                }
            }
            Ok(())
        }
        CoalitionCmd::Detect { project, json } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let report = store.detect()?;
            if *json {
                print_json(&serde_json::to_value(&report)?);
            } else {
                if report.overlaps.is_empty() {
                    println!("no intent overlaps on the board");
                }
                for o in &report.overlaps {
                    let tag = if o.non_trivial { "source" } else { "trivial" };
                    println!("overlap: {} <-> {} ({tag})", o.a, o.b);
                    println!("  {} : {}", o.a, o.path_a);
                    println!("  {} : {}", o.b, o.path_b);
                }
                for p in &report.proposals {
                    println!(
                        "proposal [{tier}/{kind}]: {} <-> {} — {}",
                        p.agents[0],
                        p.agents[1],
                        p.rationale,
                        tier = p.tier,
                        kind = serde_json::to_string(&p.kind)
                            .unwrap_or_default()
                            .trim_matches('"')
                    );
                }
                for c in &report.auto_coalesce {
                    println!(
                        "auto-coalesce candidate: {} <-> {} (non-trivial source)",
                        c.a, c.b
                    );
                }
            }
            Ok(())
        }
        CoalitionCmd::AutoCoalesce {
            goal,
            as_agent,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let report = store.detect()?;
            if report.auto_coalesce.is_empty() {
                if *json {
                    print_json(&json!({ "formed": [] }));
                } else {
                    println!("no auto-coalesce candidates (no non-trivial unaffiliated overlap)");
                }
                return Ok(());
            }
            let formed = store.auto_coalesce(goal, as_agent, &report)?;
            if *json {
                print_json(&serde_json::to_value(&formed)?);
            } else {
                for m in &formed {
                    println!(
                        "formed coalition {} (goal {goal}, members {})",
                        m.id,
                        m.members.join(", ")
                    );
                }
            }
            Ok(())
        }
        CoalitionCmd::Disband {
            id,
            beads_closed,
            reason,
            tracker,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            store.disband(id, *beads_closed, reason.as_deref(), tracker.as_deref())?;
            if *json {
                print_json(&json!({ "disbanded": id }));
            } else {
                println!("coalition {id} disbanded");
            }
            Ok(())
        }
        CoalitionCmd::Leaf {
            id,
            as_agent,
            profile,
            scope,
            harness,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let leaf = store.dispatch_leaf(id, as_agent, profile, scope, harness.as_deref())?;
            if *json {
                print_json(&serde_json::to_value(&leaf)?);
            } else {
                println!(
                    "leaf @{} run {} [{}] {} tok={}+{} {}ms (result posted to coalition {})",
                    leaf.profile,
                    leaf.run_id,
                    leaf.status,
                    leaf.model,
                    leaf.tokens_in,
                    leaf.tokens_out,
                    leaf.duration_ms,
                    id
                );
                if let Some(e) = &leaf.error {
                    eprintln!("leaf error: {e}");
                }
            }
            Ok(())
        }
    }
}

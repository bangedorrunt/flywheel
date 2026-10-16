// governed-by: ADR-0011
//! Session briefing contract (`flywheel-briefing.v1`).
//!
//! Pure assemble: objective + world facts → ranked proposal. `decide` is the
//! classifier leaf. Joinable worlds default topology to `join`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::crew::{self, DEFAULT_CREW_MAX};
use crate::decide::{self, Decision};

pub const SCHEMA: &str = "flywheel-briefing.v1";

/// Default unique inventory when the caller has not yet inferred roles.
/// Scout/housekeeper are optional (observe/compound already `continue`);
/// remix via `--crew role:n,role:n`.
pub const WAVE_SWARM_ROLES: &[&str] = &["planner", "coder", "reviewer"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct World {
    pub project: String,
    pub ready_beads: u32,
    pub dispatcher: String,
    pub joinable: bool,
    pub degraded: Vec<String>,
    pub foreign_workspaces: Vec<String>,
    pub pin_count: u32,
    pub held_paths: Vec<String>,
    pub coalition_id: Option<String>,
    /// GS-049: agents still launch-pending (from snapshot assemble).
    #[serde(default)]
    pub launch_pending: u32,
    /// bd-compose-plumbing-zdzi: the pin this briefing runs as (the
    /// "arriving pin"). Bound into declared inputs the routed token can
    /// derive (today `captain_pin`); `None` = not derivable, so the input is
    /// named in `questions` instead of silently defaulting to nobody.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_pin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// swarm | loop | open (| none). Never `join`: joining the live fleet is
    /// a swarm option, see `join`.
    pub topology: String,
    /// Phase 4 topologies: `join` is an option of `Swarm`, not a fourth
    /// topology. True = arrive on the live fleet instead of spawning a second
    /// swarm (`workflow` is `join`, argv is `flywheel arrive`). Omitted from
    /// the JSON when false, so every non-join briefing is byte-identical to
    /// what it was.
    #[serde(default, skip_serializing_if = "is_false")]
    pub join: bool,
    pub workflow: String,
    pub token: Option<String>,
    pub stages: Vec<String>,
    pub crew: BTreeMap<String, u32>,
    pub crew_total: u32,
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub why: String,
    pub argv: Vec<String>,
    /// bd-2o1v P0-5 (a): profile affinity of the proposed row, READ from
    /// the workflow's steps by the caller (never re-declared here).
    pub role: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The crew-confirm gate's predicate (bd-2o1v): does committing this proposal
/// spawn panes? `open`/`none` spawn nothing, and neither does `join` (a swarm
/// option that arrives on the live fleet), so those stay outside the gate.
pub fn spawns_crew(p: &Proposal) -> bool {
    !p.join && !matches!(p.topology.as_str(), "open" | "none" | "")
}

/// A topology word as the operator may type it, as (topology, join). `join`
/// is accepted as an alias for `swarm` with the join option, so existing
/// `--topology join`, `/join` and stored `session_topology=join` keep working.
pub fn split_topology(t: &str) -> (&str, bool) {
    if t == "join" { ("swarm", true) } else { (t, false) }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alternative {
    pub topology: String,
    pub workflow: Option<String>,
    pub when: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub options: Vec<String>,
    pub default: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Briefing {
    pub schema: String,
    pub objective: String,
    pub world: World,
    pub proposal: Proposal,
    pub alternatives: Vec<Alternative>,
    pub questions: Vec<Question>,
    /// bd-drlm: the tier-aware harness menu for this crew, DERIVED from the
    /// resolved profile catalog. Additive and optional: a briefing that could
    /// not load a catalog still renders, it just carries no menu, because a
    /// missing menu must never be a reason a captain cannot see the plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_menu: Option<crate::crew_menu::HarnessMenu>,
    /// bd-xaw8: installed-binary staleness. None = unmeasured (renders
    /// absent, never a false claim); Some(true) also warns on stderr via
    /// apply_staleness. Additive and optional like the menu above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_build: Option<bool>,
    /// bd-drlm: what the Jev pass actually did to the menu above, in one
    /// operator-readable line. Additive and optional for the same reason the menu
    /// is.
    ///
    /// It exists so a ranked menu is never a MYSTERY. Without it an operator
    /// cannot tell "Jev ranked this" from "the deterministic order happened to
    /// look considered", which is the exact ambiguity a ranking feature creates
    /// and the reason it needs an audit trail. `None` means no pass was made
    /// (unconfigured, off, or a single-option menu) — a different thing from a
    /// pass that was made and declined, and the line says which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_ranking: Option<String>,
}

fn held_summary(paths: &[String]) -> String {
    match paths {
        [] => "none".into(),
        [one] => one.clone(),
        [first, ..] => format!("{} ({}…)", paths.len(), first),
    }
}

/// Loud join banner (ADR-0011 D5). Never silent. Held is a count, not a dump.
pub fn join_banner(world: &World) -> String {
    format!(
        "JOINING — not spawning  project={} pins={} dispatcher={} coalition={} held={}",
        world.project,
        world.pin_count,
        world.dispatcher,
        world.coalition_id.as_deref().unwrap_or("-"),
        held_summary(&world.held_paths)
    )
}

pub fn is_compose_token(primitive: &str) -> bool {
    matches!(
        primitive,
        "wave-swarm" | "repair-until-verified" | "beads-lifecycle"
    )
}

/// Topology comes from decide's one routing table (ADR-0032 Spec 9): the
/// only briefing-local rule is the joinable-world override. The old private
/// primitive→topology map is deleted (it diverged on babysit-bead).
/// Returns (topology, join): `join` is a Swarm option, never a topology.
fn topology_for(decision: &Decision, world: &World) -> (String, bool) {
    if world.joinable {
        return ("swarm".into(), true);
    }
    let (t, join) = split_topology(decision.topology);
    (t.to_string(), join)
}

fn default_n(unique: u32, cap: u32) -> u32 {
    unique.max(1).min(cap)
}

/// Inputs a routed workflow cannot default. A run without them fails at render.
pub fn required_inputs(primitive: &str) -> &'static [&'static str] {
    match primitive {
        "bug-fix" => &["symptom", "repro", "verify"],
        "repair-until-verified" => &["verify"],
        "hillclimb" => &["metric", "target"],
        "perf" => &["metric", "baseline_cmd", "budget"],
        "prototype" | "arena" => &["question"],
        "forensics" => &["symptom"],
        _ => &[],
    }
}

/// `name=value` or `name: value` in the objective. The goal prose is not a value.
pub fn missing_required_inputs(primitive: &str, objective: &str) -> Vec<&'static str> {
    let lower = objective.to_ascii_lowercase();
    required_inputs(primitive)
        .iter()
        .copied()
        .filter(|name| {
            let name = name.to_ascii_lowercase();
            !lower.contains(&format!("{name}=")) && !lower.contains(&format!("{name}:"))
        })
        .collect()
}

/// A harness named as its own word. `oc` and `gk` are the short aliases.
/// A harness named as its own word. Every canonical name and every alias in
/// `harness_registry` is findable here, which is what lets briefing pin a
/// harness the registry knows. bd-drlm: the word list USED to be a fourth
/// private table that knew neither `codex`, `claude` nor `fx`.
pub fn named_harness(objective: &str) -> Option<&'static str> {
    let lower = objective.to_ascii_lowercase();
    for word in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        // No longest-first pass is needed: a word is already split, so `oc`
        // can never shadow `opencode`. Resolution is exact, and fenced as such.
        if let Some(spec) = crate::harness_registry::spec(word) {
            return Some(spec.canonical);
        }
    }
    None
}

fn splice_named_harness(argv: &mut Vec<String>, objective: &str) {
    let Some(kind) = named_harness(objective) else {
        return;
    };
    if argv
        .windows(2)
        .any(|w| w[0] == "workflow" && w[1] == "compose")
    {
        if !argv.iter().any(|a| a == "--harness") {
            argv.push("--harness".into());
            argv.push(kind.into());
        }
    } else if argv.windows(2).any(|w| w[0] == "workflow" && w[1] == "run")
        && !argv.iter().any(|a| a == "--role-harness")
    {
        argv.push("--role-harness".into());
        argv.push(format!("coder={kind}"));
    }
}

/// bd-compose-plumbing-zdzi: derive the declared inputs of a compose-token run
/// that this process can bind itself, and MERGE them into the emitted argv as
/// `--inputs`. Today the derivable pair is `captain_pin` = the arriving pin:
/// the agent being briefed IS the captain, and the recipe default `""` would
/// aim every stage escalation at nobody. An operator-provided value already on
/// the argv is never overwritten. Returns the declared names it could NOT
/// derive, so the caller names them in `questions`.
pub fn bind_declared_inputs(
    argv: &mut Vec<String>,
    token: &str,
    agent_pin: Option<&str>,
) -> Vec<&'static str> {
    let Some(wf) = crate::builtin_workflows::builtin_recipe(token) else {
        return Vec::new();
    };
    let mut underivable = Vec::new();
    let mut bindings: Vec<(&'static str, String)> = Vec::new();
    for name in ["captain_pin"] {
        if !wf.inputs.iter().any(|i| i.name == name) {
            continue;
        }
        match agent_pin.map(str::trim).filter(|p| !p.is_empty()) {
            Some(pin) => bindings.push((name, pin.to_string())),
            None => underivable.push(name),
        }
    }
    if !bindings.is_empty() {
        merge_inputs_into_argv(argv, &bindings);
    }
    underivable
}

/// Merge `(name, value)` bindings into argv's `--inputs` JSON object —
/// creating the flag when absent, extending it when present, and respecting
/// a non-empty operator value already on the argv.
fn merge_inputs_into_argv(argv: &mut Vec<String>, bindings: &[(&str, String)]) {
    let existing = argv
        .iter()
        .position(|a| a == "--inputs")
        .and_then(|i| argv.get(i + 1).cloned())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut merged = existing;
    for (name, value) in bindings {
        let already = merged
            .get(*name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !already {
            merged.insert((*name).to_string(), serde_json::json!(value));
        }
    }
    let rendered = serde_json::Value::Object(merged).to_string();
    match argv.iter().position(|a| a == "--inputs") {
        Some(i) if i + 1 < argv.len() => argv[i + 1] = rendered,
        _ => {
            argv.push("--inputs".into());
            argv.push(rendered);
        }
    }
}

fn has_diagnose_then_fix(objective: &str) -> bool {
    let lower = objective.to_ascii_lowercase();
    let diagnose = lower.contains("diagnose") || lower.contains("forensic");
    let fix = lower.contains("fix") || lower.contains("implement");
    diagnose && fix
}

/// Assemble a briefing. `unique_roles` is the packed-from inventory (may be empty).
impl Briefing {
    /// Compute installed-binary staleness, set the field, warn loudly when
    /// stale. IO lives here (caller side) so assemble stays pure.
    pub fn apply_staleness(&mut self) {
        match crate::stale_check::freshness() {
            crate::stale_check::Freshness::Stale => {
                self.stale_build = Some(true);
                eprintln!(
                    "flywheel: installed binary predates its sources — rebuild and reinstall; briefing fields may be missing"
                );
            }
            crate::stale_check::Freshness::Fresh => {
                self.stale_build = Some(false);
            }
            crate::stale_check::Freshness::Unknown => {}
        }
    }
}

pub fn assemble(
    objective: &str,
    world: &World,
    unique_roles: &[String],
    cap: u32,
    catalog: Option<&crate::profile::ProfileCatalog>,
) -> Briefing {
    let cap = if cap == 0 { DEFAULT_CREW_MAX } else { cap };
    let snap = decide::Snapshot {
        ready_bead_count: world.ready_beads,
        profiled_panes: world.pin_count,
        launch_pending: world.launch_pending,
        contention: !world.held_paths.is_empty(),
    };
    let decision = decide::decide(objective, &snap);
    let (topology, join) = topology_for(&decision, world);
    let unique_owned: Vec<String> = if unique_roles.is_empty() && topology == "swarm" && !join {
        WAVE_SWARM_ROLES.iter().map(|s| (*s).to_string()).collect()
    } else {
        unique_roles.to_vec()
    };
    let unique_roles = unique_owned.as_slice();
    let n = decide::parse_budget_n(objective)
        .unwrap_or_else(|| default_n(unique_roles.len() as u32, cap));
    let packed = crew::pack_crew(unique_roles, n, cap).unwrap_or_else(|_| crew::PackedCrew {
        members: BTreeMap::new(),
        aliases: BTreeMap::new(),
        budget: n,
        unique: unique_roles.len() as u32,
        cap,
    });
    // bd-drlm: the menu is built from the same `n` the crew is packed to, so
    // "varied by number of crews" is a function of the real budget rather than
    // a separate guess.
    let harness_menu = catalog.map(|c| crate::crew_menu::harness_menu(c, n));
    let mut remove = Vec::new();
    for (from, to) in &packed.aliases {
        remove.push(format!("{from} → {to} (packed under N={n})"));
    }
    let add: Vec<String> = packed
        .members
        .iter()
        .filter(|(_, c)| **c > 1)
        .map(|(role, c)| format!("{role} extra {} (budget fill)", c - 1))
        .collect();

    let mut argv = decision.argv.clone();
    if join {
        argv = vec!["flywheel".into(), "arrive".into(), world.project.clone()];
        if let Some(id) = &world.coalition_id {
            argv.extend(["--coalition".into(), id.clone()]);
        }
    } else if is_compose_token(decision.primitive) && !argv.iter().any(|a| a == "--crew") {
        argv.push("--crew".into());
        argv.push(n.to_string());
    }
    // workflow run/compose preflight requires a project. The classifier
    // builds argv without one, so the printed command died on "no project".
    splice_project(&mut argv, &world.project);
    splice_named_harness(&mut argv, objective);

    let mut missing = missing_required_inputs(decision.primitive, objective).to_vec();
    // bd-compose-plumbing-zdzi: a compose token's declared inputs that this
    // process can derive are bound into the argv it emits (`--inputs`); the
    // underivable ones join `missing` below — a run is never emitted with a
    // silently empty escalation target. Only a real compose argv binds (a
    // join/arrive argv carries no `{{inputs.*}}` to resolve).
    let binds_compose = argv
        .windows(2)
        .any(|w| w[0] == "workflow" && w[1] == "compose");
    if binds_compose && is_compose_token(decision.primitive) {
        missing.extend(bind_declared_inputs(
            &mut argv,
            decision.primitive,
            world.agent_pin.as_deref(),
        ));
    }
    let why = if join {
        format!(
            "live fleet on {} (pins={}, dispatcher={}) — join by default (ADR-0011 D5)",
            world.project, world.pin_count, world.dispatcher
        )
    } else {
        decision.reason.clone()
    };
    let why = if missing.is_empty() || argv.is_empty() {
        why
    } else {
        argv.clear();
        format!(
            "{why} Missing inputs: {}. Re-brief with name=value for each. The run is not dispatched.",
            missing.join(", ")
        )
    };

    let token = is_compose_token(decision.primitive).then(|| decision.primitive.to_string());

    // governed-by: bd-7lms — every decide family gets a visible alternative
    // so no stack scenario (refactor, cron, coalition, debate, forensics,
    // steer, joinable) falls off the briefing surface silently.
    let mut alternatives = vec![
        Alternative {
            topology: "loop".into(),
            workflow: Some("loop-work".into()),
            when: "ready beads exist and you want solo durable dispatch".into(),
        },
        Alternative {
            topology: "open".into(),
            workflow: None,
            when: "current pane, no spawn".into(),
        },
        Alternative {
            topology: "swarm".into(),
            workflow: Some("wave-swarm".into()),
            when: "multi-lane and no live fleet to join".into(),
        },
    ];
    if world.joinable && !alternatives.iter().any(|a| a.workflow.as_deref() == Some("join")) {
        alternatives.push(Alternative {
            topology: "swarm".into(),
            workflow: Some("join".into()),
            when: "live fleet — join instead of spawning".into(),
        });
    }
    if has_diagnose_then_fix(objective)
        && matches!(decision.primitive, "bug-fix" | "repair-until-verified")
    {
        alternatives.push(Alternative {
            topology: "open".into(),
            workflow: Some("forensics".into()),
            when: "diagnosis first, then a second briefing for the fix".into(),
        });
    }
    if !matches!(decision.primitive, "loop-work" | "none" | "wave-swarm")
        && !alternatives
            .iter()
            .any(|a| a.workflow.as_deref() == Some(decision.primitive))
    {
        alternatives.push(Alternative {
            topology: topology.clone(),
            workflow: Some(decision.primitive.to_string()),
            when: decision.reason.clone(),
        });
    }

    let mut briefing = Briefing {
        schema: SCHEMA.into(),
        objective: objective.into(),
        world: world.clone(),
        proposal: Proposal {
            topology: topology.clone(),
            join,
            workflow: decision.primitive.to_string(),
            token,
            stages: Vec::new(),
            crew: packed.members.clone(),
            crew_total: packed.total(),
            add,
            remove,
            why,
            argv,
            role: None,
        },
        alternatives,
        harness_menu,
        stale_build: None,
        // bd-drlm: `assemble` is PURE and stays that way — no network, no clock.
        // The Jev pass runs in the CALLER (cmd_briefing) and writes this line, so
        // the ranking cannot leak into the pure core that derives the menu.
        harness_ranking: None,
        questions: vec![
            Question {
                id: "topology".into(),
                options: vec!["swarm".into(), "loop".into(), "open".into()],
                default: topology,
            },
            Question {
                id: "crew".into(),
                options: vec![
                    "1".into(),
                    "2".into(),
                    "3".into(),
                    "4".into(),
                    "5".into(),
                    "planner:1,coder:1,reviewer:1".into(),
                ],
                default: n.to_string(),
            },
            Question {
                id: "model".into(),
                options: vec![
                    "default".into(),
                    "@cheap".into(),
                    "@standard".into(),
                    "@premium".into(),
                ],
                default: "default".into(),
            },
        ],
    };
    if !missing.is_empty() {
        briefing.questions.push(Question {
            id: "inputs".into(),
            options: missing.iter().map(|name| (*name).to_string()).collect(),
            default: missing.join(","),
        });
    }
    briefing
}

/// GS-049: after sticky-open `[reclassify]` announce, briefing must not present
/// a conflicting second topology surface (dual SoT). Align proposal + question
/// default with sticky `session_topology`; when sticky-held against any invent
/// flip (open×loop|swarm|join|none, loop×swarm), clear invent workflow and
/// rewrite argv so `--commit` cannot exec invent swarm/join/loop under an
/// open/loop label (crew-confirm is skipped for open — invent headless
/// `workflow run wave-swarm` would auto-dispatch).
pub fn align_with_session_topology(b: &mut Briefing, session: &str, sticky_held: bool) {
    let session = session.trim();
    if session.is_empty() {
        return;
    }
    // `join` is a swarm option: compare and set (topology, join) pairs.
    let (session_topology, session_join) = split_topology(session);
    if b.proposal.topology == session_topology && b.proposal.join == session_join {
        if sticky_held {
            // Airtight print: never leave invent swarm primitive in why under open/loop hold.
            if b.proposal.why.contains("wave-swarm")
                || b.proposal.why.contains("repair-until-verified")
                || b.proposal.why.contains("beads-lifecycle")
                || !b.proposal.why.contains("sticky")
            {
                b.proposal.why =
                    format!("session_topology={session} sticky-held — invent vehicle suppressed");
            }
            rewrite_sticky_held_argv(b, session);
        }
        return;
    }
    let classified = if b.proposal.join {
        "join".to_string()
    } else {
        b.proposal.topology.clone()
    };
    b.proposal.topology = session_topology.to_string();
    b.proposal.join = session_join;
    if let Some(q) = b.questions.iter_mut().find(|q| q.id == "topology") {
        q.default = session_topology.to_string();
    }
    let prior_why = if sticky_held {
        // Do not echo invent swarm/join/loop primitive names into the print surface.
        format!("suppressed invent topology={classified}")
    } else {
        b.proposal.why.clone()
    };
    b.proposal.why = format!(
        "session_topology={session}{} — aligned ({prior_why})",
        if sticky_held { " sticky-held" } else { "" },
    );
    // ALL sticky-held cases — not only classified∈{swarm,join}.
    if sticky_held {
        rewrite_sticky_held_argv(b, session);
    }
}

/// Invent spawn / compose / join / loop vehicles suppressed under sticky-open.
fn is_invent_spawn_under_open(workflow: &str) -> bool {
    let w = workflow.trim();
    w.is_empty() || w == "none" || w == "join" || w == "loop-work" || is_compose_token(w)
}

/// Rewrite commit argv to the held session vehicle; clear invent swarm surfaces.
///
/// Sticky-open airtight: never keep invent `wave-swarm`/`join`/`loop-work`
/// workflow or `workflow run <invent> --mode headless` argv under an open
/// label — crew-confirm is skipped for open, so that would invent-green spawn.
/// Fail-closed: empty argv (commit bails) + workflow cleared to `none`.
fn rewrite_sticky_held_argv(b: &mut Briefing, session: &str) {
    match session {
        "open" => {
            // Every sticky-held open path is cleared. A pane workflow such as
            // hillclimb is an invent spawn the same way a compose token is.
            // An input refusal may already have emptied argv. That is still
            // an invent vehicle and must not panic the door.
            debug_assert!(
                is_invent_spawn_under_open(&b.proposal.workflow) || !b.proposal.workflow.is_empty(),
                "sticky-held open expected a classified vehicle, got workflow={} argv={:?}",
                b.proposal.workflow,
                b.proposal.argv
            );
            let missing = b
                .questions
                .iter()
                .find(|q| q.id == "inputs")
                .map(|q| q.default.clone());
            b.proposal.workflow = "none".into();
            b.proposal.token = None;
            b.proposal.argv.clear();
            b.proposal.crew.clear();
            b.proposal.crew_total = 0;
            b.proposal.add.clear();
            b.proposal.remove.clear();
            if let Some(names) = missing {
                b.proposal
                    .why
                    .push_str(&format!(" Missing inputs: {names}."));
            }
        }
        "loop" => {
            // Hold loop against invent swarm: loop vehicle only (no wave-swarm).
            b.proposal.workflow = "loop-work".into();
            b.proposal.token = None;
            b.proposal.argv = vec![
                "flywheel".into(),
                "loop-mode".into(),
                "run".into(),
                b.world.project.clone(),
                "--work".into(),
            ];
            b.proposal.crew.clear();
            b.proposal.crew_total = 0;
            b.proposal.add.clear();
            b.proposal.remove.clear();
        }
        _ => {}
    }
}

/// Operator remix of the printed questions. Explicit topology is an
/// AUTHORITATIVE request (sticky-open may flip). `crew` is `N` or `role:n`.
/// `model` is `default` or an `@slot`. Unknown values fail loud.
pub fn apply_answers(
    b: &mut Briefing,
    topology: Option<&str>,
    crew: Option<&str>,
    model: Option<&str>,
) -> Result<(), String> {
    if let Some(t) = topology {
        let t = t.trim();
        if !matches!(t, "swarm" | "loop" | "open" | "join") {
            return Err(format!(
                "briefing topology {t:?} is not swarm|loop|open (join = swarm joining the live fleet)"
            ));
        }
        let (topo, join) = split_topology(t);
        if topo != b.proposal.topology || join != b.proposal.join {
            rebuild_argv_for_topology(b, t);
        }
        if let Some(q) = b.questions.iter_mut().find(|q| q.id == "topology") {
            q.default = topo.to_string();
        }
    }
    if let Some(c) = crew {
        let c = c.trim();
        if c.is_empty() {
            return Err("briefing crew must be N or role:n,role:n".into());
        }
        if !b.proposal.argv.is_empty() && !b.proposal.join {
            splice_flag(&mut b.proposal.argv, "--crew", c);
        }
        if let Some(q) = b.questions.iter_mut().find(|q| q.id == "crew") {
            q.default = c.to_string();
        }
        if let Ok(n) = c.parse::<u32>() {
            b.proposal.crew_total = n;
        }
    }
    if let Some(m) = model {
        let m = m.trim();
        if !matches!(m, "default" | "@cheap" | "@standard" | "@premium") {
            return Err(format!(
                "briefing model {m:?} is not default|@cheap|@standard|@premium"
            ));
        }
        if m != "default" && !b.proposal.argv.is_empty() && b.proposal.topology == "swarm" && !b.proposal.join {
            let roles: Vec<String> = if b.proposal.crew.is_empty() {
                WAVE_SWARM_ROLES.iter().map(|s| (*s).to_string()).collect()
            } else {
                b.proposal.crew.keys().cloned().collect()
            };
            let specs: Vec<String> = roles.iter().map(|r| format!("{r}={m}")).collect();
            splice_role_models(&mut b.proposal.argv, &specs);
        }
        if let Some(q) = b.questions.iter_mut().find(|q| q.id == "model") {
            q.default = m.to_string();
        }
    }
    Ok(())
}

fn splice_project(argv: &mut Vec<String>, project: &str) {
    if project.is_empty() || argv.iter().any(|a| a == "--project") {
        return;
    }
    let workflow = argv
        .windows(2)
        .any(|w| w[0] == "workflow" && (w[1] == "run" || w[1] == "compose"));
    if workflow {
        argv.push("--project".into());
        argv.push(project.to_string());
    }
}

fn rebuild_argv_for_topology(b: &mut Briefing, topology: &str) {
    let (topo, join) = split_topology(topology);
    b.proposal.topology = topo.to_string();
    b.proposal.join = join;
    match topology {
        "open" => {
            b.proposal.workflow = "none".into();
            b.proposal.token = None;
            b.proposal.argv.clear();
            b.proposal.crew.clear();
            b.proposal.crew_total = 0;
            b.proposal.add.clear();
            b.proposal.remove.clear();
            b.proposal.why = "none: raw (operator topology=open)".into();
        }
        "loop" => {
            b.proposal.workflow = "loop-work".into();
            b.proposal.token = None;
            b.proposal.argv = vec![
                "flywheel".into(),
                "loop-mode".into(),
                "run".into(),
                b.world.project.clone(),
                "--work".into(),
            ];
            b.proposal.crew.clear();
            b.proposal.crew_total = 0;
            b.proposal.why = "loop-work (operator topology=loop)".into();
        }
        "join" => {
            b.proposal.workflow = "join".into();
            b.proposal.token = None;
            let mut argv = vec!["flywheel".into(), "arrive".into(), b.world.project.clone()];
            if let Some(id) = &b.world.coalition_id {
                argv.extend(["--coalition".into(), id.clone()]);
            }
            b.proposal.argv = argv;
            b.proposal.why = "join (operator topology=join)".into();
        }
        "swarm" => {
            b.proposal.workflow = "wave-swarm".into();
            b.proposal.token = Some("wave-swarm".into());
            let n = if b.proposal.crew_total > 0 {
                b.proposal.crew_total
            } else {
                WAVE_SWARM_ROLES.len() as u32
            };
            b.proposal.crew_total = n;
            if b.proposal.crew.is_empty() {
                for role in WAVE_SWARM_ROLES {
                    b.proposal.crew.insert((*role).into(), 1);
                }
            }
            let mut argv = vec![
                "flywheel".into(),
                "workflow".into(),
                "compose".into(),
                b.objective.clone(),
                "--parts".into(),
                "wave-swarm".into(),
                "--run".into(),
                "--project".into(),
                b.world.project.clone(),
                "--crew".into(),
                n.to_string(),
            ];
            // bd-compose-plumbing-zdzi: the same declared-input binding the
            // classify path applies — an explicit swarm flip must not drop the
            // captain pin the token needs. Underivable = the explicit choice
            // still dispatches, with the input surfaced as a question below.
            let underivable =
                bind_declared_inputs(&mut argv, "wave-swarm", b.world.agent_pin.as_deref());
            b.proposal.argv = argv;
            b.proposal.why = "wave-swarm (operator topology=swarm)".into();
            if !underivable.is_empty() {
                // Explicit operator choice: the body still dispatches, but the
                // underivable input is surfaced the same way the classify path
                // surfaces it — a named question, never a silent "".
                b.proposal.why = format!(
                    "wave-swarm (operator topology=swarm) — Missing inputs: {}. Pass them via \
                     --inputs before --commit (captain_pin targets escalations).",
                    underivable.join(", ")
                );
                b.questions.push(Question {
                    id: "inputs".into(),
                    options: underivable.iter().map(|n| (*n).to_string()).collect(),
                    default: underivable.join(","),
                });
            }
        }
        _ => {}
    }
}

fn splice_flag(argv: &mut Vec<String>, flag: &str, value: &str) {
    if let Some(i) = argv.iter().position(|a| a == flag) {
        if i + 1 < argv.len() && !argv[i + 1].starts_with('-') {
            argv[i + 1] = value.to_string();
        } else {
            argv.insert(i + 1, value.to_string());
        }
    } else {
        argv.push(flag.to_string());
        argv.push(value.to_string());
    }
}

fn splice_role_models(argv: &mut Vec<String>, specs: &[String]) {
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--role-model" {
            argv.remove(i);
            if i < argv.len() {
                argv.remove(i);
            }
        } else {
            i += 1;
        }
    }
    for spec in specs {
        argv.push("--role-model".into());
        argv.push(spec.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd-compose-plumbing-zdzi: an empty FLEET, not an anonymous caller —
    /// the fixture carries the arriving pin the way the CLI resolves it, so a
    /// wave-swarm briefing in these tests emits a dispatchable argv. Tests
    /// that need the underivable case build `World { agent_pin: None, .. }`.
    fn world_empty() -> World {
        World {
            project: "toron".into(),
            dispatcher: "absent".into(),
            agent_pin: Some("captain".into()),
            ..World::default()
        }
    }

    fn joinable() -> World {
        World {
            project: "toron".into(),
            dispatcher: "up".into(),
            joinable: true,
            pin_count: 3,
            coalition_id: Some("coal-1".into()),
            ..World::default()
        }
    }

    fn solo() -> World {
        World {
            project: "toron".into(),
            dispatcher: "absent".into(),
            ..World::default()
        }
    }

    #[test]
    fn join_is_a_swarm_option_and_no_briefing_names_a_join_topology() {
        for (world, objective) in [
            (joinable(), "wave-swarm 3 crews"),
            (joinable(), "join the fleet"),
            (joinable(), "fix the login bug"),
            (solo(), "join the fleet"),
            (solo(), "wave-swarm 3 crews"),
            (solo(), "fix the login bug"),
        ] {
            let b = assemble(objective, &world, &["coder".into()], 5, None);
            let v = serde_json::to_value(&b).unwrap();
            assert_ne!(b.proposal.topology, "join", "{objective}");
            assert!(b.alternatives.iter().all(|a| a.topology != "join"), "{objective}");
            let q = b.questions.iter().find(|q| q.id == "topology").unwrap();
            assert!(!q.options.iter().any(|o| o == "join"), "{objective}: {:?}", q.options);
            assert_ne!(q.default, "join", "{objective}");
            if !world.joinable && !b.proposal.join {
                assert!(
                    b.alternatives.iter().all(|a| a.workflow.as_deref() != Some("join")),
                    "{objective}: no join alternative without a live fleet"
                );
            }
            // The join option is visible only when it is on.
            assert_eq!(v["proposal"].get("join").is_some(), b.proposal.join, "{objective}");
            if b.proposal.join {
                assert_eq!(b.proposal.topology, "swarm");
                if let Some(a) = b.proposal.argv.get(1) {
                    assert_eq!(a, "arrive", "{objective}");
                }
            }
        }
        // A joinable world offers the join workflow as a swarm alternative.
        let b = assemble("fix the login bug", &joinable(), &[], 5, None);
        assert!(b.alternatives.iter().any(|a| a.topology == "swarm" && a.workflow.as_deref() == Some("join")));
    }

    #[test]
    fn join_does_not_default_the_wave_swarm_roles_into_the_crew() {
        // A swarm with no named roles defaults to the wave-swarm roles; a join
        // arrives on an existing fleet and must not pick them up.
        let j = assemble("wave-swarm 3 crews", &joinable(), &[], 5, None);
        let s = assemble("wave-swarm 3 crews", &solo(), &[], 5, None);
        assert!(j.proposal.join && !s.proposal.join);
        assert!(!s.proposal.crew.is_empty(), "precondition: a swarm gets default roles");
        assert_ne!(j.proposal.crew, s.proposal.crew, "join: {:?}", j.proposal.crew);
    }

    #[test]
    fn a_non_join_briefing_serialises_without_a_join_key() {
        let b = assemble("wave-swarm 3 crews", &solo(), &["coder".into()], 5, None);
        assert!(!b.proposal.join);
        let v = serde_json::to_value(&b).unwrap();
        assert!(v["proposal"].get("join").is_none(), "byte-identical for non-join: {v}");
        // And an old briefing JSON without the key still reads back (join=false).
        let back: Briefing = serde_json::from_value(v).unwrap();
        assert!(!back.proposal.join);
    }

    #[test]
    fn the_crew_confirm_gate_ignores_join_exactly_as_the_old_join_topology_did() {
        let p = |t: &str, join: bool| {
            let mut b = assemble("wave-swarm 3 crews", &solo(), &["coder".into()], 5, None);
            b.proposal.topology = t.into();
            b.proposal.join = join;
            b.proposal
        };
        assert!(spawns_crew(&p("swarm", false)));
        assert!(spawns_crew(&p("loop", false)));
        assert!(!spawns_crew(&p("swarm", true)), "join spawns no pane");
        assert!(!spawns_crew(&p("open", false)));
        assert!(!spawns_crew(&p("none", false)));
        assert!(!spawns_crew(&p("", false)));
        let joined = assemble("wave-swarm 3 crews", &joinable(), &["coder".into()], 5, None);
        assert!(!spawns_crew(&joined.proposal), "a live fleet is joined without the gate");
    }

    #[test]
    fn the_join_alias_still_remixes_and_swarm_clears_it() {
        let mut b = assemble("wave-swarm 3 crews", &solo(), &["coder".into()], 5, None);
        apply_answers(&mut b, Some("join"), None, None).unwrap();
        assert_eq!((b.proposal.topology.as_str(), b.proposal.join), ("swarm", true));
        assert_eq!(b.proposal.workflow, "join");
        assert_eq!(b.proposal.argv[1], "arrive");
        let q = b.questions.iter().find(|q| q.id == "topology").unwrap();
        assert_eq!(q.default, "swarm");
        // crew is not spliced into an arrive argv; a model slot is not either.
        apply_answers(&mut b, None, Some("3"), Some("@cheap")).unwrap();
        assert!(!b.proposal.argv.iter().any(|a| a == "--crew" || a.contains("=@cheap")));
        // Choosing swarm explicitly leaves the join option.
        apply_answers(&mut b, Some("swarm"), None, None).unwrap();
        assert_eq!((b.proposal.topology.as_str(), b.proposal.join), ("swarm", false));
        assert_eq!(b.proposal.workflow, "wave-swarm");
        assert_ne!(b.proposal.argv.get(1).map(String::as_str), Some("arrive"));
        // open and loop clear it too.
        apply_answers(&mut b, Some("join"), None, None).unwrap();
        apply_answers(&mut b, Some("open"), None, None).unwrap();
        assert!(!b.proposal.join && b.proposal.topology == "open");
        apply_answers(&mut b, Some("join"), None, None).unwrap();
        apply_answers(&mut b, Some("loop"), None, None).unwrap();
        assert!(!b.proposal.join && b.proposal.topology == "loop");
        // Re-asking for what is already there changes nothing.
        let mut j = assemble("wave-swarm 3 crews", &joinable(), &["coder".into()], 5, None);
        let before = j.proposal.clone();
        apply_answers(&mut j, Some("join"), None, None).unwrap();
        assert_eq!(j.proposal, before);
        assert!(apply_answers(&mut j, Some("joined"), None, None).is_err());
    }

    #[test]
    fn a_stored_session_topology_of_join_aligns_as_swarm_with_the_option() {
        let mut b = assemble("wave-swarm 3 crews", &solo(), &["coder".into()], 5, None);
        align_with_session_topology(&mut b, "join", false);
        assert_eq!((b.proposal.topology.as_str(), b.proposal.join), ("swarm", true));
        let q = b.questions.iter().find(|q| q.id == "topology").unwrap();
        assert_eq!(q.default, "swarm");
        // Already aligned: nothing to do.
        let before = b.proposal.clone();
        align_with_session_topology(&mut b, "join", false);
        assert_eq!(b.proposal, before);
        // A swarm session over a join proposal drops the option (the old label flip).
        let mut j = assemble("wave-swarm 3 crews", &joinable(), &["coder".into()], 5, None);
        align_with_session_topology(&mut j, "swarm", false);
        assert_eq!((j.proposal.topology.as_str(), j.proposal.join), ("swarm", false));
    }

    #[test]
    fn joinable_world_proposes_join_not_compose() {
        let world = World {
            project: "toron".into(),
            dispatcher: "up".into(),
            joinable: true,
            pin_count: 3,
            coalition_id: Some("coal-1".into()),
            ..World::default()
        };
        let b = assemble("wave-swarm 3 crews", &world, &["coder".into()], 5, None);
        assert_eq!(b.schema, SCHEMA);
        assert_eq!(b.proposal.topology, "swarm", "join is a swarm option, not a topology");
        assert!(b.proposal.join);
        assert_eq!(b.proposal.argv[1], "arrive");
        assert!(b.proposal.why.contains("JOINING") || b.proposal.why.contains("join"));
        let banner = join_banner(&world);
        assert!(banner.contains("JOINING — not spawning"), "{banner}");
        assert!(banner.contains("pins=3"), "{banner}");
    }

    #[test]
    fn join_banner_summarizes_held_paths() {
        let world = World {
            project: "toron".into(),
            dispatcher: "up".into(),
            joinable: true,
            pin_count: 1,
            held_paths: (0..10).map(|i| format!("p{i}")).collect(),
            ..World::default()
        };
        let banner = join_banner(&world);
        assert!(banner.contains("held=10 (p0…)"), "{banner}");
        assert!(!banner.contains("p9"), "{banner}");
    }

    #[test]
    fn empty_world_wave_swarm_emits_crew_n() {
        let roles = vec![
            "planner".into(),
            "coder".into(),
            "scout".into(),
            "reviewer".into(),
            "housekeeper".into(),
        ];
        let b = assemble("wave-swarm the auth lanes", &world_empty(), &roles, 5, None);
        assert_eq!(b.proposal.topology, "swarm");
        assert_eq!(b.proposal.workflow, "wave-swarm");
        assert!(b.proposal.argv.iter().any(|a| a == "--crew"));
        assert_eq!(b.proposal.crew_total, 5);
        assert_eq!(
            b.questions
                .iter()
                .find(|q| q.id == "crew")
                .map(|q| q.default.as_str()),
            Some("5")
        );
    }

    #[test]
    fn ready_beads_classifies_loop() {
        let world = World {
            project: "toron".into(),
            ready_beads: 4,
            dispatcher: "absent".into(),
            ..World::default()
        };
        let b = assemble("work the ready beads", &world, &["coder".into()], 5, None);
        assert_eq!(b.proposal.workflow, "loop-work");
        assert_eq!(b.proposal.topology, "loop");
    }

    #[test]
    fn two_crews_packs_n_2_not_default_5() {
        let b = assemble("wave-swarm with 2 crews", &world_empty(), &[], 5, None);
        assert_eq!(b.proposal.topology, "swarm");
        assert_eq!(b.proposal.workflow, "wave-swarm");
        assert_eq!(b.proposal.crew_total, 2);
        assert!(b.proposal.argv.windows(2).any(|w| w == ["--crew", "2"]));
    }

    #[test]
    fn empty_unique_on_swarm_defaults_wave_roles() {
        let b = assemble("wave-swarm the auth lanes", &world_empty(), &[], 5, None);
        assert_eq!(b.proposal.topology, "swarm");
        assert_eq!(b.proposal.crew_total, 3);
        assert!(b.proposal.argv.iter().any(|a| a == "--crew"));
        assert_eq!(
            b.proposal.crew.keys().cloned().collect::<Vec<_>>(),
            vec!["coder", "planner", "reviewer"]
        );
    }

    #[test]
    fn diagnose_with_leftover_ready_count_is_forensics_not_loop() {
        let world = World {
            project: "flywheel".into(),
            ready_beads: 2,
            dispatcher: "absent".into(),
            ..World::default()
        };
        let b = assemble(
            "diagnose flywheel x toron x beads friction: workflow use",
            &world,
            &[],
            5,
            None,
        );
        assert_eq!(b.proposal.workflow, "forensics");
        assert_eq!(b.proposal.topology, "open");
    }

    // ADR-0032 Spec 9 (bd-himk) RED: briefing reads decide's table — the
    // babysit-bead divergence (decide open vs briefing swarm) dies.
    #[test]
    fn decide_router_briefing_reads_decide_topology() {
        let world = World {
            project: "flywheel".into(),
            dispatcher: "absent".into(),
            ..World::default()
        };
        let b = assemble(
            "babysit the bd-xyz bead",
            &world,
            &["coder".into()],
            5,
            None,
        );
        let d = decide::decide("babysit the bd-xyz bead", &decide::Snapshot::default());
        assert_eq!(d.primitive, "babysit-bead");
        assert_eq!(b.proposal.topology, d.topology);
        assert_eq!(b.proposal.topology, "open");
    }

    #[test]
    fn sticky_held_suppresses_conflicting_swarm_proposal() {
        let world = world_empty();
        let mut b = assemble(
            "wave-swarm the auth lanes",
            &world,
            &["coder".into()],
            5,
            None,
        );
        assert_eq!(b.proposal.topology, "swarm");
        align_with_session_topology(&mut b, "open", true);
        assert_eq!(b.proposal.topology, "open");
        assert_eq!(
            b.questions
                .iter()
                .find(|q| q.id == "topology")
                .map(|q| q.default.as_str()),
            Some("open")
        );
        assert!(b.proposal.why.contains("sticky-held"), "{}", b.proposal.why);
        assert!(
            b.proposal.why.contains("suppressed invent"),
            "{}",
            b.proposal.why
        );
        assert!(
            !b.proposal.argv.iter().any(|a| a == "--crew"),
            "spawn crew argv suppressed: {:?}",
            b.proposal.argv
        );
        assert!(
            !b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["loop-mode", "run"]),
            "must not leave loop-mode under open: {:?}",
            b.proposal.argv
        );
        assert!(
            b.proposal.argv.is_empty(),
            "sticky_held open×swarm: argv must be empty (no invent workflow run): {:?}",
            b.proposal.argv
        );
        assert_eq!(
            b.proposal.workflow, "none",
            "proposal.workflow must clear invent wave-swarm on align"
        );
        assert!(b.proposal.token.is_none(), "compose token cleared");
        assert!(
            !b.proposal.argv.iter().any(|a| a == "wave-swarm"),
            "argv must not name invent wave-swarm: {:?}",
            b.proposal.argv
        );
        assert_eq!(b.proposal.crew_total, 0);
        assert!(b.proposal.crew.is_empty());
    }

    /// GS-049 adversarial: open held against loop must rewrite argv — else
    /// `--commit` skips crew-confirm (open label) and execs suppressed loop.
    #[test]
    fn sticky_held_open_x_loop_rewrites_argv_not_loop_mode() {
        let world = World {
            project: "toron".into(),
            ready_beads: 3,
            dispatcher: "absent".into(),
            ..World::default()
        };
        let mut b = assemble("work the ready beads", &world, &["coder".into()], 5, None);
        assert_eq!(b.proposal.topology, "loop", "precondition: loop classify");
        assert!(
            b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["loop-mode", "run"]),
            "precondition loop argv: {:?}",
            b.proposal.argv
        );
        align_with_session_topology(&mut b, "open", true);
        assert_eq!(b.proposal.topology, "open");
        assert!(
            !b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["loop-mode", "run"]),
            "open sticky-held must not keep loop-mode argv (commit would skip crew-confirm): {:?}",
            b.proposal.argv
        );
        assert!(
            b.proposal.argv.is_empty(),
            "sticky_held open×loop: fail-closed empty argv (no invent workflow run): {:?}",
            b.proposal.argv
        );
        assert_eq!(b.proposal.workflow, "none");
        assert!(
            !b.proposal.argv.windows(2).any(|w| w == ["workflow", "run"]),
            "must not invent workflow run under open: {:?}",
            b.proposal.argv
        );
        assert_eq!(b.proposal.crew_total, 0);
        assert!(b.proposal.crew.is_empty());
    }

    /// GS-049 adversarial: open held against none — argv must not invent a
    /// spawn vehicle; commit stays fail-closed.
    #[test]
    fn sticky_held_open_x_none_clears_spawn_argv() {
        let world = world_empty();
        let mut b = assemble("zzzz-unmatched-objective-qqq", &world, &[], 5, None);
        assert_eq!(b.proposal.topology, "none", "precondition: none classify");
        align_with_session_topology(&mut b, "open", true);
        assert_eq!(b.proposal.topology, "open");
        assert!(
            b.proposal.argv.is_empty(),
            "open×none sticky-held: fail-closed empty argv, got {:?}",
            b.proposal.argv
        );
        assert_eq!(b.proposal.workflow, "none");
        assert!(
            !b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["loop-mode", "run"]),
            "{:?}",
            b.proposal.argv
        );
        assert_eq!(b.proposal.crew_total, 0);
        assert!(b.proposal.why.contains("sticky-held"), "{}", b.proposal.why);
    }

    /// GS-049 airtight: sticky_held open×join → empty argv, no arrive/swarm.
    #[test]
    fn sticky_held_open_x_join_clears_invent_argv() {
        let world = World {
            project: "toron".into(),
            dispatcher: "up".into(),
            joinable: true,
            pin_count: 3,
            coalition_id: Some("coal-1".into()),
            ..World::default()
        };
        let mut b = assemble(
            "wave-swarm the auth lanes",
            &world,
            &["coder".into()],
            5,
            None,
        );
        assert!(b.proposal.join, "precondition: join");
        assert_eq!(b.proposal.argv[1], "arrive");
        align_with_session_topology(&mut b, "open", true);
        assert_eq!(b.proposal.topology, "open");
        assert!(!b.proposal.join, "an open session clears the join option");
        assert!(
            b.proposal.argv.is_empty(),
            "sticky_held open×join: empty argv, got {:?}",
            b.proposal.argv
        );
        assert_eq!(b.proposal.workflow, "none");
        assert!(
            !b.proposal
                .argv
                .iter()
                .any(|a| a == "arrive" || a == "wave-swarm"),
            "{:?}",
            b.proposal.argv
        );
        assert_eq!(b.proposal.crew_total, 0);
    }

    /// BAR sticky_open_airtight: sticky_held open × swarm|join → argv non-swarm/empty;
    /// print/align surface names no invent swarm primitive; --commit cannot
    /// spawn invent crew (empty argv → commit bail).
    #[test]
    fn sticky_open_airtight_open_x_swarm_and_join() {
        for (label, world) in [
            (
                "swarm",
                World {
                    project: "toron".into(),
                    dispatcher: "absent".into(),
                    ..World::default()
                },
            ),
            (
                "join",
                World {
                    project: "toron".into(),
                    dispatcher: "up".into(),
                    joinable: true,
                    pin_count: 2,
                    coalition_id: Some("c1".into()),
                    ..World::default()
                },
            ),
        ] {
            let mut b = assemble(
                "wave-swarm the auth lanes",
                &world,
                &["coder".into()],
                5,
                None,
            );
            assert_eq!(
                b.proposal.topology, "swarm",
                "{label}: precondition topology={}",
                b.proposal.topology
            );
            align_with_session_topology(&mut b, "open", true);
            assert_eq!(b.proposal.topology, "open", "{label}");
            assert!(!b.proposal.join, "{label}");
            assert!(
                b.proposal.argv.is_empty(),
                "{label}: argv must be empty for --commit fail-closed, got {:?}",
                b.proposal.argv
            );
            assert_eq!(b.proposal.workflow, "none", "{label}");
            assert!(b.proposal.token.is_none(), "{label}");
            let surface = format!(
                "{} {} {:?} {}",
                b.proposal.workflow, b.proposal.topology, b.proposal.argv, b.proposal.why
            );
            assert!(
                !surface.contains("wave-swarm"),
                "{label}: invent swarm must not appear in align surface: {surface}"
            );
            assert!(
                !b.proposal
                    .argv
                    .iter()
                    .any(|a| a == "--crew" || a == "arrive"),
                "{label}: no invent crew spawn argv: {:?}",
                b.proposal.argv
            );
            assert_eq!(b.proposal.crew_total, 0, "{label}");
            // --commit path: empty argv ⇒ bail!("briefing commit: empty argv")
            assert!(
                b.proposal.argv.len() < 2,
                "{label}: commit gate treats len<2 as empty"
            );
        }
    }

    /// GS-049 airtight: loop sticky vs invent swarm → loop-work vehicle only.
    #[test]
    fn sticky_held_loop_x_swarm_rewrites_to_loop_not_wave_swarm() {
        let world = world_empty();
        let mut b = assemble(
            "wave-swarm the auth lanes",
            &world,
            &["coder".into()],
            5,
            None,
        );
        assert_eq!(b.proposal.topology, "swarm");
        assert_eq!(b.proposal.workflow, "wave-swarm");
        align_with_session_topology(&mut b, "loop", true);
        assert_eq!(b.proposal.topology, "loop");
        assert_eq!(b.proposal.workflow, "loop-work");
        assert!(b.proposal.token.is_none());
        assert!(
            b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["loop-mode", "run"]),
            "{:?}",
            b.proposal.argv
        );
        assert!(
            !b.proposal
                .argv
                .iter()
                .any(|a| a == "wave-swarm" || a == "--crew"),
            "must not keep invent swarm argv: {:?}",
            b.proposal.argv
        );
        assert_eq!(b.proposal.crew_total, 0);
        assert!(
            !b.proposal.why.contains("wave-swarm"),
            "why must not name invent wave-swarm: {}",
            b.proposal.why
        );
    }

    #[test]
    fn apply_answers_topology_swarm_rebuilds_compose_argv() {
        let mut b = assemble("diagnose the mailbox", &world_empty(), &[], 5, None);
        assert_eq!(b.proposal.topology, "open");
        apply_answers(&mut b, Some("swarm"), Some("3"), Some("@cheap")).unwrap();
        assert_eq!(b.proposal.topology, "swarm");
        assert_eq!(b.proposal.workflow, "wave-swarm");
        assert!(
            b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["--parts", "wave-swarm"]),
            "{:?}",
            b.proposal.argv
        );
        assert!(
            b.proposal.argv.windows(2).any(|w| w == ["--crew", "3"]),
            "{:?}",
            b.proposal.argv
        );
        assert!(
            b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["--role-model", "coder=@cheap"]),
            "{:?}",
            b.proposal.argv
        );
    }

    #[test]
    fn apply_answers_topology_open_clears_swarm_argv() {
        let mut b = assemble("wave-swarm the auth lanes", &world_empty(), &[], 5, None);
        apply_answers(&mut b, Some("open"), None, None).unwrap();
        assert_eq!(b.proposal.topology, "open");
        assert_eq!(b.proposal.workflow, "none");
        assert!(b.proposal.argv.is_empty(), "{:?}", b.proposal.argv);
    }

    /// bd-compose-plumbing-zdzi: the arriving pin is bound as `captain_pin` in
    /// the emitted compose argv — the friction left `captain_pin` at its `""`
    /// default, so every stage escalation targeted nobody and the operator
    /// had to bind it by hand. With no derivable pin the input is NAMED in
    /// `questions` and no argv is dispatched, never silently defaulted.
    #[test]
    fn briefing_binds_captain_pin_for_wave_swarm() {
        let world = World {
            agent_pin: Some("Buffy".into()),
            ..world_empty()
        };
        let b = assemble("wave-swarm the auth lanes", &world, &[], 5, None);
        assert_eq!(
            b.proposal.token.as_deref(),
            Some("wave-swarm"),
            "precondition: routed to the token: {:?}",
            b.proposal
        );
        let idx = b
            .proposal
            .argv
            .iter()
            .position(|a| a == "--inputs")
            .unwrap_or_else(|| panic!("--inputs must be bound: {:?}", b.proposal.argv));
        let inputs: serde_json::Value =
            serde_json::from_str(&b.proposal.argv[idx + 1]).expect("--inputs is a JSON object");
        assert_eq!(
            inputs.get("captain_pin").and_then(|v| v.as_str()),
            Some("Buffy"),
            "the arriving pin is the captain: {:?}",
            b.proposal.argv
        );

        // No derivable pin: the input is asked for, and nothing is dispatched.
        let world = World {
            agent_pin: None,
            ..world_empty()
        };
        let b = assemble("wave-swarm the auth lanes", &world, &[], 5, None);
        assert!(
            b.proposal.argv.is_empty(),
            "an underivable captain pin must not dispatch: {:?}",
            b.proposal.argv
        );
        assert!(
            b.questions
                .iter()
                .any(|q| q.id == "inputs" && q.options.iter().any(|o| o == "captain_pin")),
            "captain_pin must be named as a missing input: {:?}",
            b.questions
        );
        assert!(
            b.proposal.why.contains("captain_pin"),
            "why names the missing input: {}",
            b.proposal.why
        );
    }

    #[test]
    fn apply_answers_rejects_unknown() {
        let mut b = assemble("wave-swarm the auth lanes", &world_empty(), &[], 5, None);
        assert!(apply_answers(&mut b, Some("banana"), None, None).is_err());
        assert!(apply_answers(&mut b, None, None, Some("gpt-x")).is_err());
    }

    #[test]
    fn apply_answers_same_topology_keeps_beads_lifecycle_token() {
        let mut b = assemble(
            "beads-lifecycle for an existing open bead",
            &world_empty(),
            &[],
            5,
            None,
        );
        assert_eq!(b.proposal.workflow, "beads-lifecycle");
        apply_answers(&mut b, Some("swarm"), Some("planner:1,coder:1"), None).unwrap();
        assert_eq!(b.proposal.workflow, "beads-lifecycle");
        assert!(
            b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["--parts", "beads-lifecycle"]),
            "{:?}",
            b.proposal.argv
        );
        assert!(
            b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["--crew", "planner:1,coder:1"]),
            "{:?}",
            b.proposal.argv
        );
    }

    // governed-by: bd-7lms — every decide topology + workflow family gets a
    // briefing proposal with crew/model questions. One scenario, one path.
    fn has_crew_and_model_questions(b: &Briefing) -> bool {
        ["topology", "crew", "model"]
            .iter()
            .all(|id| b.questions.iter().any(|q| &q.id == id))
    }

    fn assert_family_surfaced(b: &Briefing, workflow: &str) {
        assert!(
            b.alternatives
                .iter()
                .any(|a| a.workflow.as_deref() == Some(workflow)),
            "alternatives must surface {workflow}: {:?}",
            b.alternatives
        );
    }

    /// One row per briefing family: goal -> topology + workflow, plus the
    /// crew/model questions and the family line in `alternatives`. The rows
    /// are the six tests this replaces, byte-for-byte on the inputs.
    #[test]
    fn briefing_covers_families() {
        let rows = [
            (
                "refactor-swarm",
                "refactor the auth module",
                "swarm",
                "refactoring",
            ),
            ("cron-swarm", "cron nightly digest", "swarm", "cron-start"),
            (
                "coalition-swarm",
                "coalition shared-file contention",
                "swarm",
                "coalition-form",
            ),
            ("debate-swarm", "debate the api design", "swarm", "debate"),
            (
                "forensics-open",
                "diagnose the mailbox leak: spindump the idle cpu",
                "open",
                "forensics",
            ),
            (
                "single-lane-steer",
                "crew-steer the peer mention fix",
                "swarm",
                "steer",
            ),
        ];
        for (name, goal, topology, workflow) in rows {
            let b = assemble(goal, &world_empty(), &[], 5, None);
            assert_eq!(b.proposal.topology, topology, "row {name}: topology");
            assert_eq!(b.proposal.workflow, workflow, "row {name}: workflow");
            assert!(
                has_crew_and_model_questions(&b),
                "row {name}: crew + model questions"
            );
            assert_family_surfaced(&b, workflow);
        }
    }

    #[test]
    fn briefing_covers_joinable_world() {
        let world = World {
            project: "toron".into(),
            dispatcher: "up".into(),
            joinable: true,
            pin_count: 3,
            ..World::default()
        };
        let b = assemble("refactor the auth module", &world, &[], 5, None);
        assert_eq!(b.proposal.topology, "swarm");
        assert!(b.proposal.join);
        assert!(has_crew_and_model_questions(&b));
        assert!(
            b.alternatives
                .iter()
                .any(|a| a.workflow.as_deref() == Some("join")),
            "alternatives must surface join: {:?}",
            b.alternatives
        );
    }

    #[test]
    fn briefing_multi_lane_without_trigger_is_explicit_none_not_open() {
        let b = assemble("two lanes auth and billing", &world_empty(), &[], 5, None);
        assert_eq!(b.proposal.topology, "none");
        assert_eq!(b.proposal.workflow, "none");
        assert!(b.proposal.argv.is_empty(), "{:?}", b.proposal.argv);
        assert!(has_crew_and_model_questions(&b));
    }

    #[test]
    fn until_green_refuses_until_verify_is_bound() {
        let held = assemble(
            "fix the failing test until it passes",
            &world_empty(),
            &[],
            5,
            None,
        );
        assert_eq!(held.proposal.workflow, "repair-until-verified");
        assert!(held.proposal.argv.is_empty(), "{:?}", held.proposal.argv);
        assert!(
            held.proposal.why.contains("verify"),
            "{}",
            held.proposal.why
        );
        assert!(held.questions.iter().any(|q| q.id == "inputs"));

        let go = assemble(
            "fix the failing test until it passes verify=cargo test",
            &world_empty(),
            &[],
            5,
            None,
        );
        assert!(
            go.proposal
                .argv
                .windows(2)
                .any(|w| w == ["--parts", "repair-until-verified"]),
            "{:?}",
            go.proposal.argv
        );
        assert!(
            go.proposal
                .argv
                .windows(2)
                .any(|w| w == ["--project", "toron"])
        );
    }

    #[test]
    fn named_harness_lands_on_the_argv_and_diagnose_then_fix_is_an_alternative() {
        let b = assemble(
            "fix the failing test until it passes verify=cargo test with opencode, then we already diagnosed it",
            &world_empty(),
            &[],
            5,
            None,
        );
        // "diagnosed" contains "diagnose"? "diagnosed".contains("diagnose") is true.
        assert!(
            b.proposal
                .argv
                .windows(2)
                .any(|w| w == ["--harness", "opencode"]),
            "{:?}",
            b.proposal.argv
        );
        assert!(
            b.alternatives
                .iter()
                .any(|a| a.workflow.as_deref() == Some("forensics")),
            "{:?}",
            b.alternatives
        );
    }

    #[test]
    fn climb_refuses_without_metric_and_target() {
        let held = assemble(
            "the slow path should keep measuring until it improves",
            &world_empty(),
            &[],
            5,
            None,
        );
        assert_eq!(held.proposal.workflow, "hillclimb");
        assert!(held.proposal.argv.is_empty(), "{:?}", held.proposal.argv);
        assert!(
            held.proposal.why.contains("metric"),
            "{}",
            held.proposal.why
        );
    }
}

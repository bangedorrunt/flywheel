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

/// wave-swarm unique inventory when the caller has not yet inferred roles.
pub const WAVE_SWARM_ROLES: &[&str] = &["coder", "reviewer", "planner", "scout", "housekeeper"];

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub topology: String,
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
fn topology_for(decision: &Decision, world: &World) -> String {
    if world.joinable {
        return "join".into();
    }
    decision.topology.to_string()
}

fn default_n(unique: u32, cap: u32) -> u32 {
    unique.max(1).min(cap)
}

/// Assemble a briefing. `unique_roles` is the packed-from inventory (may be empty).
pub fn assemble(objective: &str, world: &World, unique_roles: &[String], cap: u32) -> Briefing {
    let cap = if cap == 0 { DEFAULT_CREW_MAX } else { cap };
    let snap = decide::Snapshot {
        ready_bead_count: world.ready_beads,
        profiled_panes: world.pin_count,
        launch_pending: world.launch_pending,
        contention: !world.held_paths.is_empty(),
    };
    let decision = decide::decide(objective, &snap);
    let topology = topology_for(&decision, world);
    let unique_owned: Vec<String> = if unique_roles.is_empty() && topology == "swarm" {
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
    if topology == "join" {
        argv = vec!["flywheel".into(), "arrive".into(), world.project.clone()];
        if let Some(id) = &world.coalition_id {
            argv.extend(["--coalition".into(), id.clone()]);
        }
    } else if is_compose_token(decision.primitive) && !argv.iter().any(|a| a == "--crew") {
        argv.push("--crew".into());
        argv.push(n.to_string());
    }

    let why = if topology == "join" {
        format!(
            "live fleet on {} (pins={}, dispatcher={}) — join by default (ADR-0011 D5)",
            world.project, world.pin_count, world.dispatcher
        )
    } else {
        decision.reason.clone()
    };

    let token = is_compose_token(decision.primitive).then(|| decision.primitive.to_string());

    Briefing {
        schema: SCHEMA.into(),
        objective: objective.into(),
        world: world.clone(),
        proposal: Proposal {
            topology: topology.clone(),
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
        alternatives: vec![
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
        ],
        questions: vec![
            Question {
                id: "topology".into(),
                options: vec!["swarm".into(), "loop".into(), "open".into(), "join".into()],
                default: topology,
            },
            Question {
                id: "crew".into(),
                options: vec!["1".into(), "2".into(), "3".into(), "4".into(), "5".into()],
                default: n.to_string(),
            },
        ],
    }
}

/// GS-049: after sticky-open `[reclassify]` announce, briefing must not present
/// a conflicting second topology surface (dual SoT). Align proposal + question
/// default with sticky `session_topology`; when sticky-held against any invent
/// flip (open×loop|swarm|join|none, loop×swarm), rewrite argv so `--commit`
/// cannot exec the suppressed vehicle under an open/loop label (crew-confirm
/// is skipped for open — leaving loop argv would auto-dispatch).
pub fn align_with_session_topology(b: &mut Briefing, session: &str, sticky_held: bool) {
    let session = session.trim();
    if session.is_empty() {
        return;
    }
    if b.proposal.topology == session {
        if sticky_held && !b.proposal.why.contains("sticky") {
            b.proposal.why = format!("{} — sticky session topology held", b.proposal.why);
        }
        // Still rewrite argv when sticky-held: topology label may already match
        // after a prior align, while argv still carries a suppressed vehicle.
        if sticky_held {
            rewrite_sticky_held_argv(b, session);
        }
        return;
    }
    let classified = b.proposal.topology.clone();
    b.proposal.topology = session.to_string();
    if let Some(q) = b.questions.iter_mut().find(|q| q.id == "topology") {
        q.default = session.to_string();
    }
    b.proposal.why = format!(
        "session_topology={session}{} — aligned (suppressed dual classify surface was topology={classified}): {}",
        if sticky_held { " sticky-held" } else { "" },
        b.proposal.why
    );
    // ALL sticky-held cases — not only classified∈{swarm,join}.
    if sticky_held {
        rewrite_sticky_held_argv(b, session);
    }
}

/// Rewrite commit argv to the held session vehicle; clear crew spawn surfaces.
fn rewrite_sticky_held_argv(b: &mut Briefing, session: &str) {
    match session {
        "open" => {
            // none / empty workflow: fail-closed empty argv (commit bails) —
            // never invent a workflow name. Otherwise headless run of the
            // classified workflow (no spawn/crew, no loop-mode).
            if b.proposal.workflow.is_empty() || b.proposal.workflow == "none" {
                b.proposal.argv.clear();
            } else {
                b.proposal.argv = vec![
                    "flywheel".into(),
                    "workflow".into(),
                    "run".into(),
                    b.proposal.workflow.clone(),
                    "--mode".into(),
                    "headless".into(),
                ];
            }
            b.proposal.crew.clear();
            b.proposal.crew_total = 0;
            b.proposal.add.clear();
            b.proposal.remove.clear();
        }
        "loop" => {
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


#[cfg(test)]
mod tests {
    use super::*;

    fn world_empty() -> World {
        World {
            project: "toron".into(),
            dispatcher: "absent".into(),
            ..World::default()
        }
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
        let b = assemble("wave-swarm 3 crews", &world, &["coder".into()], 5);
        assert_eq!(b.schema, SCHEMA);
        assert_eq!(b.proposal.topology, "join");
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
        let b = assemble("wave-swarm the auth lanes", &world_empty(), &roles, 5);
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
        let b = assemble("work the ready beads", &world, &["coder".into()], 5);
        assert_eq!(b.proposal.workflow, "loop-work");
        assert_eq!(b.proposal.topology, "loop");
    }

    #[test]
    fn two_crews_packs_n_2_not_default_5() {
        let b = assemble("wave-swarm with 2 crews", &world_empty(), &[], 5);
        assert_eq!(b.proposal.topology, "swarm");
        assert_eq!(b.proposal.workflow, "wave-swarm");
        assert_eq!(b.proposal.crew_total, 2);
        assert!(b.proposal.argv.windows(2).any(|w| w == ["--crew", "2"]));
    }

    #[test]
    fn empty_unique_on_swarm_defaults_wave_roles() {
        let b = assemble("wave-swarm the auth lanes", &world_empty(), &[], 5);
        assert_eq!(b.proposal.topology, "swarm");
        assert_eq!(b.proposal.crew_total, 5);
        assert!(b.proposal.argv.iter().any(|a| a == "--crew"));
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
        let b = assemble("babysit the bd-xyz bead", &world, &["coder".into()], 5);
        let d = decide::decide("babysit the bd-xyz bead", &decide::Snapshot::default());
        assert_eq!(d.primitive, "babysit-bead");
        assert_eq!(b.proposal.topology, d.topology);
        assert_eq!(b.proposal.topology, "open");
    }

    #[test]
    fn sticky_held_suppresses_conflicting_swarm_proposal() {
        let world = world_empty();
        let mut b = assemble("wave-swarm the auth lanes", &world, &["coder".into()], 5);
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
        assert!(b.proposal.why.contains("suppressed dual"), "{}", b.proposal.why);
        assert!(
            !b.proposal.argv.iter().any(|a| a == "--crew"),
            "spawn crew argv suppressed: {:?}",
            b.proposal.argv
        );
        assert!(
            !b.proposal.argv.windows(2).any(|w| w == ["loop-mode", "run"]),
            "must not leave loop-mode under open: {:?}",
            b.proposal.argv
        );
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
        let mut b = assemble("work the ready beads", &world, &["coder".into()], 5);
        assert_eq!(b.proposal.topology, "loop", "precondition: loop classify");
        assert!(
            b.proposal.argv.windows(2).any(|w| w == ["loop-mode", "run"]),
            "precondition loop argv: {:?}",
            b.proposal.argv
        );
        align_with_session_topology(&mut b, "open", true);
        assert_eq!(b.proposal.topology, "open");
        assert!(
            !b.proposal.argv.windows(2).any(|w| w == ["loop-mode", "run"]),
            "open sticky-held must not keep loop-mode argv (commit would skip crew-confirm): {:?}",
            b.proposal.argv
        );
        assert!(
            b.proposal.argv.is_empty()
                || b.proposal
                    .argv
                    .windows(2)
                    .any(|w| w == ["workflow", "run"]),
            "open vehicle is headless workflow run or fail-closed empty: {:?}",
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
        let mut b = assemble("zzzz-unmatched-objective-qqq", &world, &[], 5);
        assert_eq!(b.proposal.topology, "none", "precondition: none classify");
        align_with_session_topology(&mut b, "open", true);
        assert_eq!(b.proposal.topology, "open");
        assert!(
            b.proposal.argv.is_empty(),
            "open×none sticky-held: fail-closed empty argv, got {:?}",
            b.proposal.argv
        );
        assert!(
            !b.proposal.argv.windows(2).any(|w| w == ["loop-mode", "run"]),
            "{:?}",
            b.proposal.argv
        );
        assert_eq!(b.proposal.crew_total, 0);
        assert!(b.proposal.why.contains("sticky-held"), "{}", b.proposal.why);
    }
}

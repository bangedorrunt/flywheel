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

/// Loud join banner (ADR-0011 D5). Never silent.
pub fn join_banner(world: &World) -> String {
    format!(
        "JOINING — not spawning  project={} pins={} dispatcher={} coalition={} held={}",
        world.project,
        world.pin_count,
        world.dispatcher,
        world.coalition_id.as_deref().unwrap_or("-"),
        if world.held_paths.is_empty() {
            "none".to_string()
        } else {
            world.held_paths.join(",")
        }
    )
}

pub fn is_compose_token(primitive: &str) -> bool {
    matches!(
        primitive,
        "wave-swarm" | "repair-until-verified" | "beads-lifecycle"
    )
}

fn topology_for(decision: &Decision, world: &World) -> String {
    if world.joinable {
        return "join".into();
    }
    match decision.primitive {
        "loop-work" => "loop".into(),
        "spawn-send" | "captain-audit" | "forensics" => "open".into(),
        "wave-swarm" | "coalition-form" => "swarm".into(),
        _ => "swarm".into(),
    }
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
        launch_pending: 0,
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
    let n = default_n(unique_roles.len() as u32, cap);
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
    fn empty_unique_on_swarm_defaults_wave_roles() {
        let b = assemble("wave-swarm the auth lanes", &world_empty(), &[], 5);
        assert_eq!(b.proposal.topology, "swarm");
        assert_eq!(b.proposal.crew_total, 5);
        assert!(b.proposal.argv.iter().any(|a| a == "--crew"));
    }
}

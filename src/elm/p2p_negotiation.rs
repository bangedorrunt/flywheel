//! P2P negotiation statechart (ADR-0021).
//!
//! Models lateral request/release/review negotiation with deadline arbitration.
//! Pure fold; commands are descriptive only.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use toron_core::kernel::{
    ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition, StatechartProgram,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum P2PNegotiationState {
    Idle,
    Proposed,
    WaitingDecision,
    Accepted,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum P2PNegotiationEvent {
    Init,
    Propose {
        path: String,
        peer: String,
        bead: Option<String>,
    },
    PeerResponded {
        accepted: bool,
    },
    DeadlineElapsed,
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct P2PNegotiationContext {
    pub path: Option<String>,
    pub peer: Option<String>,
    pub bead: Option<String>,
    pub accepted: Option<bool>,
    pub history: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum P2PNegotiationCmd {
    NotifyPeer { peer: String, path: String },
    RecordDecision { accepted: bool },
    EscalateDeadline { path: String },
}

impl CmdTrait for P2PNegotiationCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct P2PNegotiationProgramDef;

impl StatechartDefinition for P2PNegotiationProgramDef {
    type State = P2PNegotiationState;
    type Event = P2PNegotiationEvent;
    type Context = P2PNegotiationContext;
    type Cmd = P2PNegotiationCmd;
    type Msg = P2PNegotiationEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(P2PNegotiationState::Idle, P2PNegotiationContext::default())
            .state(P2PNegotiationState::Idle, |node| {
                let propose = node
                    .transition(P2PNegotiationState::Proposed)
                    .on(|e| matches!(e, P2PNegotiationEvent::Propose { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let P2PNegotiationEvent::Propose { path, peer, bead } = e {
                            ctx.path = Some(path.clone());
                            ctx.peer = Some(peer.clone());
                            ctx.bead = bead.clone();
                            ctx.history.push("proposed".into());
                            q.exec(P2PNegotiationCmd::NotifyPeer {
                                peer: peer.clone(),
                                path: path.clone(),
                            });
                        }
                    });
                node.on(propose)
            })
            .state(P2PNegotiationState::Proposed, |node| {
                let wait = node
                    .transition(P2PNegotiationState::WaitingDecision)
                    .on(|e| matches!(e, P2PNegotiationEvent::Init))
                    .action(|ctx, _, _| ctx.history.push("waiting".into()));
                // Also allow immediate peer response from Proposed.
                let accepted = node
                    .transition(P2PNegotiationState::Accepted)
                    .on(|e| matches!(e, P2PNegotiationEvent::PeerResponded { accepted: true }))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.accepted = Some(true);
                        ctx.history.push("accepted".into());
                        q.exec(P2PNegotiationCmd::RecordDecision { accepted: true });
                    });
                let rejected = node
                    .transition(P2PNegotiationState::Rejected)
                    .on(|e| matches!(e, P2PNegotiationEvent::PeerResponded { accepted: false }))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.accepted = Some(false);
                        ctx.history.push("rejected".into());
                        q.exec(P2PNegotiationCmd::RecordDecision { accepted: false });
                    });
                let expired = node
                    .transition(P2PNegotiationState::Expired)
                    .on(|e| matches!(e, P2PNegotiationEvent::DeadlineElapsed))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.history.push("expired".into());
                        if let Some(path) = ctx.path.clone() {
                            q.exec(P2PNegotiationCmd::EscalateDeadline { path });
                        }
                    });
                node.on(wait).on(accepted).on(rejected).on(expired)
            })
            .state(P2PNegotiationState::WaitingDecision, |node| {
                let accepted = node
                    .transition(P2PNegotiationState::Accepted)
                    .on(|e| matches!(e, P2PNegotiationEvent::PeerResponded { accepted: true }))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.accepted = Some(true);
                        ctx.history.push("accepted".into());
                        q.exec(P2PNegotiationCmd::RecordDecision { accepted: true });
                    });
                let rejected = node
                    .transition(P2PNegotiationState::Rejected)
                    .on(|e| matches!(e, P2PNegotiationEvent::PeerResponded { accepted: false }))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.accepted = Some(false);
                        ctx.history.push("rejected".into());
                        q.exec(P2PNegotiationCmd::RecordDecision { accepted: false });
                    });
                let expired = node
                    .transition(P2PNegotiationState::Expired)
                    .on(|e| matches!(e, P2PNegotiationEvent::DeadlineElapsed))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.history.push("expired".into());
                        if let Some(path) = ctx.path.clone() {
                            q.exec(P2PNegotiationCmd::EscalateDeadline { path });
                        }
                    });
                node.on(accepted).on(rejected).on(expired)
            })
            .state(P2PNegotiationState::Accepted, |node| {
                let reset = node
                    .transition(P2PNegotiationState::Idle)
                    .on(|e| matches!(e, P2PNegotiationEvent::Reset))
                    .action(|ctx, _, _| {
                        *ctx = P2PNegotiationContext::default();
                    });
                node.on(reset)
            })
            .state(P2PNegotiationState::Rejected, |node| {
                let reset = node
                    .transition(P2PNegotiationState::Idle)
                    .on(|e| matches!(e, P2PNegotiationEvent::Reset))
                    .action(|ctx, _, _| {
                        *ctx = P2PNegotiationContext::default();
                    });
                node.on(reset)
            })
            .state(P2PNegotiationState::Expired, |node| {
                let reset = node
                    .transition(P2PNegotiationState::Idle)
                    .on(|e| matches!(e, P2PNegotiationEvent::Reset))
                    .action(|ctx, _, _| {
                        *ctx = P2PNegotiationContext::default();
                    });
                node.on(reset)
            })
            .build()
    }

    fn init_event() -> Self::Event {
        P2PNegotiationEvent::Init
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        P2PNegotiationEvent::Reset
    }
}

#[must_use]
pub fn build_p2p_negotiation_machine()
-> Machine<P2PNegotiationState, P2PNegotiationEvent, P2PNegotiationContext, P2PNegotiationCmd> {
    P2PNegotiationProgramDef::machine()
}

pub type P2PNegotiationProgram = StatechartProgram<P2PNegotiationProgramDef>;

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::{Boot, MachineState, Program};

    #[test]
    fn happy_path_propose_to_accepted() {
        let machine = build_p2p_negotiation_machine();
        let mut ms = MachineState::at(P2PNegotiationState::Idle, P2PNegotiationContext::default());

        let out = machine.step_state(
            &ms,
            &P2PNegotiationEvent::Propose {
                path: "src/lib.rs".into(),
                peer: "FoggyDesert".into(),
                bead: Some("bd-123".into()),
            },
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&P2PNegotiationState::Proposed)
        );
        assert!(out.commands.iter().any(|c| matches!(
            c,
            P2PNegotiationCmd::NotifyPeer { peer, path } if peer == "FoggyDesert" && path == "src/lib.rs"
        )));
        assert_eq!(out.context.path.as_deref(), Some("src/lib.rs"));
        assert_eq!(out.context.peer.as_deref(), Some("FoggyDesert"));
        assert_eq!(out.context.bead.as_deref(), Some("bd-123"));

        ms = MachineState::at(P2PNegotiationState::Proposed, out.context);
        let out = machine.step_state(&ms, &P2PNegotiationEvent::Init);
        assert_eq!(
            out.configuration.primary(),
            Some(&P2PNegotiationState::WaitingDecision)
        );

        ms = MachineState::at(P2PNegotiationState::WaitingDecision, out.context);
        let out = machine.step_state(&ms, &P2PNegotiationEvent::PeerResponded { accepted: true });
        assert_eq!(
            out.configuration.primary(),
            Some(&P2PNegotiationState::Accepted)
        );
        assert!(
            out.commands
                .iter()
                .any(|c| matches!(c, P2PNegotiationCmd::RecordDecision { accepted: true }))
        );
        assert_eq!(out.context.accepted, Some(true));

        ms = MachineState::at(P2PNegotiationState::Accepted, out.context);
        let out = machine.step_state(&ms, &P2PNegotiationEvent::Reset);
        assert_eq!(
            out.configuration.primary(),
            Some(&P2PNegotiationState::Idle)
        );
        assert_eq!(out.context, P2PNegotiationContext::default());
    }

    #[test]
    fn propose_to_rejected() {
        let machine = build_p2p_negotiation_machine();
        let ms = MachineState::at(
            P2PNegotiationState::WaitingDecision,
            P2PNegotiationContext {
                path: Some("src/main.rs".into()),
                peer: Some("BlackGorge".into()),
                ..Default::default()
            },
        );

        let out = machine.step_state(&ms, &P2PNegotiationEvent::PeerResponded { accepted: false });
        assert_eq!(
            out.configuration.primary(),
            Some(&P2PNegotiationState::Rejected)
        );
        assert!(
            out.commands
                .iter()
                .any(|c| matches!(c, P2PNegotiationCmd::RecordDecision { accepted: false }))
        );
        assert_eq!(out.context.accepted, Some(false));
    }

    #[test]
    fn propose_timeout_escalation() {
        let machine = build_p2p_negotiation_machine();
        let ms = MachineState::at(
            P2PNegotiationState::WaitingDecision,
            P2PNegotiationContext {
                path: Some("src/cmd/peer.rs".into()),
                peer: Some("AmberHarbor".into()),
                ..Default::default()
            },
        );

        let out = machine.step_state(&ms, &P2PNegotiationEvent::DeadlineElapsed);
        assert_eq!(
            out.configuration.primary(),
            Some(&P2PNegotiationState::Expired)
        );
        assert!(out.commands.iter().any(|c| matches!(
            c,
            P2PNegotiationCmd::EscalateDeadline { path } if path == "src/cmd/peer.rs"
        )));
    }

    #[test]
    fn tla_export_p2p_negotiation() {
        let machine = build_p2p_negotiation_machine();
        let tla = machine.to_tla_named("P2PNegotiation");
        assert!(tla.starts_with("---- MODULE P2PNegotiation ----"));
        assert!(tla.contains("VARIABLES state"));
        assert!(tla.contains("Init == state = \"Idle\""));
        assert!(tla.contains("\"Proposed\""));
        assert!(tla.contains("\"WaitingDecision\""));
        assert!(tla.contains("\"Accepted\""));
        assert!(tla.contains("\"Rejected\""));
        assert!(tla.contains("\"Expired\""));
        assert!(tla.ends_with("====") || tla.ends_with("====\n"));
    }

    #[test]
    fn elm_program_integration_step() {
        let init = P2PNegotiationProgram::init(&Boot::default());
        let model = init.model;
        assert_eq!(model.state(), Some(&P2PNegotiationState::Idle));

        let update = P2PNegotiationProgram::update(
            &model,
            &P2PNegotiationEvent::Propose {
                path: "src/elm.rs".into(),
                peer: "PeerA".into(),
                bead: None,
            },
        );
        assert_eq!(update.model.state(), Some(&P2PNegotiationState::Proposed));
    }
}

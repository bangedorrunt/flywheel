// governed-by: ADR-0016
//! Pure durable-loop phase transition policy.

use crate::durable::{IllegalTransition, LoopEvent, LoopPhase};

pub(crate) fn transition(
    from: &LoopPhase,
    event: &LoopEvent,
) -> Result<LoopPhase, IllegalTransition> {
    match (from, event) {
        (LoopPhase::ClaimWatch, LoopEvent::Verify) => Err(IllegalTransition {
            from: from.clone(),
            event: event.clone(),
            reason: "Verify without close",
        }),
        (
            LoopPhase::Verify,
            LoopEvent::Complete {
                wave_gate: true, ..
            },
        ) => Err(IllegalTransition {
            from: from.clone(),
            event: event.clone(),
            reason: "Complete while WaveGate",
        }),
        (
            LoopPhase::Verify,
            LoopEvent::Complete {
                has_command_verified: false,
                wave_gate: false,
            },
        ) => Err(IllegalTransition {
            from: from.clone(),
            event: event.clone(),
            reason: "Complete without CommandVerified",
        }),
        (
            LoopPhase::Idle,
            LoopEvent::Dispatch {
                agent_status,
                force: false,
            },
        ) if agent_status == "working" => Err(IllegalTransition {
            from: from.clone(),
            event: event.clone(),
            reason: "Dispatch to working without force",
        }),
        (
            LoopPhase::Reclaim,
            LoopEvent::Reclaim {
                holder_pin,
                claimant_pin,
                is_live: true,
            },
        ) if holder_pin != claimant_pin => Err(IllegalTransition {
            from: from.clone(),
            event: event.clone(),
            reason: "Reclaim foreign reservation",
        }),
        (LoopPhase::Idle, LoopEvent::Dispatch { .. }) => Ok(LoopPhase::Dispatched),
        (LoopPhase::Dispatched, LoopEvent::ClaimSeen) => Ok(LoopPhase::ClaimWatch),
        (LoopPhase::Dispatched, LoopEvent::PaneExited) => Ok(LoopPhase::Reclaim),
        (LoopPhase::ClaimWatch, LoopEvent::PaneExited) => Ok(LoopPhase::Reclaim),
        (LoopPhase::Reclaim, LoopEvent::Reclaim { .. }) => Ok(LoopPhase::Idle),
        (LoopPhase::ClaimWatch, LoopEvent::BrClose) => Ok(LoopPhase::Verify),
        (
            LoopPhase::Verify,
            LoopEvent::Complete {
                has_command_verified: true,
                wave_gate: false,
            },
        ) => Ok(LoopPhase::Complete),
        (LoopPhase::Verify, LoopEvent::WaveGate { .. }) => Ok(LoopPhase::WaveHeld),
        (LoopPhase::WaveHeld, LoopEvent::BrClose) => Ok(LoopPhase::Idle),
        (LoopPhase::Idle, LoopEvent::Blocked) => Ok(LoopPhase::Blocked),
        _ => Err(IllegalTransition {
            from: from.clone(),
            event: event.clone(),
            reason: "illegal transition",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_policy_rejects_unverified_completion() {
        let result = transition(
            &LoopPhase::Verify,
            &LoopEvent::Complete {
                has_command_verified: false,
                wave_gate: false,
            },
        );
        assert_eq!(
            result.unwrap_err().reason,
            "Complete without CommandVerified"
        );
    }
}

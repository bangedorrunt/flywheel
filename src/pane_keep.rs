//! A pane binding may clear only on positive evidence from the ACTIVE plane.
//! bd-j0b5 / bd-rex-pane-reap.
//!
//! On hc, a pin from a harness hc cannot inject (`freebuff`) has no hc
//! terminal on purpose, so "hc lists no pane" is not evidence of death. On
//! rex (called `superlogical` by older configs), the plane owns the pane for
//! every harness: a missing rex block is death, and refusing to clear there
//! stranded every dead rex row and suppressed stale-heartbeat pages.
//!
//! One rule, shared by every clear (and by the supervisor heartbeat page):
//! the plane can prove death, the binding names a non-empty pane id, and the
//! known live set omits it.

use std::collections::HashSet;

/// Harnesses hc can inject. A pin hc cannot inject has no hc terminal by
/// design, so a missing pane is never evidence against it.
pub const HC_INJECTABLE: [&str; 4] = ["opencode", "pi", "grok", "omp"];

/// May this pane binding be cleared? True only on positive evidence: the
/// active backend is hc, the harness is one hc injects, the binding
/// names a pane, and a live pane set is known that omits it. `None` (hc did
/// not answer) clears nothing, and an unknown harness keeps its row.
pub fn pane_binding_may_clear(
    harness: &str,
    backend: &str,
    pane_id: &str,
    live_panes: Option<&HashSet<String>>,
) -> bool {
    let plane_proves_death = match backend {
        "hc" => HC_INJECTABLE.contains(&harness),
        // rex has no injectable-harness carve-out: it lists every block it
        // owns, so an omitted block is positive evidence for any harness.
        "rex" | "superlogical" => true,
        _ => false,
    };
    plane_proves_death
        && !pane_id.is_empty()
        && live_panes.is_some_and(|live| !live.contains(pane_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn hc_keeps_non_injectable_pins_but_rex_clears_any_missing_pane() {
        let none_live = live(&[]);
        assert!(!pane_binding_may_clear(
            "freebuff",
            "hc",
            "w1:p1",
            Some(&none_live)
        ));
        assert!(!pane_binding_may_clear(
            "superlogical",
            "hc",
            "w1:p1",
            Some(&none_live)
        ));
        assert!(
            pane_binding_may_clear("freebuff", "rex", "w1:p1", Some(&none_live)),
            "rex lists every block it owns, so a missing block is death"
        );
        assert!(
            pane_binding_may_clear("freebuff", "superlogical", "w1:p1", Some(&none_live)),
            "the legacy rex name is the same plane"
        );
        assert!(pane_binding_may_clear(
            "opencode",
            "hc",
            "w1:p1",
            Some(&none_live)
        ));
        assert!(!pane_binding_may_clear(
            "claude",
            "hc",
            "w1:p1",
            Some(&none_live)
        ));
        assert!(!pane_binding_may_clear("", "hc", "w1:p1", Some(&none_live)));
        assert!(!pane_binding_may_clear("opencode", "hc", "w1:p1", None));
        let present = live(&["w1:p1"]);
        assert!(!pane_binding_may_clear(
            "opencode",
            "hc",
            "w1:p1",
            Some(&present)
        ));
        assert!(pane_binding_may_clear(
            "opencode",
            "superlogical",
            "w1:p1",
            Some(&none_live)
        ));
        assert!(!pane_binding_may_clear(
            "opencode",
            "hc",
            "",
            Some(&none_live)
        ));
    }
}

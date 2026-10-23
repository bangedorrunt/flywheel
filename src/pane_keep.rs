//! A herdr pane binding may clear; a mail-only pin never does. bd-j0b5.
//!
//! A pin from a harness herdr cannot inject (`freebuff`) or from a different
//! multiplexer (`superlogical`) has no herdr pane on purpose, so "herdr lists
//! no pane" is not evidence of death. Clearing on absence alone would retire a
//! live pin. One rule, shared by every clear (and by the supervisor heartbeat
//! page): a binding clears only for a harness herdr can inject, on herdr, with
//! a non-empty pane id the known live set omits.

use std::collections::HashSet;

/// Harnesses herdr can inject. A pin herdr cannot inject has no herdr pane by
/// design, so a missing pane is never evidence against it.
pub const HERDR_INJECTABLE: [&str; 4] = ["opencode", "pi", "grok", "omp"];

/// May this pane binding be cleared? True only on positive evidence: the
/// active backend is herdr, the harness is one herdr injects, the binding
/// names a pane, and a live pane set is known that omits it. `None` (herdr did
/// not answer) clears nothing, and an unknown harness keeps its row.
pub fn pane_binding_may_clear(
    harness: &str,
    backend: &str,
    pane_id: &str,
    live_panes: Option<&HashSet<String>>,
) -> bool {
    backend == "herdr"
        && HERDR_INJECTABLE.contains(&harness)
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
    fn a_freebuff_or_superlogical_pin_is_kept_when_herdr_has_no_pane() {
        let none_live = live(&[]);
        // A freebuff pin is invisible to herdr on purpose — absence is not death.
        assert!(!pane_binding_may_clear(
            "freebuff",
            "herdr",
            "w1:p1",
            Some(&none_live)
        ));
        assert!(!pane_binding_may_clear(
            "superlogical",
            "herdr",
            "w1:p1",
            Some(&none_live)
        ));
        // A harness herdr injects may clear once its pane is gone.
        assert!(pane_binding_may_clear(
            "opencode",
            "herdr",
            "w1:p1",
            Some(&none_live)
        ));
        // Unknown and empty harnesses keep their row.
        assert!(!pane_binding_may_clear(
            "claude",
            "herdr",
            "w1:p1",
            Some(&none_live)
        ));
        assert!(!pane_binding_may_clear(
            "",
            "herdr",
            "w1:p1",
            Some(&none_live)
        ));
        // Herdr not answering clears nothing.
        assert!(!pane_binding_may_clear("opencode", "herdr", "w1:p1", None));
        // A live pane is never cleared, and only herdr may clear at all.
        let present = live(&["w1:p1"]);
        assert!(!pane_binding_may_clear(
            "opencode",
            "herdr",
            "w1:p1",
            Some(&present)
        ));
        assert!(!pane_binding_may_clear(
            "opencode",
            "superlogical",
            "w1:p1",
            Some(&none_live)
        ));
        // An empty pane id names nothing, so it is not positive evidence.
        assert!(!pane_binding_may_clear(
            "opencode",
            "herdr",
            "",
            Some(&none_live)
        ));
    }
}

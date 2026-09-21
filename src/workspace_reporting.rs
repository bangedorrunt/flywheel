// governed-by: ADR-0016
//! Pure reporting projections for workspace/control-plane state.
//!
//! This module owns no live state and performs no I/O. It turns already
//! observed ledger facts into deterministic divergence rows for snapshots and
//! operator reports. The workspace reducer remains the authority for state;
//! this is only a read-side projection.

/// A recorded divergence between the call ledger and the bead ledger.
/// Reporting only, recording never closes, drops, or silently resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divergence {
    /// Bead shows closed while its call is still open.
    ClosedBeadOpenCorr { bead: String, corr: String },
    /// Call was answered while its bead is still held.
    AnsweredCorrHeldBead { corr: String, bead: String },
    /// A `[{id}] done` report with no legal close behind it.
    DoneWithoutLegalClose { bead: String },
}

/// Divergence inputs. Corr↔bead linkage comes from the caller (br JSON plus
/// receipt storage); this projection does not create a flywheel-only ledger.
pub struct DivergenceSpec<'a> {
    pub open_corrs: &'a [(String, String)],
    pub closed_beads: &'a [String],
    pub answered_corrs: &'a [(String, String)],
    pub held_beads: &'a [String],
    pub done_without_close: &'a [String],
}

/// Pure divergence fold. Every matching rule emits a row, and no input is
/// removed or mutated as a side effect.
pub fn record_divergence(spec: &DivergenceSpec<'_>) -> Vec<Divergence> {
    let mut rows = Vec::new();
    for (corr, bead) in spec.open_corrs {
        if spec.closed_beads.iter().any(|b| b == bead) {
            rows.push(Divergence::ClosedBeadOpenCorr {
                bead: bead.clone(),
                corr: corr.clone(),
            });
        }
    }
    for (corr, bead) in spec.answered_corrs {
        if spec.held_beads.iter().any(|b| b == bead) {
            rows.push(Divergence::AnsweredCorrHeldBead {
                corr: corr.clone(),
                bead: bead.clone(),
            });
        }
    }
    for bead in spec.done_without_close {
        rows.push(Divergence::DoneWithoutLegalClose { bead: bead.clone() });
    }
    rows
}

/// Deterministic operator-facing rendering. Empty is explicit: the check ran.
pub fn render_divergence(rows: &[Divergence]) -> String {
    let mut out = String::from("RECORD DIVERGENCE");
    if rows.is_empty() {
        out.push_str(": none");
        return out;
    }
    for row in rows {
        match row {
            Divergence::ClosedBeadOpenCorr { bead, corr } => {
                out.push_str(&format!(
                    "\n- closed-bead/open-corr bead={bead} corr={corr}"
                ));
            }
            Divergence::AnsweredCorrHeldBead { corr, bead } => {
                out.push_str(&format!(
                    "\n- answered-corr/held-bead corr={corr} bead={bead}"
                ));
            }
            Divergence::DoneWithoutLegalClose { bead } => {
                out.push_str(&format!("\n- done-without-legal-close bead={bead}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_and_render_are_deterministic_and_non_mutating() {
        let open = vec![("corr-1".into(), "bd-1".into())];
        let closed = vec!["bd-1".into()];
        let spec = DivergenceSpec {
            open_corrs: &open,
            closed_beads: &closed,
            answered_corrs: &[],
            held_beads: &[],
            done_without_close: &["bd-2".into()],
        };
        let rows = record_divergence(&spec);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            render_divergence(&rows),
            "RECORD DIVERGENCE\n- closed-bead/open-corr bead=bd-1 corr=corr-1\n- done-without-legal-close bead=bd-2"
        );
        assert_eq!(open[0].0, "corr-1");
        assert_eq!(closed[0], "bd-1");
    }
}

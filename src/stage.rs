//! Judgment STAGES: composable, individually skippable Jev (or plain-code) checks
//! at named points in a workflow. bd-51zr.
//!
//! ## Why this is a module and not a helper
//!
//! One Jev call is not a design. A workflow wants several: rank the crew before
//! dispatch, decide whether the work needs dispatching at all, notice that a
//! finished run's evidence does not show what it claims. Each of those is the same
//! shape — a pack of questions, a threshold, a verdict — and each must be able to
//! say "I could not tell" without taking the workflow down with it.
//!
//! So a [`Stage`] is that shape, and the three variants exist because the honest
//! answer to "should a model judge this?" is sometimes "no, and the compiler can
//! already tell you".
//!
//! ## The three variants, and why `Code` is not a consolation prize
//!
//! - [`Stage::Code`] — a plain predicate. **No network.** This variant exists so a
//!   check the compiler can already answer does NOT become a 5-second HTTP call.
//!   "Does this verify command match zero tests?" is decidable in Rust. Wiring it
//!   as a Jev stage would be paying latency to learn something known.
//! - [`Stage::Jev`] — a pack of questions plus a code-side threshold.
//! - [`Stage::Compose`] — several stages and an explicit rule, combined in code.
//!
//! ## Three rules, each traceable to a documented property of the model
//!
//! 1. **Single-hop, literal questions only.** Jev answers the question you wrote,
//!    not the one you meant, and it struggles with indirection. Where
//!    interpretation is unavoidable the documented remedy is to split it into two
//!    literal questions and combine them in code. So a stage never asks "is this
//!    plan good?" — that is a three-hop question, and [`Stage::Compose`] is how you
//!    express it as several one-hop ones.
//! 2. **No arithmetic in Jev.** It does not count reliably and it is not a
//!    calculator. Every sum, threshold, and comparison lives in `policy` — in this
//!    file, in Rust — never in a criterion string.
//! 3. **A stage may return "I could not tell".** [`Signal::Unknown`] is a
//!    first-class outcome, not an error to be squashed. A stage that cannot reach
//!    its judgment, or whose answer is below the floor, says Unknown and the
//!    deterministic path stands. This is the single most load-bearing property of
//!    the whole design: the door must never depend on a network call.
//!
//! ## Policy is code, judgments are raw
//!
//! A stage returns raw answers. A rule in this file turns them into a [`Verdict`].
//! The model never decides the gate. That split is what keeps a ranked menu, a
//! held wave, and a false green all resolvable by the same mechanism without any
//! of them becoming load-bearing on a probability.

use crate::crew_menu::HarnessMenu;
use crate::judgment::{Answers, QuestionPack, Transport};

// bd-51zr: this is a REGISTRY, and only one stage is registered so far
// (`crew-fit`). The rest of the surface — `Inflight`, `Verdict`, the `Any` and
// `Majority` rules, `verdict_of` — is exercised by tests and constructed by no
// production path yet, because the second and third stages have not landed. That is
// the same state `judgment::OfflineTransport` is in, and it carries the same
// `cfg_attr`: the deny-path contract and its tests are the point, and the
// production construction arrives with the stage that needs it. NOT a suppression
// of a known defect: everything named here has a test that would fail if its
// behaviour changed.
#[cfg_attr(not(test), allow(dead_code))]
/// Where a stage runs relative to the work it judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Before the work starts. Wide, speculative, decides whether to begin.
    ///
    /// Budget: a preflight stage is on a path an operator is already waiting on, so
    /// it must tolerate the transport's 5s timeout without becoming a 5s door.
    Preflight,
    /// While the work runs. Narrow, reactive, only on a signal.
    Inflight,
}

impl std::fmt::Display for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preflight => f.write_str("preflight"),
            Self::Inflight => f.write_str("inflight"),
        }
    }
}

/// A stage's own answer, before any policy is applied. Deliberately not a verdict:
/// the whole point is that policy lives in [`Rule`], not in the model's output.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    /// The literal condition holds.
    Holds,
    /// The literal condition does not hold.
    Fails,
    /// Could not be determined: no key, transport error, malformed answer, or a
    /// value below the stage's own floor.
    ///
    /// Not an error variant. Callers must handle it by falling back, and a stage
    /// that cannot answer must never be indistinguishable from one that answered
    /// "no".
    Unknown,
}

#[cfg_attr(not(test), allow(dead_code))]
/// What the composed pipeline concluded. The only type a caller acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every required condition held.
    Proceed,
    /// A condition failed in a way code is expected to handle.
    Reject,
    /// At least one required stage could not answer.
    ///
    /// Distinct from `Reject` on purpose: an unanswered stage is not a refusal, and
    /// collapsing the two would let a network outage masquerade as a decision.
    Unavailable,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proceed => f.write_str("proceed"),
            Self::Reject => f.write_str("reject"),
            Self::Unavailable => f.write_str("unavailable"),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
/// How a [`Stage::Compose`] combines its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Every child must hold. One `Fails` is a `Fails`.
    All,
    /// One child holding is enough.
    Any,
    /// A configurable majority. Present because "any serious violation needs
    /// separate conditions" — weighted scores suit compensating preferences, and a
    /// safety-ish check must not be outvoted by a strong showing elsewhere.
    Majority,
}

/// One stage's outcome, with the reason in operator-readable form.
///
/// The `detail` string is not decoration. A stage that silently returns `Unknown`
/// is indistinguishable from one that was never run, and that is how a judgment
/// feature becomes folklore.
#[derive(Debug, Clone, PartialEq)]
pub struct StageReport {
    pub id: &'static str,
    pub placement: Placement,
    pub signal: Signal,
    pub detail: String,
}

impl StageReport {
    fn new(
        id: &'static str,
        placement: Placement,
        signal: Signal,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            id,
            placement,
            signal,
            detail: detail.into(),
        }
    }
}

/// What a stage is allowed to look at. Extensible by construction: named fields
/// for what stages need today, and a catch-all for the rest.
pub struct StageCtx<'a> {
    /// The objective the work is trying to achieve. Every Jev stage is asked about
    /// this, so it is the one field that is never optional.
    pub objective: &'a str,
    /// The derived crew menu, mutable for ranking stages.
    pub menu: &'a mut HarnessMenu,
}

/// A composable judgment or plain-code check.
pub enum Stage {
    /// A predicate with no network. See the module docs for why this exists.
    Code {
        id: &'static str,
        placement: Placement,
        check: fn(&StageCtx) -> (Signal, String),
    },
    /// A pack of questions plus a code-side threshold.
    Jev {
        id: &'static str,
        placement: Placement,
        /// Build the pack for this run. Questions are per-run because the candidate
        /// set is data, not authored content.
        pack: fn(&StageCtx) -> QuestionPack,
        /// Read the answers into a signal, and act on this stage's own domain if it
        /// has one. This is where the model's numbers are interpreted, and it MUST
        /// NOT do arithmetic beyond a threshold compare — rule 2 in the module docs.
        /// A ranking stage applies its own ranking here; a stage that only judges
        /// ignores the context.
        read: fn(&Answers, &mut StageCtx) -> (Signal, String),
    },
    /// Several stages under one rule.
    Compose {
        id: &'static str,
        stages: Vec<Stage>,
        rule: Rule,
    },
}

impl Stage {
    /// Run this stage. NEVER panics and never returns an error type: every failure
    /// mode is a [`Signal`] or a [`Verdict`], because the callers are workflow
    /// doors and they must survive anything a transport or a model can do.
    pub fn run(&self, ctx: &mut StageCtx, transport: &dyn Transport) -> StageReport {
        match self {
            Self::Code {
                id,
                placement,
                check,
            } => {
                let (signal, detail) = check(ctx);
                StageReport::new(id, *placement, signal, detail)
            }
            Self::Jev {
                id,
                placement,
                pack,
                read,
            } => {
                let pack = pack(ctx);
                let slice = crate::judgment::build_state_slice(
                    [("objective".to_string(), ctx.objective.to_string())]
                        .into_iter()
                        .collect(),
                );
                match transport.ask(&slice, &pack) {
                    Ok(answers) => {
                        let (signal, detail) = read(&answers, ctx);
                        StageReport::new(id, *placement, signal, detail)
                    }
                    Err(e) => StageReport::new(
                        id,
                        *placement,
                        Signal::Unknown,
                        format!("could not ask: {e}"),
                    ),
                }
            }
            Self::Compose { id, stages, rule } => {
                let reports: Vec<StageReport> =
                    stages.iter().map(|s| s.run(ctx, transport)).collect();
                let (signal, detail) = combine(*rule, &reports);
                StageReport::new(id, Placement::Preflight, signal, detail)
            }
        }
    }
}

/// Combine child reports under one rule. All policy for a composed stage is here,
/// in Rust, where it is testable without a model.
fn combine(rule: Rule, reports: &[StageReport]) -> (Signal, String) {
    let detail = reports
        .iter()
        .map(|r| format!("{}={:?}", r.id, r.signal))
        .collect::<Vec<_>>()
        .join(", ");
    let holds = reports.iter().filter(|r| r.signal == Signal::Holds).count();
    let fails = reports.iter().filter(|r| r.signal == Signal::Fails).count();
    let unknown = reports
        .iter()
        .filter(|r| r.signal == Signal::Unknown)
        .count();
    let signal = match rule {
        Rule::All => {
            if fails > 0 {
                Signal::Fails
            } else if unknown > 0 {
                Signal::Unknown
            } else {
                Signal::Holds
            }
        }
        Rule::Any => {
            if holds > 0 {
                Signal::Holds
            } else if unknown > 0 {
                Signal::Unknown
            } else {
                Signal::Fails
            }
        }
        Rule::Majority => {
            // Strict majority of the ANSWERED children. Unknown children abstain
            // rather than counting as failures — a stage that could not answer has
            // no opinion, and letting absences decide a majority would make an
            // outage look like a verdict.
            let answered = holds + fails;
            if holds > fails {
                Signal::Holds
            } else if fails > holds {
                Signal::Fails
            } else if answered == 0 {
                Signal::Unknown
            } else {
                // An exact tie is genuinely undecided.
                Signal::Unknown
            }
        }
    };
    (signal, detail)
}

#[cfg_attr(not(test), allow(dead_code))]
/// Turn a stage tree into a verdict, for the placement that matters.
pub fn verdict_of(reports: &[StageReport], placement: Placement) -> Verdict {
    let relevant: Vec<&StageReport> = reports
        .iter()
        .filter(|r| r.placement == placement)
        .collect();
    if relevant.is_empty() {
        return Verdict::Unavailable;
    }
    if relevant.iter().any(|r| r.signal == Signal::Fails) {
        return Verdict::Reject;
    }
    if relevant.iter().any(|r| r.signal == Signal::Unknown) {
        return Verdict::Unavailable;
    }
    Verdict::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judgment::JevError;

    fn ctx<'a>(objective: &'a str, menu: &'a mut HarnessMenu) -> StageCtx<'a> {
        StageCtx { objective, menu }
    }

    fn menu() -> HarnessMenu {
        HarnessMenu {
            crew: 2,
            options: vec![],
            registered_unconfigured: vec![],
        }
    }

    /// A transport that always fails, so the "never fails" property is reachable
    /// on demand rather than by unplugging the network.
    struct Broken;
    impl Transport for Broken {
        fn ask(
            &self,
            _s: &crate::judgment::StateSlice,
            _p: &QuestionPack,
        ) -> Result<Answers, JevError> {
            Err(JevError::Offline("refused".into()))
        }
    }

    fn never_read() -> fn(&Answers, &mut StageCtx) -> (Signal, String) {
        |_: &Answers, _: &mut StageCtx| unreachable!("this read must never be evaluated")
    }

    #[test]
    fn a_broken_transport_yields_unknown_never_an_error() {
        let stage = Stage::Jev {
            id: "ask",
            placement: Placement::Preflight,
            pack: |_| QuestionPack {
                id: "p".into(),
                version: 1,
                model: "m".into(),
                questions: vec![],
            },
            read: never_read(),
        };
        let mut m = menu();
        let mut c = ctx("objective", &mut m);
        let r = stage.run(&mut c, &Broken);
        assert_eq!(r.signal, Signal::Unknown, "a dead transport is Unknown");
        assert!(
            r.detail.contains("could not ask"),
            "the reason must be kept: {}",
            r.detail
        );
    }

    #[test]
    fn a_read_that_would_panic_is_never_reached_when_the_transport_is_down() {
        // The `read` closure is `unreachable!()`. If the runner ever called it
        // before/without a successful ask, this test would abort rather than pass.
        let stage = Stage::Jev {
            id: "ask",
            placement: Placement::Inflight,
            pack: |_| QuestionPack {
                id: "p".into(),
                version: 1,
                model: "m".into(),
                questions: vec![],
            },
            read: never_read(),
        };
        let mut m = menu();
        let mut c = ctx("o", &mut m);
        assert_eq!(stage.run(&mut c, &Broken).signal, Signal::Unknown);
    }

    #[test]
    fn code_stages_run_without_a_transport_at_all() {
        let stage = Stage::Code {
            id: "wave-held",
            placement: Placement::Preflight,
            check: |_: &StageCtx| (Signal::Holds, "not held".into()),
        };
        let mut m = menu();
        let mut c = ctx("o", &mut m);
        let r = stage.run(&mut c, &Broken);
        assert_eq!(
            r.signal,
            Signal::Holds,
            "a Code stage ignores the transport"
        );
    }

    #[test]
    fn unknown_is_not_a_refusal_under_any_rule() {
        let mk = |sig: Signal, id: &'static str| StageReport {
            id,
            placement: Placement::Preflight,
            signal: sig,
            detail: String::new(),
        };
        // All: a failure dominates, but with no failure an Unknown must not become
        // a Reject — that is the outage-masquerading-as-decision bug.
        assert_eq!(
            verdict_of(&[mk(Signal::Unknown, "a")], Placement::Preflight),
            Verdict::Unavailable
        );
        assert_eq!(
            verdict_of(
                &[mk(Signal::Holds, "a"), mk(Signal::Unknown, "b")],
                Placement::Preflight
            ),
            Verdict::Unavailable
        );
        assert_eq!(
            verdict_of(
                &[mk(Signal::Fails, "a"), mk(Signal::Unknown, "b")],
                Placement::Preflight
            ),
            Verdict::Reject
        );
        assert_eq!(
            verdict_of(
                &[mk(Signal::Holds, "a"), mk(Signal::Holds, "b")],
                Placement::Preflight
            ),
            Verdict::Proceed
        );
    }

    #[test]
    fn placement_filters_which_stages_decide_the_verdict() {
        let mk = |sig: Signal, place: Placement| StageReport {
            id: "x",
            placement: place,
            signal: sig,
            detail: String::new(),
        };
        let reports = [
            mk(Signal::Fails, Placement::Inflight),
            mk(Signal::Holds, Placement::Preflight),
        ];
        assert_eq!(verdict_of(&reports, Placement::Preflight), Verdict::Proceed);
        assert_eq!(verdict_of(&reports, Placement::Inflight), Verdict::Reject);
    }

    #[test]
    fn no_stage_for_a_placement_is_unavailable_not_proceed() {
        let r = StageReport {
            id: "x",
            placement: Placement::Inflight,
            signal: Signal::Holds,
            detail: String::new(),
        };
        assert_eq!(verdict_of(&[r], Placement::Preflight), Verdict::Unavailable);
    }

    #[test]
    fn majority_lets_an_absent_stage_abstain_rather_than_vote_no() {
        let mk = |sig: Signal, id: &'static str| StageReport {
            id,
            placement: Placement::Preflight,
            signal: sig,
            detail: String::new(),
        };
        // Two Holds, one Unknown: the Unknown must not become a Fails.
        let (sig, _) = combine(
            Rule::Majority,
            &[
                mk(Signal::Holds, "a"),
                mk(Signal::Holds, "b"),
                mk(Signal::Unknown, "c"),
            ],
        );
        assert_eq!(sig, Signal::Holds);
        // An exact tie among answered stages is genuinely undecided.
        let (sig, _) = combine(
            Rule::Majority,
            &[mk(Signal::Holds, "a"), mk(Signal::Fails, "b")],
        );
        assert_eq!(sig, Signal::Unknown, "a tie is not a win");
        // All Unknown: no opinion at all.
        let (sig, _) = combine(Rule::Majority, &[mk(Signal::Unknown, "a")]);
        assert_eq!(sig, Signal::Unknown);
    }

    #[test]
    fn any_requires_a_real_hold_not_merely_the_absence_of_failure() {
        let mk = |sig: Signal, id: &'static str| StageReport {
            id,
            placement: Placement::Preflight,
            signal: sig,
            detail: String::new(),
        };
        let (sig, _) = combine(Rule::Any, &[mk(Signal::Unknown, "a")]);
        assert_eq!(sig, Signal::Unknown, "an unknown child cannot satisfy Any");
        let (sig, _) = combine(Rule::Any, &[mk(Signal::Fails, "a"), mk(Signal::Holds, "b")]);
        assert_eq!(sig, Signal::Holds);
    }

    #[test]
    fn a_composed_stage_names_every_child_in_its_detail() {
        let mk = |sig: Signal, id: &'static str| StageReport {
            id,
            placement: Placement::Preflight,
            signal: sig,
            detail: String::new(),
        };
        let (_, detail) = combine(
            Rule::All,
            &[mk(Signal::Holds, "ready"), mk(Signal::Fails, "held")],
        );
        assert!(detail.contains("ready=Holds"), "{detail}");
        assert!(detail.contains("held=Fails"), "{detail}");
    }
}

//! Jev RANKS the crew menu. It never defines it. bd-drlm.
//!
//! The split is the whole point, and it is the reason this is a separate module
//! rather than a branch inside [`crate::crew_menu`]. The menu is DERIVED from the
//! harness registry and the configured profiles, so an operator can audit it. If
//! the model also chose the options, the menu would be unauditable; and a menu
//! that does not exist cannot be ranked at all.
//!
//! TWO RULES THAT SHAPE EVERYTHING HERE.
//!
//! **No Choice question over the harnesses.** A `choice` question needs the
//! candidate set in its `criteria`, which would make the model's request carry a
//! SECOND hand-written copy of the menu — free to drift from the one code
//! derived. Instead each candidate gets its own `score` question about a FIXED
//! fit rubric, and the candidates are only ever named in `state`. The rubric
//! describes fit, never a list of harnesses, so it cannot become a stale menu.
//!
//! **Jev is an enhancement, never a dependency.** No key, a transport error, a
//! 429, or a model that shrugs (low confidence) all leave the deterministic order
//! exactly as it was, and say so in the outcome. A crew cannot fail to be built
//! because a model was unreachable, and a shrug must not be allowed to reshuffle
//! a menu that was already right — confidence is the gate, exactly as the docs
//! intend.
//!
//! What survives a Jev pass is deliberately small: the ORDER, and nothing else.
//! The diversity floor still applies afterwards, so a model cannot collapse a
//! crew of five onto one lens, and `asks_for_a_model` and the slot gate are
//! untouched because they are tier facts, not opinions.

use crate::crew_menu::{HarnessMenu, HarnessOption};
use crate::judgment::{Answer, Answers, QuestionDef, QuestionKind, QuestionPack, Transport};
use std::collections::{BTreeMap, HashMap};

/// The fit rubric, low to high. This is a rubric about FIT, never a list of
/// harnesses — see the module note. Ordered, because `score` interpolates
/// between levels and the order carries the meaning.
pub const FIT_LEVELS: [&str; 4] = [
    "a poor fit: this harness cannot usefully serve the objective",
    "a workable fit: usable, with real friction or gaps",
    "a strong fit: well suited with minor friction",
    "an excellent fit: ideally suited, no material friction",
];

/// A score below this confidence is a shrug, not a signal, and is ignored.
///
/// Measured against the design: several acceptable alternatives legitimately
/// spread probability, and a low-confidence answer on a harmless preference must
/// not invalidate the deterministic order. Confidence summarises distribution
/// concentration, not correctness, so it earns a floor rather than a veto.
pub const CONFIDENCE_FLOOR: f64 = 0.55;

/// One candidate's graded fit.
#[derive(Debug, Clone, PartialEq)]
pub struct FitScore {
    pub harness: String,
    /// Probability-weighted level position; can land between levels.
    pub score: f64,
    pub confidence: f64,
}

/// What a Jev pass actually did, so the caller can print the truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ranking {
    /// Order changed. `moved` counts options that ended up somewhere else.
    Reordered { moved: usize },
    /// The deterministic order stands. `why` is operator-readable.
    Declined { why: String },
}

/// Apply Jev's scores to a menu. PURE — no clock, no model, no I/O.
///
/// Three properties, each fenced:
///
/// 1. **Only the order changes.** Every field of every option survives, and the
///    SET is identical before and after. A ranker that can drop or add a harness
///    is a second menu.
/// 2. **A shrug cannot reorder.** Scores below [`CONFIDENCE_FLOOR`] are ignored,
///    and a harness with no score at all keeps its place relative to the
///    unshuffled ones.
/// 3. **The diversity floor is re-applied**, so a confident model cannot put one
///    harness in every leading slot of a big crew.
pub fn apply_scores(menu: &mut HarnessMenu, scores: &[FitScore]) -> Ranking {
    let usable: HashMap<&str, &FitScore> = scores
        .iter()
        .filter(|s| s.confidence >= CONFIDENCE_FLOOR)
        .map(|s| (s.harness.as_str(), s))
        .collect();

    if usable.is_empty() {
        let why = if scores.is_empty() {
            "Jev returned no scores".to_string()
        } else {
            format!(
                "every Jev score was below the {CONFIDENCE_FLOOR} confidence floor \
                 (a shrug is not a signal), so the deterministic order stands"
            )
        };
        return Ranking::Declined { why };
    }

    // The pre-pass order, so "moved" and the stable tiebreak both have a
    // reference. Index is the tiebreak: equal scores keep their prior relative
    // order, so Jev is a perturbation of a known-good order and never a reshuffle.
    let before: Vec<String> = menu.options.iter().map(|o| o.harness.clone()).collect();
    let prior: HashMap<&str, usize> = before
        .iter()
        .enumerate()
        .map(|(i, h)| (h.as_str(), i))
        .collect();

    let mut opts: Vec<(usize, HarnessOption)> = menu.options.drain(..).enumerate().collect();
    opts.sort_by(|(ia, a), (ib, b)| {
        let sa = usable
            .get(a.harness.as_str())
            .map_or(f64::NEG_INFINITY, |s| s.score);
        let sb = usable
            .get(b.harness.as_str())
            .map_or(f64::NEG_INFINITY, |s| s.score);
        // Options Jev said nothing useful about keep their place at the END of the
        // ranking rather than inheriting a fabricated score: -inf sorts them last,
        // and the index tiebreak keeps them in prior order among themselves.
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| ia.cmp(ib))
    });

    let after: Vec<HarnessOption> = opts.into_iter().map(|(_, o)| o).collect();
    // A harness "moved" if its NEW index differs from its prior one. This first
    // compared the prior index against itself, reached by two equivalent paths, so
    // `moved` was structurally always 0 and every confident pass reported "Jev
    // agreed" -- a green that would have hidden a working reordering entirely.
    let moved = after
        .iter()
        .enumerate()
        .filter(|(new_i, o)| prior.get(o.harness.as_str()) != Some(new_i))
        .count();

    menu.options = after;
    // The diversity floor is a property of the SET and the crew size, so it is
    // re-established here rather than trusted to survive a reorder.
    menu.reapply_diversity_floor();

    if moved == 0 {
        Ranking::Declined {
            why: "Jev agreed with the deterministic order".to_string(),
        }
    } else {
        Ranking::Reordered { moved }
    }
}

/// Why a harness is a candidate, as the model sees it. Only the facts the menu
/// already asserts — nothing here invents capability.
fn state_for(harness: &HarnessOption) -> serde_json::Value {
    serde_json::json!({
        "harness": harness.harness,
        "tier": harness.tier.as_str(),
        "profiles_that_use_it": harness.roles,
        "accepts_a_chosen_model": harness.asks_for_a_model,
        "why_it_is_offered": harness.why,
    })
}

/// Build the pack: ONE `score` question per candidate, asked in a single request.
///
/// PURE. The transport is the existing [`crate::judgment::UreqTransport`], so this
/// adds no second HTTP client, no second error taxonomy, and no second copy of the
/// endpoint or the key lookup. What it adds is the SHAPE: a Score per candidate
/// against a fixed fit rubric.
///
/// The candidate is named inside `instructions`, never as the question's criteria,
/// so the request never carries a second copy of the menu that could drift from
/// the one [`crate::crew_menu`] derived.
#[must_use]
pub fn fit_pack(objective: &str, candidates: &[HarnessOption]) -> QuestionPack {
    let questions = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| QuestionDef {
            id: format!("fit_{i}"),
            kind: QuestionKind::Score,
            instructions: format!(
                "The objective is: {objective}\n\nThis candidate:\n{}\n\n                 How well does it serve the objective? Judge only what is stated                  here, and do not assume capability that is not described.",
                serde_json::to_string_pretty(&state_for(c))
                    .unwrap_or_else(|_| c.harness.clone())
            ),
            options: Vec::new(),
            criteria: FIT_LEVELS.iter().map(|l| (*l).to_string()).collect(),
        })
        .collect();
    QuestionPack {
        id: "crew-menu-fit".into(),
        version: 1,
        model: "jev-latest".into(),
        questions,
    }
}

/// The state the questions are asked against.
#[must_use]
pub fn fit_state(objective: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("objective".to_string(), objective.to_string())])
}

/// `crew-fit` as a [`crate::stage::Stage`]: the first real Jev stage, and the
/// worked example of the shape the registry exists for.
///
/// Preflight, because it decides who gets dispatched and so has to happen before
/// anything is. It is a COMPOSE of two children under `All`:
///
/// 1. `menu-contested` — a [`crate::stage::Stage::Code`] check. A menu with one
///    option is not a ranking, and that is decidable in Rust for free. Making it a
///    code stage rather than a short-circuit inside the model stage means the saved
///    round trip is a property of the COMPOSITION, visible in one place, instead of
///    an `if` buried in a function that also does the ranking.
/// 2. `crew-fit:rank` — the [`crate::stage::Stage::Jev`] ranking itself.
///
/// So a single-option menu never reaches the network, and the reason is stated by
/// the tree rather than by a comment.
///
pub fn crew_fit_stage() -> crate::stage::Stage {
    use crate::stage::{Placement, Rule, Signal, Stage, StageCtx};
    Stage::Compose {
        id: "crew-fit",
        stages: vec![
            Stage::Code {
                id: "menu-contested",
                placement: Placement::Preflight,
                check: |ctx: &StageCtx| {
                    let n = ctx.menu.options.len();
                    if n < 2 {
                        (
                            Signal::Fails,
                            format!("the menu has {n} option(s), so there is nothing to rank"),
                        )
                    } else {
                        (Signal::Holds, format!("{n} candidates to rank"))
                    }
                },
            },
            Stage::Jev {
                id: "crew-fit:rank",
                placement: Placement::Preflight,
                pack: |ctx: &StageCtx| fit_pack(ctx.objective, &ctx.menu.options),
                read: |answers: &Answers, ctx: &mut StageCtx| {
                    // Same borrow order as everywhere else: score against the roster
                    // first, then take the mutable borrow to apply.
                    let scores = scores_from_answers(&ctx.menu.options, answers);
                    match apply_scores(ctx.menu, &scores) {
                        Ranking::Reordered { moved } => {
                            (Signal::Holds, format!("reordered {moved} option(s)"))
                        }
                        // A DECLINE is not a failure, and this is the live bug that
                        // proved it. The first cut mapped `Declined` to
                        // `Signal::Fails`, and the live briefing duly reported
                        //     jev declined: menu-contested=Holds, crew-fit:rank=Fails
                        // Under `Rule::All` one `Fails` makes the parent `Fails`, and
                        // `verdict_of` turns that into `Verdict::Reject`. Nothing
                        // gates on it today so nothing broke — but the moment
                        // anything does, a shrug would become a DISPATCH REFUSAL.
                        // A shrug is "no signal", which is exactly what `Unknown`
                        // means, so that is what it maps to.
                        Ranking::Declined { why } => (Signal::Unknown, why),
                    }
                },
            },
        ],
        rule: Rule::All,
    }
}

/// Rank `menu` in place with one Jev pass. The EFFECTFUL seam — the only
/// function in this module that performs I/O, and the only reason this module
/// needs a transport at all.
///
/// Everything above it is pure: the menu comes from [`crate::crew_menu`], the
/// pack and the state are derived, and [`apply_scores`] does the reordering. So
/// the whole judgement surface is testable with a fake transport, and this
/// wrapper is the only part that needs a real network to exercise.
///
/// NEVER FAILS. A transport error, an unconfigured host, a rejected answer or a
/// shrug all leave the menu's deterministic order exactly as it was and return
/// `Ranking::Declined { why }` naming the reason. A crew cannot fail to be built
/// because a model was slow, and the briefing cannot fail to render because a
/// ranking pass errored — the door must not depend on a network call.
///
/// Generic over [`Transport`] precisely so the failure paths are testable: the
/// "offline" case is the one that matters most and the one a real network makes
/// hardest to reach on demand.
pub fn rank_menu_with<T: Transport>(
    objective: &str,
    menu: &mut HarnessMenu,
    transport: &T,
) -> Ranking {
    // One option is not a ranking. Asking the model to score a single harness
    // spends a network round trip to learn something already known.
    if menu.options.len() < 2 {
        return Ranking::Declined {
            why: format!(
                "the menu has {} option(s), so there is nothing to rank",
                menu.options.len()
            ),
        };
    }
    let pack = fit_pack(objective, &menu.options);
    let slice = crate::judgment::build_state_slice(fit_state(objective));
    match transport.ask(&slice, &pack) {
        // Scores are computed BEFORE the mutable borrow: `apply_scores` takes
        // `&mut menu`, and the roster it scored against IS `menu.options`. The
        // borrow checker is what keeps those two in step, so the coupling lives
        // in the order of these statements rather than in a comment.
        Ok(answers) => {
            let scores = scores_from_answers(&menu.options, &answers);
            apply_scores(menu, &scores)
        }
        Err(e) => Ranking::Declined {
            why: format!("Jev was unreachable, so the deterministic order stands: {e}"),
        },
    }
}

/// Map the pack's answers back onto candidates. PURE.
///
/// A missing or non-finite number becomes confidence 0.0 rather than being skipped,
/// so a candidate with a broken answer cannot silently inherit a neighbour's rank —
/// [`apply_scores`] ignores it, and the deterministic order underneath is what
/// remains.
#[must_use]
pub fn scores_from_answers(candidates: &[HarnessOption], answers: &Answers) -> Vec<FitScore> {
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (score, confidence) = match answers.by_question.get(&format!("fit_{i}")) {
                Some(Answer::Score(s)) => (s.position, s.confidence),
                _ => (0.0, 0.0),
            };
            FitScore {
                harness: c.harness.clone(),
                score: if score.is_finite() { score } else { 0.0 },
                confidence: if confidence.is_finite() {
                    confidence
                } else {
                    0.0
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_registry::HarnessTier;
    use crate::judgment::JevError;
    use std::collections::BTreeSet;

    fn opt(harness: &str, roles: &[&str]) -> HarnessOption {
        HarnessOption {
            harness: harness.to_string(),
            tier: HarnessTier::Open,
            roles: roles.iter().map(|r| (*r).to_string()).collect(),
            implied_slot: Some("standard".into()),
            asks_for_a_model: true,
            why: format!("{harness} is configured"),
        }
    }

    fn menu(crew: u32, options: Vec<HarnessOption>) -> HarnessMenu {
        HarnessMenu {
            crew,
            options,
            registered_unconfigured: vec!["codex".into()],
        }
    }

    fn score(harness: &str, s: f64, c: f64) -> FitScore {
        FitScore {
            harness: harness.to_string(),
            score: s,
            confidence: c,
        }
    }

    /// A transport that answers from a script, so the effectful seam is testable
    /// without a network. `calls` counts invocations, which is how the
    /// one-option test proves the round trip was skipped rather than merely
    /// ignored.
    /// What the scripted transport replies with. An enum rather than a boxed
    /// closure so the fixture stays inspectable and `Clone` does the work.
    #[derive(Clone)]
    enum Reply {
        Ok(Answers),
        Err(JevError),
    }

    struct Scripted {
        reply: Reply,
        calls: std::cell::Cell<u32>,
    }

    impl Scripted {
        fn ok(answers: Answers) -> Self {
            Self {
                reply: Reply::Ok(answers),
                calls: std::cell::Cell::new(0),
            }
        }
        fn err(e: JevError) -> Self {
            Self {
                reply: Reply::Err(e),
                calls: std::cell::Cell::new(0),
            }
        }
    }

    impl Transport for Scripted {
        fn ask(
            &self,
            _slice: &crate::judgment::StateSlice,
            _pack: &QuestionPack,
        ) -> Result<Answers, JevError> {
            self.calls.set(self.calls.get() + 1);
            match &self.reply {
                Reply::Ok(a) => Ok(a.clone()),
                Reply::Err(e) => Err(e.clone()),
            }
        }
    }

    /// `answers()` builds the real `Answers` shape (model_id, pack_id, version,
    /// usage) rather than a `Default`, so a fixture that drifts from the struct
    /// is a compile error rather than a silently-wrong test.
    fn answers(pairs: &[(&str, f64, f64)]) -> Answers {
        let mut by_question = BTreeMap::new();
        for (id, position, confidence) in pairs {
            by_question.insert(
                (*id).to_string(),
                Answer::Score(crate::judgment::ScoreAnswer {
                    position: *position,
                    levels: FIT_LEVELS.len(),
                    confidence: *confidence,
                }),
            );
        }
        Answers {
            model_id: "jev-test".into(),
            pack_id: "crew-menu-fit".into(),
            pack_version: 1,
            by_question,
            usage: String::new(),
        }
    }

    /// The seam's whole contract: a transport error leaves the menu EXACTLY as it
    /// was and names the reason. This is the case that matters most — the door
    /// must render when the network does not — and it is the one a real network
    /// makes hardest to reach on demand, which is why it is scripted here.
    #[test]
    fn an_unreachable_jev_leaves_the_menu_exactly_as_it_was() {
        let mut m = menu(
            3,
            vec![opt("opencode", &["coder"]), opt("pi", &["planner"])],
        );
        let before: Vec<String> = m.options.iter().map(|o| o.harness.clone()).collect();
        let t = Scripted::err(JevError::Offline("connection refused".into()));
        let out = rank_menu_with("ship the thing", &mut m, &t);
        assert_eq!(
            t.calls.get(),
            1,
            "the pass must actually have been attempted"
        );
        let after: Vec<String> = m.options.iter().map(|o| o.harness.clone()).collect();
        assert_eq!(after, before, "an offline Jev must not reorder anything");
        match out {
            Ranking::Declined { why } => {
                assert!(
                    why.contains("deterministic order stands"),
                    "must say the order stands: {why}"
                );
                assert!(!why.is_empty(), "a decline must name a reason");
            }
            other => panic!("an unreachable Jev must decline, got {other:?}"),
        }
    }

    /// A single-option menu has nothing to rank. The fence is on the CALL COUNT,
    /// not the outcome: a decline for the right reason is also what a shrug
    /// produces, so asserting the reason alone would pass even if the round trip
    /// happened. The network call is the cost being avoided.
    #[test]
    fn a_single_option_menu_is_never_sent_to_jev() {
        let mut m = menu(1, vec![opt("opencode", &["coder"])]);
        let before: Vec<String> = m.options.iter().map(|o| o.harness.clone()).collect();
        let t = Scripted::ok(answers(&[]));
        let out = rank_menu_with("ship the thing", &mut m, &t);
        assert_eq!(
            t.calls.get(),
            0,
            "one option is not a ranking; the round trip must be skipped entirely"
        );
        let after: Vec<String> = m.options.iter().map(|o| o.harness.clone()).collect();
        assert_eq!(after, before);
        assert!(matches!(out, Ranking::Declined { .. }), "got {out:?}");
    }

    /// The happy path, and the reason the seam exists: a confident model moves the
    /// order. Scored the OTHER way round on purpose — `fit_1` high, `fit_0` low — so
    /// the pass genuinely flips the menu. The first version of this test scored
    /// `fit_0` high, which happened to AGREE with the deterministic order, and the
    /// ranker correctly answered "Jev agreed" — so the test went red while
    /// proving the opposite of what it claimed. It also pins the positional
    /// coupling: the pack's `fit_<i>` ids must line up with `menu.options`, or the
    /// promotion lands on the wrong harness.
    #[test]
    fn a_confident_pass_reorders_through_the_seam() {
        let mut m = menu(
            2,
            vec![opt("opencode", &["coder"]), opt("pi", &["planner"])],
        );
        let t = Scripted::ok(answers(&[("fit_0", 0.0, 0.95), ("fit_1", 3.0, 0.95)]));
        let out = rank_menu_with("a hard refactor", &mut m, &t);
        assert_eq!(t.calls.get(), 1);
        assert!(
            matches!(out, Ranking::Reordered { moved } if moved >= 1),
            "a confident DISAGREEING pass must reorder, got {out:?}"
        );
        let after: Vec<String> = m.options.iter().map(|o| o.harness.clone()).collect();
        assert_eq!(
            after,
            vec!["pi".to_string(), "opencode".to_string()],
            "fit_1 scored 3.0, so the SECOND option must be promoted"
        );
    }

    /// The live bug, pinned. A decline must never be able to become a `Fails`,
    /// because under `Rule::All` that reaches `verdict_of` as `Verdict::Reject` —
    /// and a shrug would then REFUSE A DISPATCH. The first cut mapped `Declined` to
    /// `Fails` and the live briefing reported `crew-fit:rank=Fails`; nothing gated on
    /// it yet, so it was a latent refusal rather than an observed one.
    ///
    /// The fence is on the SIGNAL, not on the wording, because the wording was never
    /// the problem.
    #[test]
    fn a_decline_is_never_a_failure_so_a_shrug_can_never_refuse_a_dispatch() {
        let mut m = menu(
            2,
            vec![opt("opencode", &["coder"]), opt("pi", &["planner"])],
        );
        // A shrug through the stage: both candidates answered below the floor.
        let t = Scripted::ok(answers(&[("fit_0", 3.0, 0.01), ("fit_1", 0.0, 0.01)]));
        let mut c = crate::stage::StageCtx {
            objective: "a hard refactor",
            menu: &mut m,
        };
        let report = crew_fit_stage().run(&mut c, &t);
        assert_ne!(
            report.signal,
            crate::stage::Signal::Fails,
            "a decline must not be Fails: that is one step from Verdict::Reject"
        );
        assert_eq!(
            report.signal,
            crate::stage::Signal::Unknown,
            "no usable signal IS Unknown; got {report:?}"
        );
        assert!(
            !report.detail.is_empty(),
            "even a shrug must name its reason: {}",
            report.detail
        );
        let verdict = crate::stage::verdict_of(&[report], crate::stage::Placement::Preflight);
        assert_ne!(
            verdict,
            crate::stage::Verdict::Reject,
            "and therefore must never reach Reject, got {verdict}"
        );
    }

    /// A shrug through the SEAM (not through `apply_scores` directly): a model that
    /// answers every question at zero confidence must leave the order alone. The
    /// floor is the only thing standing between "the model had an opinion" and "the
    /// model moved the menu", and this is the path a real shrug takes.
    #[test]
    fn a_shrug_through_the_seam_does_not_reorder() {
        let mut m = menu(
            2,
            vec![opt("pi", &["planner"]), opt("opencode", &["coder"])],
        );
        let before: Vec<String> = m.options.iter().map(|o| o.harness.clone()).collect();
        let t = Scripted::ok(answers(&[("fit_0", 3.0, 0.01), ("fit_1", 0.0, 0.01)]));
        let out = rank_menu_with("a hard refactor", &mut m, &t);
        assert_eq!(
            t.calls.get(),
            1,
            "the pass was made; the ANSWER was a shrug"
        );
        let after: Vec<String> = m.options.iter().map(|o| o.harness.clone()).collect();
        assert_eq!(after, before, "a shrug must not reorder, got {out:?}");
        assert!(matches!(out, Ranking::Declined { .. }), "got {out:?}");
    }

    /// Jev may change the ORDER and nothing else. A ranker that can drop or add a
    /// harness is a second menu, and a second menu is the thing the whole design
    /// refuses.
    #[test]
    fn only_the_order_changes_never_the_set_or_the_fields() {
        let mut m = menu(
            1,
            vec![
                opt("opencode", &["coder"]),
                opt("pi", &["coder", "planner"]),
                opt("codex", &[]),
            ],
        );
        let before_set: BTreeSet<String> = m.options.iter().map(|o| o.harness.clone()).collect();
        let before_by_name: Vec<(String, Vec<String>, bool)> = m
            .options
            .iter()
            .map(|o| (o.harness.clone(), o.roles.clone(), o.asks_for_a_model))
            .collect();

        let out = apply_scores(
            &mut m,
            &[
                score("opencode", 2.9, 0.9),
                score("pi", 0.1, 0.9),
                score("codex", 1.5, 0.9),
            ],
        );
        assert!(matches!(out, Ranking::Reordered { .. }), "{out:?}");
        let ranked = m.ranked();
        // opencode holds the top score (2.9). The deterministic order was by ROLE
        // COUNT, which put pi (2 roles) first -- so this assertion is about the
        // order having changed at all, not about a particular winner.
        assert_eq!(ranked[0], "opencode", "the top score leads: {ranked:?}");
        assert_eq!(
            ranked,
            vec!["opencode", "codex", "pi"],
            "scores, not role counts, decide the order"
        );

        let after_set: BTreeSet<String> = m.options.iter().map(|o| o.harness.clone()).collect();
        assert_eq!(
            before_set, after_set,
            "the SET is the menu; a ranker must not edit it"
        );
        for (h, roles, asks) in &before_by_name {
            let got = m
                .options
                .iter()
                .find(|o| &o.harness == h)
                .expect("still present");
            assert_eq!(&got.roles, roles, "{h}: roles changed");
            assert_eq!(
                got.asks_for_a_model, *asks,
                "{h}: the tier gate must not be negotiable"
            );
        }
        assert_eq!(
            m.registered_unconfigured,
            vec!["codex".to_string()],
            "the unconfigured remainder is not a ranking input"
        );
    }

    /// A shrug is not a signal. Below the floor the deterministic order stands,
    /// and the reason is returned so an operator is told WHY nothing moved rather
    /// than left to guess whether the model was ignored or simply agreed.
    #[test]
    fn a_shrug_cannot_reorder_a_menu_that_was_already_right() {
        for (label, scores) in [
            (
                "every score below the floor",
                vec![score("pi", 9.0, 0.01), score("opencode", 0.0, 0.2)],
            ),
            ("no scores at all", vec![]),
        ] {
            let mut m = menu(
                1,
                vec![
                    opt("pi", &["coder", "planner"]),
                    opt("opencode", &["coder"]),
                ],
            );
            let before: Vec<String> = m.options.iter().map(|o| o.harness.clone()).collect();
            let out = apply_scores(&mut m, &scores);
            assert!(
                matches!(out, Ranking::Declined { .. }),
                "{label}: a shrug must decline, got {out:?}"
            );
            let after: Vec<String> = m.options.iter().map(|o| o.harness.clone()).collect();
            assert_eq!(after, before, "{label}: the order moved anyway");
            if let Ranking::Declined { why } = out {
                assert!(!why.is_empty(), "{label}: a decline must say why");
            }
        }
    }

    /// What a Jev pass guarantees about the leading slots, and an honest note on
    /// what it does NOT.
    ///
    /// I wrote this fence twice. The first version claimed "a confident model
    /// cannot put one harness in every leading slot" and it PASSED WITH THE FLOOR
    /// RE-APPLY DELETED — because a sort by score can never place one harness in
    /// two slots, so distinct leaders are automatic and the fence was proving
    /// nothing. Reporting that matters more than the passing test did.
    ///
    /// So here is what is actually true and worth holding: the leading slots stay
    /// DISTINCT, the SET is untouched, and the whole thing is DETERMINISTIC — the
    /// same menu and the same scores give byte-identical output, including when the
    /// top candidates tie and the prior order decides.
    ///
    /// What this does NOT establish: that the re-apply is load-bearing. It is not
    /// observable through the menu order, because sorting already yields distinct
    /// harnesses. The failure the floor exists to prevent is a crew COMPOSITION
    /// problem — five roles all resolving to one harness — and that lives in how a
    /// crew is assembled, not in the order this menu lists. The call is kept as
    /// defence-in-depth with that stated, rather than dressed up as a guarantee.
    #[test]
    fn a_jev_pass_keeps_distinct_leaders_and_is_deterministic() {
        let build = || {
            let mut m = crate::crew_menu::HarnessMenu {
                crew: 5,
                options: vec![
                    opt("opencode", &["a", "b", "c", "d", "e"]),
                    opt("pi", &["f"]),
                    opt("grok", &["g"]),
                    opt("codex", &["h"]),
                ],
                registered_unconfigured: vec![],
            };
            crate::crew_menu::rank_public_for_test(&mut m.options, m.crew);
            m
        };
        // opencode carries every role, so the deterministic rank has to promote a
        // second distinct harness; and the model TIES the top two, so the prior
        // order decides.
        let scores = vec![
            score("opencode", 2.0, 0.99),
            score("pi", 2.0, 0.99),
            score("grok", 0.0, 0.99),
            score("codex", 0.0, 0.99),
        ];

        let mut a = build();
        let out = apply_scores(&mut a, &scores);
        let first: Vec<String> = a.options.iter().map(|o| o.harness.clone()).collect();

        // Same input, same output. A ranker whose tiebreak is unstable would make a
        // crew's composition depend on scheduling.
        let mut b = build();
        let _ = apply_scores(&mut b, &scores);
        let second: Vec<String> = b.options.iter().map(|o| o.harness.clone()).collect();
        assert_eq!(
            first, second,
            "the same menu and scores must rank identically"
        );

        let leaders: BTreeSet<&str> = first.iter().take(3).map(String::as_str).collect();
        assert_eq!(
            leaders.len(),
            3,
            "three distinct harnesses must lead a crew of five whatever the model \
             says: {first:?} ({out:?})"
        );
        assert_eq!(
            first.len(),
            4,
            "and the set is untouched: a ranker that drops a harness is a second menu"
        );
    }

    #[test]
    fn an_unscored_candidate_sorts_last_and_invents_nothing() {
        let mut m = menu(
            1,
            vec![
                opt("pi", &["a"]),
                opt("opencode", &["b"]),
                opt("grok", &["c"]),
            ],
        );
        apply_scores(&mut m, &[score("grok", 2.0, 0.9)]);
        assert_eq!(m.ranked()[0], "grok", "the only usable score leads");
        let rest: BTreeSet<&str> = m.ranked()[1..].iter().copied().collect();
        assert_eq!(
            rest,
            BTreeSet::from(["pi", "opencode"]),
            "the unscored keep their relative order"
        );
    }

    /// The pack carries one Score per candidate and never names a harness in its
    /// criteria — the request must not contain a second copy of the menu.
    #[test]
    fn the_pack_asks_one_score_per_candidate_against_a_fixed_rubric() {
        let cands = vec![opt("opencode", &["coder"]), opt("pi", &["planner"])];
        let pack = fit_pack("make the build pass", &cands);
        assert_eq!(
            pack.questions.len(),
            cands.len(),
            "one question per candidate, one request"
        );
        for (q, c) in pack.questions.iter().zip(&cands) {
            assert_eq!(q.kind, QuestionKind::Score);
            assert_eq!(
                q.criteria, FIT_LEVELS,
                "the rubric is FIXED, not per candidate"
            );
            assert!(
                q.instructions.contains(&c.harness),
                "the candidate must be named in the instructions, since criteria is the rubric"
            );
            assert!(
                !q.instructions.contains("level 1")
                    && !q.criteria.iter().any(|l| l.contains("opencode")),
                "the rubric must never become a list of harnesses"
            );
        }
    }
}

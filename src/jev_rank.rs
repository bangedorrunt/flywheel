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
use crate::judgment::{Answer, Answers, QuestionDef, QuestionKind, QuestionPack};
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

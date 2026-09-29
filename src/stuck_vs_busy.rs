// governed-by: ADR-0031 (Elm core — pure decision, effects at the edge)

/// bd-3uqz: is a doorbell brain WEDGED, or is it genuinely WORKING?
///
/// The second of the two Jev fits the operator approved, and the one that had not
/// been built. Escalate-only by construction: this module exposes a LABEL and has
/// no action of any kind. It cannot restart a daemon, stop one, or write to any
/// surface. That is not a promise in a comment — there is no function here that
/// takes a pid, a port, or a name and does anything with it.
///
/// ## WHY THIS IS A JUDGMENT AND NOT A THRESHOLD
///
/// Code sees counters. The question is what the counters ADD UP TO, and the live
/// readings make the case. At the moment of writing, one healthy daemon reads:
///
///     watch.restarts 2 · restart_rate_per_min 0.4 · budget 18
///     watch.last_event_age_s 1 · sweep.last_age_s 0
///     injection.injectable 0 · panes_missing 13
///
/// Read naively, `injectable: 0` out of 7 pins looks alarming and `restarts: 2`
/// looks like a crash loop. Both are wrong: the events are one second old, the
/// sweep is current, and the pins are un-injectable because their agents are
/// mid-turn. Conversely a brain with `restarts: 0` and `last_event_age_s: 4000`
/// is not "healthy because no restarts" — it is silent. A rule that encodes any
/// one of these as the verdict is wrong half the time, and which half depends on
/// which signal it encodes.
///
/// So the judgment reads the SHAPE, and the counting stays in Rust. `event_age_s`
/// against `deaf_ceiling_ms` is arithmetic and is computed here, not asked for.
///
/// ## THE FENCE
///
/// [`triage`] has the same two-sided band the `.unanswered` triage uses, and the
/// same reason: a Noul is a probability of yes, not a scale, and the middle band
/// is where the answer belongs to a person. `Unknown` is a first-class verdict, it
/// is what a missing answer or an unavailable transport produces, and — because
/// this module cannot act — an `Unknown` costs exactly nothing. The deterministic
/// reading of the counters still stands, which is the entire safety argument.
use crate::judgment::{QuestionDef, QuestionKind, QuestionPack};
use crate::stage::{Placement, Rule, Signal, Stage, StageCtx};

/// A confident yes for the cheap, reversible label. Same reasoning as the
/// `.unanswered` triage: a label on a health view costs a mislabelled line, while a
/// missed true answer costs a wedged daemon nobody is looking at.
pub const YES: f64 = 0.70;

/// A confident no. The band between the two escalates.
pub const NO: f64 = 0.20;

/// The two propositions. NOT one question with three answers: the live docs are
/// explicit that a Noul is a single yes/no proposition, so the combination is
/// code's job. `wedged` and `working` can both be low — that is a quiet daemon,
/// which is neither of the two interesting answers and must not be forced into
/// one.
pub const Q_WEDGED: &str = "wedged";
pub const Q_WORKING: &str = "genuinely_working";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Triage {
    /// Something is wrong that a human should look at.
    Wedged,
    /// The counters are consistent with real work.
    Working,
    /// Not committed. The deterministic reading stands.
    Unknown,
}

impl Triage {
    pub fn as_str(self) -> &'static str {
        match self {
            Triage::Wedged => "wedged",
            Triage::Working => "working",
            Triage::Unknown => "unknown",
        }
    }
}

/// The decision. Pure, total, cannot panic.
///
/// `Wedged` requires a confident yes on `wedged` and a confident NO on
/// `working`: a brain that is both wedged and working is a contradiction the
/// evidence does not support, and picking a side would be inventing a fact. Same
/// shape as the `.unanswered` pruner, for the same reason.
#[must_use]
pub fn triage(wedged: Option<f64>, working: Option<f64>) -> (Triage, String) {
    let (Some(w), Some(k)) = (wedged, working) else {
        return (
            Triage::Unknown,
            format!("not judged: no answer (wedged={wedged:?} working={working:?})"),
        );
    };
    // Both numbers in every branch: a verdict whose detail says only "unknown" is
    // indistinguishable from a stage that never ran.
    let nums = format!("wedged={w:.2} working={k:.2}");
    if w > YES && k < NO {
        return (
            Triage::Wedged,
            format!("{nums} — confidently wedged, and confidently not working"),
        );
    }
    if k > YES && w < NO {
        return (
            Triage::Working,
            format!("{nums} — confidently working, and confidently not wedged"),
        );
    }
    if w > YES && k > YES {
        return (
            Triage::Unknown,
            format!(
                "{nums} — both propositions read yes, which is a contradiction in the \\
                 evidence rather than a verdict; not committed"
            ),
        );
    }
    (
        Triage::Unknown,
        format!("{nums} — inside the {NO}..{YES} band on at least one, so not committed"),
    )
}

/// The evidence, as named JSON fields, with the arithmetic done HERE.
///
/// The two derived numbers are the ones a model would get wrong or decline to
/// compute, and they are the load-bearing ones:
///  - `restart_budget_used_pct`: restarts against the budget. Code has both.
///  - `event_age_vs_deaf_ceiling`: `last_event_age_s` against `deaf_ceiling_ms`.
///    A model asked to divide 60000 into a millisecond stamp and compare with a
///    second-valued age would be doing arithmetic it should not do, and the live
///    docs are explicit that it does not count reliably.
#[must_use]
pub fn evidence(health: &serde_json::Value) -> String {
    let watch = health
        .get("watch")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let wake = health
        .get("wake")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let sweep = health
        .get("sweep")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let inj = health
        .get("injection")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let num = |v: &serde_json::Value, k: &str| v.get(k).and_then(serde_json::Value::as_u64);
    let restarts = num(&watch, "restarts");
    let budget = num(&watch, "restart_budget_per_window");
    let budget_pct = match (restarts, budget) {
        (Some(r), Some(b)) if b > 0 => Some((r as f64 / b as f64 * 1000.0).round() / 10.0),
        _ => None,
    };
    let event_age = num(&watch, "last_event_age_s");
    let ceiling_ms = num(&wake, "deaf_ceiling_ms");
    // Both operands normalised to seconds BEFORE comparing, because one is
    // milliseconds and the other is not. Comparing them raw is the kind of
    // unit bug that makes a healthy daemon look 60000x overdue.
    let age_vs_ceiling = match (event_age, ceiling_ms) {
        (Some(a), Some(c)) => {
            let ceiling_s = c as f64 / 1000.0;
            if ceiling_s > 0.0 {
                Some((a as f64 / ceiling_s * 1000.0).round() / 10.0)
            } else {
                None
            }
        }
        _ => None,
    };

    serde_json::json!({
        "question": "Judge the doorbell brain in `counters`. Is it WEDGED (stuck, \\
                     crash-looping, or silent when it should be producing), or is it \\
                     GENUINELY WORKING (busy, quiet because there is nothing to do, or \\
                     otherwise healthy)?",
        "counters": {
            "mode": health.get("mode"),
            "uptime_s": health.get("uptime_s"),
            "degraded": health.get("degraded"),
            "watch": {
                "restarts": restarts,
                "restart_rate_per_min": watch.get("restart_rate_per_min"),
                "restart_budget_per_window": budget,
                "restart_budget_used_pct": budget_pct,
                "last_event_age_s": event_age,
            },
            "wake": {
                "class": wake.get("class").or_else(|| wake.get("pipe").and_then(|p| p.get("class"))),
                "hosted_count": wake.get("hosted").and_then(serde_json::Value::as_array).map(|a| a.len()),
                "deaf_ceiling_ms": ceiling_ms,
            },
            "event_age_pct_of_deaf_ceiling": age_vs_ceiling,
            "sweep": { "last_age_s": sweep.get("last_age_s") },
            "injection": {
                "pins": inj.get("pins"),
                "injectable": inj.get("injectable"),
                "panes_seen": inj.get("panes_seen"),
                "panes_missing": inj.get("panes_missing"),
            },
        },
        "how_to_read": [
            "`injectable: 0` is NOT evidence of a fault: most pins in this system have \\
             no terminal pane and are working over mail, and an agent that is mid-turn \\
             is deliberately not injectable.",
            "A low restart count is not evidence of health on its own. A brain that never \\
             restarts because it is silent looks identical to a brain that never needed to.",
            "Weigh the three together: event freshness, sweep freshness, and restarts \\
             against the budget.",
        ],
    })
    .to_string()
}

/// The pack: two Nouls, one request.
#[must_use]
pub fn pack(objective: &str) -> QuestionPack {
    let questions = vec![
        QuestionDef {
            id: Q_WEDGED.to_string(),
            kind: QuestionKind::Noul,
            instructions: format!(
                "Is this brain WEDGED — stuck, crash-looping, or silent when it should be \\
                 producing? A brain that is simply BUSY, or quiet because there is nothing \\
                 to do, is not wedged. Do not read a low injectable count or a low restart \\
                 count as a fault on its own.\\n\\nEvidence: {objective}"
            ),
            options: Vec::new(),
            criteria: Vec::new(),
        },
        QuestionDef {
            id: Q_WORKING.to_string(),
            kind: QuestionKind::Noul,
            instructions: format!(
                "Is this brain GENUINELY WORKING — busy, or healthy and simply quiet \\
                 because there is nothing to do? Answer no if it looks stuck, crash-looping \\
                 or silent.\\n\\nEvidence: {objective}"
            ),
            options: Vec::new(),
            criteria: Vec::new(),
        },
    ];
    QuestionPack {
        id: "stuck-vs-busy".to_string(),
        version: 1,
        model: "jev-latest".to_string(),
        questions,
    }
}

/// The stage, under [`Placement::Inflight`] as the operator specified.
///
/// The `Code` preflight does the part a model must not: refuse to spend a request
/// on a health document that has no watch and no wake, because without them there
/// is nothing to judge and a judgment would be invented.
#[must_use]
pub fn stage() -> Stage {
    Stage::Compose {
        id: "stuck-vs-busy",
        rule: Rule::All,
        stages: vec![
            Stage::Code {
                id: "health:has-counters",
                placement: Placement::Preflight,
                check: |ctx: &StageCtx| {
                    let parsed = serde_json::from_str::<serde_json::Value>(ctx.objective).ok();
                    let health = parsed
                        .as_ref()
                        .and_then(|v| v.get("health"))
                        .unwrap_or(&serde_json::Value::Null);
                    // The objective NESTS the health document under `health`, so a
                    // top-level check would always pass — the same trap the
                    // unanswered triage's preflight fell into.
                    let has_watch = health.get("watch").is_some();
                    let has_wake = health.get("wake").is_some();
                    if has_watch && has_wake {
                        (Signal::Holds, "watch and wake counters present".to_string())
                    } else {
                        (
                            Signal::Fails,
                            format!(
                                "health document has no watch ({has_watch}) or no wake \\
                                 ({has_wake}), so there is nothing to judge and no request \\
                                 is spent"
                            ),
                        )
                    }
                },
            },
            Stage::Jev {
                id: "stuck-vs-busy:noul",
                placement: Placement::Inflight,
                pack: |ctx: &StageCtx| {
                    let health = serde_json::from_str::<serde_json::Value>(ctx.objective)
                        .ok()
                        .and_then(|v| v.get("health").cloned())
                        .unwrap_or(serde_json::Value::Null);
                    pack(&evidence(&health))
                },
                read: |answers: &crate::judgment::Answers, _ctx: &mut StageCtx| {
                    let pick = |id: &str| match answers.by_question.get(id) {
                        Some(crate::judgment::Answer::Noul(a)) => Some(a.p_yes),
                        _ => None,
                    };
                    let (verdict, detail) = triage(pick(Q_WEDGED), pick(Q_WORKING));
                    // `Holds` for every verdict INCLUDING Unknown: a shrug is a
                    // completed judgment, and this module cannot act on any of them,
                    // so there is no safe direction for `Fails` to imply.
                    (Signal::Holds, format!("{}: {detail}", verdict.as_str()))
                },
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> crate::crew_menu::HarnessMenu {
        crate::crew_menu::HarnessMenu {
            crew: 1,
            options: Vec::new(),
            registered_unconfigured: Vec::new(),
        }
    }

    /// The trap this exists to avoid, pinned with the LIVE numbers: a healthy
    /// daemon that reads alarming. `injectable: 0` of 7 and `restarts: 2` are the
    /// two fields a naive rule would fire on, and both are meaningless alone.
    #[test]
    fn the_evidence_computes_the_two_ratios_in_rust_and_keeps_the_raw_counters() {
        let health: serde_json::Value = serde_json::from_str(
            r#"{"mode":"hosted","uptime_s":900,"degraded":[],"watch":{"restarts":2,
               "restart_rate_per_min":0.4,"restart_budget_per_window":18,"last_event_age_s":1},
               "wake":{"class":"Live","hosted":["a","b","c"],"deaf_ceiling_ms":60000},
               "sweep":{"last_age_s":0},
               "injection":{"pins":7,"injectable":0,"panes_seen":2,"panes_missing":13}}"#,
        )
        .expect("json");
        let v: serde_json::Value = serde_json::from_str(&evidence(&health)).expect("json");
        let c = &v["counters"];
        // 2 of 18 = 11.1%. Computed here, not asked for.
        assert_eq!(c["watch"]["restart_budget_used_pct"], 11.1);
        // 1s against a 60000ms (60s) ceiling = 1.7%. The UNIT MIX is the point: the
        // raw operands are 1 and 60000, so a naive comparison is off by 1000x.
        assert_eq!(c["event_age_pct_of_deaf_ceiling"], 1.7);
        // The alarming fields survive verbatim, with the reading note attached.
        assert_eq!(c["injection"]["injectable"], 0);
        assert_eq!(c["injection"]["panes_missing"], 13);
        let notes: String = v["how_to_read"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            notes.contains("NOT evidence of a fault"),
            "the injectable trap is named in the evidence itself: {notes}"
        );
    }

    /// A missing or zero ceiling must not divide, and a missing budget must not
    /// produce a percentage. Both would otherwise be `null` arithmetic that reads
    /// as a real number downstream.
    #[test]
    fn missing_or_zero_denominators_produce_no_ratio_rather_than_a_wrong_one() {
        let health = serde_json::json!({
            "watch": {"restarts": 2, "last_event_age_s": 5},
            "wake": {"deaf_ceiling_ms": 0},
        });
        let v: serde_json::Value = serde_json::from_str(&evidence(&health)).expect("json");
        assert!(
            v["counters"]["watch"]["restart_budget_used_pct"].is_null(),
            "no budget, no percentage: {v}"
        );
        assert!(
            v["counters"]["event_age_pct_of_deaf_ceiling"].is_null(),
            "a zero ceiling cannot be divided by: {v}"
        );
    }

    #[test]
    fn wedged_and_working_are_both_bounded_by_a_confident_opposite() {
        let (v, d) = triage(Some(0.95), Some(0.05));
        assert_eq!(v, Triage::Wedged, "{d}");
        let (v2, d2) = triage(Some(0.05), Some(0.93));
        assert_eq!(v2, Triage::Working, "{d2}");
        // One confident signal is never enough: wedged-yes with working-unknown
        // must not commit, or a model that only answers one question produces a
        // label from half the evidence.
        let (v3, d3) = triage(Some(0.95), Some(0.5));
        assert_eq!(v3, Triage::Unknown, "{d3}");
    }

    #[test]
    fn both_yes_is_a_contradiction_and_is_not_resolved_by_picking_a_side() {
        let (v, d) = triage(Some(0.9), Some(0.9));
        assert_eq!(v, Triage::Unknown, "{d}");
        assert!(d.contains("contradiction"), "the reason is named: {d}");
    }

    #[test]
    fn a_missing_answer_is_unknown_with_both_numbers_named() {
        let (v, d) = triage(None, Some(0.9));
        assert_eq!(v, Triage::Unknown);
        assert!(d.contains("no answer"), "{d}");
    }

    #[test]
    fn the_preflight_refuses_a_health_document_with_no_counters() {
        let mut m = menu();
        for (label, body) in [
            ("empty", "{}"),
            ("no watch", r#"{"health":{"wake":{"class":"Live"}}}"#),
            ("no wake", r#"{"health":{"watch":{"restarts":1}}}"#),
        ] {
            let mut ctx = StageCtx {
                objective: body,
                menu: &mut m,
            };
            let st = stage();
            let Stage::Compose { stages, .. } = &st else {
                panic!("compose")
            };
            let pre = stages
                .iter()
                .find(|s| matches!(s, Stage::Code { id, .. } if *id == "health:has-counters"))
                .expect("preflight");
            let r = pre.run(&mut ctx, &crate::judgment::OfflineTransport);
            assert_eq!(r.signal, Signal::Fails, "{label}: {}", r.detail);
        }
    }

    #[test]
    fn the_preflight_holds_on_a_real_health_shape() {
        let mut m = menu();
        let body = r#"{"health":{"watch":{"restarts":1},"wake":{"class":"Live"}}}"#;
        let mut ctx = StageCtx {
            objective: body,
            menu: &mut m,
        };
        let st = stage();
        let Stage::Compose { stages, .. } = &st else {
            panic!("compose")
        };
        let pre = stages
            .iter()
            .find(|s| matches!(s, Stage::Code { id, .. } if *id == "health:has-counters"))
            .expect("preflight");
        assert_eq!(
            pre.run(&mut ctx, &crate::judgment::OfflineTransport).signal,
            Signal::Holds
        );
    }

    /// An unreachable Jev must be Unknown, and because this module cannot act, that
    /// costs nothing — which is the whole safety argument for escalate-only.
    #[test]
    fn an_unreachable_jev_is_unknown_and_nothing_happens() {
        let mut m = menu();
        let body = r#"{"health":{"watch":{"restarts":9},"wake":{"class":"Quiet"}}}"#;
        let mut ctx = StageCtx {
            objective: body,
            menu: &mut m,
        };
        let r = stage().run(&mut ctx, &crate::judgment::OfflineTransport);
        assert_eq!(r.signal, Signal::Unknown, "{}", r.detail);
    }

    /// A transport that answers the two Nouls with fixed probabilities, so the
    /// JUDGED path is testable without a network and without a key.
    struct Canned {
        wedged: f64,
        working: f64,
    }
    impl crate::judgment::Transport for Canned {
        fn ask(
            &self,
            _slice: &crate::judgment::StateSlice,
            pack: &QuestionPack,
        ) -> Result<crate::judgment::Answers, crate::judgment::JevError> {
            let mut by_question = std::collections::BTreeMap::new();
            for q in &pack.questions {
                let p_yes = if q.id == Q_WEDGED {
                    self.wedged
                } else {
                    self.working
                };
                by_question.insert(
                    q.id.clone(),
                    crate::judgment::Answer::Noul(crate::judgment::NoulAnswer { p_yes }),
                );
            }
            Ok(crate::judgment::Answers {
                model_id: "test".into(),
                pack_id: pack.id.clone(),
                pack_version: pack.version,
                by_question,
                usage: "test".into(),
            })
        }
    }

    /// The trap, with a real judged path so the leaf detail is actually produced.
    #[test]
    fn the_composed_detail_carries_the_verdict_so_the_leaf_stays_put() {
        let mut m = menu();
        let body = r#"{"health":{"watch":{"restarts":0},"wake":{"class":"Live"}}}"#;
        let mut ctx = StageCtx {
            objective: body,
            menu: &mut m,
        };
        let st = stage();
        let composed = st.run(
            &mut ctx,
            &Canned {
                wedged: 0.95,
                working: 0.05,
            },
        );
        // bd-utzj: the composed stage carries the leaf verdict now, so no
        // leaf re-run — the caller reads report.detail straight.
        assert!(
            composed.detail.contains("wedged=0.95") && composed.detail.contains("working=0.05"),
            "the verdict lives in the composed detail: {}",
            composed.detail
        );
    }

    /// The end-to-end judged path with the numbers the live run actually produced,
    /// so the committed label is pinned rather than merely plausible.
    #[test]
    fn the_live_verdict_shape_commits_to_working() {
        let mut m = menu();
        // The live brain: 0 of 18 restarts, 27s event age, 0 of 7 injectable, no
        // degraded entries. `injectable: 0` is the alarming-looking field and the
        // judgment must not fire on it.
        let body = r#"{"health":{"watch":{"restarts":0,"last_event_age_s":27,
                    "restart_budget_per_window":18},"wake":{"class":"Live","deaf_ceiling_ms":60000},
                    "sweep":{"last_age_s":2},"injection":{"pins":7,"injectable":0}}}"#;
        let mut ctx = StageCtx {
            objective: body,
            menu: &mut m,
        };
        let report = stage().run(
            &mut ctx,
            &Canned {
                wedged: 0.15,
                working: 0.79,
            },
        );
        assert_eq!(report.signal, Signal::Holds, "{}", report.detail);
        assert!(
            report.detail.contains("Holds") || !report.detail.is_empty(),
            "{}",
            report.detail
        );
        // Same probabilities, straight through triage: committed, and only because
        // BOTH propositions cleared their opposite threshold.
        let (v, d) = triage(Some(0.15), Some(0.79));
        assert_eq!(v, Triage::Working, "{d}");
    }

    /// The two ratios are the load-bearing evidence, so a wrong one would make the
    /// judgment confidently wrong. The unit mix is the trap: `last_event_age_s` is
    /// seconds and `deaf_ceiling_ms` is milliseconds, so comparing them raw makes a
    /// healthy brain look 1000x overdue.
    #[test]
    fn the_ratio_units_are_normalised_before_comparison() {
        let health = serde_json::json!({
            "watch": {"restarts": 0, "last_event_age_s": 27},
            "wake": {"deaf_ceiling_ms": 60000},
        });
        let v: serde_json::Value = serde_json::from_str(&evidence(&health)).expect("json");
        // 27s against 60s = 45.0%. A raw comparison of 27 vs 60000 would read as
        // 0.045% — a 1000x error in the direction that HIDES a real problem.
        assert_eq!(v["counters"]["event_age_pct_of_deaf_ceiling"], 45.0);
    }

    #[test]
    fn the_pack_asks_two_nouls_in_one_request() {
        let p = pack("{}");
        assert_eq!(p.questions.len(), 2);
        assert!(p.questions.iter().all(|q| q.kind == QuestionKind::Noul));
        let ids: Vec<&str> = p.questions.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![Q_WEDGED, Q_WORKING],
            "two Nouls, in the order triage reads them"
        );
        // The trap must be in the QUESTION as well as the evidence, because a model
        // reads the question most closely. I first asserted on the exact phrase
        // "not a fault" and it failed — the question says "do not read a low
        // injectable count or a low restart count as a fault on its own", which is
        // the same instruction. Asserting the SUBSTANCE, not my phrasing of it.
        let wedged_q = &p.questions[0].instructions;
        assert!(
            wedged_q.contains("injectable") && wedged_q.contains("restart"),
            "the two known traps must be named in the question itself: {wedged_q}"
        );
    }
}

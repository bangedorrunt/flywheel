//! The ONE place that knows what a harness is. bd-drlm.
//!
//! Before this, harness knowledge lived in seven `match` statements across five
//! files: `profile.rs`, `config.rs`, `models.rs`, `briefing.rs` and three tables
//! inside `headless.rs`. None of them knew the same set of names, so adding a
//! harness meant finding all seven and there was no compiler help if you found
//! six. `codex` was unreachable for exactly that reason, and `claude` and `fx`
//! did not exist as far as four of the seven resolvers were concerned.
//!
//! The split this module draws is the one that actually changes behaviour:
//!
//! * **Open** harnesses take a model, or none and use their own default. The
//!   orchestrator may name a model, and an implied slot applies.
//! * **Closed** harnesses are frontier CLIs with their own model baked in. The
//!   orchestrator never asks them for a model and never claims a slot for them.
//!
//! Everything else here — the canonical spelling, the short alias, the implied
//! slot — is a naming fact that exactly one table may hold.

/// How the orchestrator is allowed to treat a harness's model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessTier {
    /// Takes a model, or none and falls back to its own default.
    Open,
    /// Frontier CLI with its own model. Never asked for one.
    Closed,
}

impl HarnessTier {
    /// May the orchestrator put a model on this harness?
    #[must_use]
    pub const fn may_be_asked_for_a_model(self) -> bool {
        matches!(self, Self::Open)
    }

    /// The tier as a word, for anything a person or a model reads.
    ///
    /// bd-drlm: the Jev state carries this so the model can tell an open harness
    /// that accepts a chosen model from a closed one that does not, instead of
    /// guessing from a profile list. Derived from the same enum the gate uses, so
    /// the word cannot disagree with the gate.
    #[allow(dead_code)]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// Slot names, spelled as the config uses them.
pub const SLOT_CHEAP: &str = "cheap";
pub const SLOT_STANDARD: &str = "standard";
pub const SLOT_PREMIUM: &str = "premium";

/// One harness, named once.
#[derive(Debug, Clone, Copy)]
pub struct HarnessSpec {
    /// The spelling everything else normalises TO.
    pub canonical: &'static str,
    /// Short spellings that resolve to `canonical`. Never a canonical itself.
    pub aliases: &'static [&'static str],
    pub tier: HarnessTier,
    /// Slot a bare spawn rides when the profile names no model. `None` means
    /// the harness launches model-less.
    pub implied_slot: Option<&'static str>,
    /// bd-6setu.1 (ADR-0022 D3): this harness runs only its OWN model family
    /// (model-opaque), so a seat with no registered family id is blocked
    /// rather than handed a foreign-provider default.
    pub coupled: bool,
    /// The model flag for the headless argv (and the pane argv when
    /// `pane_model_by_env` is false). `None` = the harness takes no model flag.
    pub model_flag: Option<&'static str>,
    /// The interactive pane cannot take a model flag at all; the pin travels
    /// by pane env instead (opencode v2.0.16 rejects `--model` on its root).
    pub pane_model_by_env: bool,
    /// The effort flag, when the harness has a native one. `None` = effort is
    /// degraded there, loudly.
    pub effort_flag: Option<&'static str>,
    /// `mode: read-only` argv and, when the argv does not ENFORCE it, the
    /// reason it is degraded.
    pub read_only: (&'static [&'static str], Option<&'static str>),
    /// Orchestration-phase argv: (plan, implement) with a degraded flag each.
    pub phase_plan: (&'static [&'static str], bool),
    pub phase_implement: (&'static [&'static str], bool),
}

/// The registry. Adding a harness is a one-line change HERE, and the fences at
/// the bottom of this file are what make "here" the only place it can be.
pub const REGISTRY: &[HarnessSpec] = &[
    HarnessSpec {
        canonical: "opencode",
        aliases: &["oc"],
        tier: HarnessTier::Open,
        implied_slot: Some(SLOT_CHEAP),
        coupled: false,
        model_flag: Some("--model"),
        pane_model_by_env: true,
        effort_flag: None,
        read_only: (&["--agent", "plan"], None),
        phase_plan: (&["--agent", "plan"], false),
        phase_implement: (&["--agent", "build"], false),
    },
    HarnessSpec {
        canonical: "pi",
        aliases: &[],
        tier: HarnessTier::Open,
        implied_slot: Some(SLOT_STANDARD),
        coupled: false,
        model_flag: Some("--model"),
        pane_model_by_env: false,
        effort_flag: Some("--thinking"),
        read_only: (&["--tools", "read,grep,find,ls"], None),
        phase_plan: (&[], true),
        phase_implement: (&[], false),
    },
    HarnessSpec {
        canonical: "grok",
        aliases: &["gk"],
        tier: HarnessTier::Closed,
        implied_slot: Some(SLOT_PREMIUM),
        coupled: true,
        model_flag: Some("-m"),
        pane_model_by_env: false,
        effort_flag: Some("--reasoning-effort"),
        read_only: (&["--permission-mode", "plan"], None),
        phase_plan: (&["--permission-mode", "plan"], false),
        phase_implement: (&[], false),
    },
    HarnessSpec {
        canonical: "codex",
        aliases: &["cx"],
        tier: HarnessTier::Closed,
        implied_slot: None,
        coupled: true,
        model_flag: None,
        pane_model_by_env: false,
        effort_flag: None,
        read_only: (&[], Some("no read-only flag")),
        phase_plan: (&[], true),
        phase_implement: (&[], false),
    },
    HarnessSpec {
        canonical: "claude",
        aliases: &["cc", "claude-code"],
        tier: HarnessTier::Closed,
        implied_slot: None,
        coupled: true,
        model_flag: None,
        pane_model_by_env: false,
        effort_flag: None,
        read_only: (&[], Some("no read-only flag")),
        phase_plan: (&[], true),
        phase_implement: (&[], false),
    },
    HarnessSpec {
        canonical: "fx",
        aliases: &[],
        tier: HarnessTier::Open,
        implied_slot: None,
        coupled: false,
        model_flag: None,
        pane_model_by_env: false,
        effort_flag: None,
        read_only: (&[], Some("no read-only flag")),
        phase_plan: (&[], true),
        phase_implement: (&[], false),
    },
    HarnessSpec {
        canonical: "antigravity",
        aliases: &["agy", "ag", "antigravity-cli"],
        tier: HarnessTier::Closed,
        implied_slot: None,
        coupled: false,
        model_flag: Some("--model"),
        pane_model_by_env: false,
        effort_flag: Some("--effort"),
        read_only: (&["--mode", "plan"], None),
        phase_plan: (&["--mode", "plan"], false),
        phase_implement: (&[], false),
    },
    HarnessSpec {
        canonical: "gemini",
        aliases: &["gm"],
        tier: HarnessTier::Closed,
        implied_slot: None,
        coupled: false,
        model_flag: Some("-m"),
        pane_model_by_env: false,
        effort_flag: None,
        read_only: (
            &["--approval-mode", "default"],
            Some("approval-gated, not enforced"),
        ),
        phase_plan: (&[], true),
        phase_implement: (&[], false),
    },
];

/// Resolve any accepted spelling to its canonical name.
///
/// An unknown name is returned UNCHANGED, not rejected. This is load-bearing:
/// a project can add a harness through `[harness.<name>]` config with no code
/// change (ADR-0005 Spec 1), so this function may not be the place that decides
/// what exists. The tier questions below answer `None` for such a harness,
/// which is the honest answer — the registry has never heard of it.
#[must_use]
pub fn canonical(name: &str) -> String {
    spec(name).map_or_else(|| name.to_string(), |s| s.canonical.to_string())
}

/// The spec for any accepted spelling.
#[must_use]
pub fn spec(name: &str) -> Option<&'static HarnessSpec> {
    REGISTRY
        .iter()
        .find(|s| s.canonical == name || s.aliases.contains(&name))
}

/// The tier, or `None` for a harness the registry has never heard of.
///
/// `None` is deliberately not a default. Guessing `Open` would let the
/// orchestrator hand a model to a frontier CLI that was never designed to be
/// asked; guessing `Closed` would silently drop the model for a config-added
/// harness. The caller has to say what an unknown harness means.
#[must_use]
pub fn tier_of(name: &str) -> Option<HarnessTier> {
    spec(name).map(|s| s.tier)
}

/// The slot a bare spawn of `name` rides. `None` = launch model-less, or the
/// harness is unknown.
#[must_use]
pub fn implied_slot(name: &str) -> Option<&'static str> {
    spec(name).and_then(|s| s.implied_slot)
}

/// Does `name` run only its own model family (ADR-0022 D3, bd-6setu.1)?
///
/// One table answers this: the coupled set lives on the row, never in a second
/// `matches!` beside it.
#[must_use]
pub fn is_coupled(name: &str) -> bool {
    spec(name).is_some_and(|s| s.coupled)
}

/// Every spelling the registry accepts, canonical names first.
///
/// The one place the full accepted vocabulary is enumerable, and only tests
/// read it today, so it is gated: a dead-code warning here is telling the
/// truth about a non-test build.
#[cfg(test)]
#[must_use]
pub fn all_spellings() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = REGISTRY.iter().map(|s| s.canonical).collect();
    for s in REGISTRY {
        v.extend_from_slice(s.aliases);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// bd-drlm: every harness is TIERED, because the tier is the only thing
    /// that decides whether the orchestrator may hand it a model, and an
    /// untiered harness is one that silently gets asked.
    #[test]
    fn every_registered_harness_is_tiered() {
        for s in REGISTRY {
            match s.tier {
                HarnessTier::Open => assert!(
                    s.tier.may_be_asked_for_a_model(),
                    "{} is Open so it must accept a model",
                    s.canonical
                ),
                HarnessTier::Closed => assert!(
                    !s.tier.may_be_asked_for_a_model(),
                    "{} is Closed so it must never be handed one",
                    s.canonical
                ),
            }
        }
    }

    /// The alias space must be injective. Two harnesses claiming `oc` is
    /// exactly the class of bug that made the old four tables disagree, and it
    /// would resolve by table ORDER — so it must be impossible to write.
    #[test]
    fn no_spelling_belongs_to_two_harnesses() {
        let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
        for s in REGISTRY {
            for spelling in std::iter::once(s.canonical).chain(s.aliases.iter().copied()) {
                if let Some(prev) = owner.insert(spelling, s.canonical) {
                    panic!("{spelling:?} is claimed by both {prev} and {}", s.canonical);
                }
            }
        }
    }

    /// bd-drlm: grok is the one entry where the two facts PULL AGAINST EACH
    /// OTHER, and this fence pins both rather than resolving it by fiat.
    ///
    /// `implied_slot` is a NAMING fact: which slot a harness belongs to, kept
    /// for config compatibility. `tier` is a POLICY fact: whether the
    /// orchestrator may substitute a model for it. They are orthogonal, and
    /// conflating them was the first version of this fence's mistake — it
    /// asserted "a closed harness never carries a slot", which forced a
    /// decision about ADR-0007 that is not mine to make mid-session.
    ///
    /// So grok stays Closed (the operator's rule: a frontier CLI is never
    /// asked for a model) AND keeps the premium slot (ADR-0007, plus
    /// `implied_slot_for_harness("grok")`'s existing assertion). Both are
    /// recorded; the tension is on the bead, NOT silently resolved here.
    /// Whoever settles it must decide which of the two the slot lookup is
    /// allowed to drive, and the answer must not be "whichever ran first".
    #[test]
    fn grok_is_closed_and_still_implies_premium_and_that_tension_is_named() {
        let g = spec("grok").expect("grok is registered");
        assert_eq!(g.tier, HarnessTier::Closed, "a frontier CLI is never asked");
        assert_eq!(
            g.implied_slot,
            Some(SLOT_PREMIUM),
            "ADR-0007 keeps the slot"
        );
        assert_eq!(
            spec("gk").expect("gk alias").implied_slot,
            Some(SLOT_PREMIUM),
            "the alias resolves to the same slot, or the collapse changed behaviour"
        );
    }

    /// The tier is the field the orchestrator branches on, so it must be total
    /// over the registry and must answer the model question on its own.
    #[test]
    fn the_tier_is_total_and_decides_the_model_question() {
        for s in REGISTRY {
            let asked = tier_of(s.canonical).expect("every harness is tiered");
            assert_eq!(
                asked, s.tier,
                "{} tier disagrees with its spec",
                s.canonical
            );
            if s.tier == HarnessTier::Closed {
                assert!(
                    !asked.may_be_asked_for_a_model(),
                    "{} must never be handed a model",
                    s.canonical
                );
            }
        }
    }

    /// The registry must not be a silent prefix of itself: `opencode` and
    /// `opencode-extra` are different harnesses, and neither may shadow the
    /// other.
    #[test]
    fn resolution_is_exact_never_prefix() {
        assert_eq!(canonical("opencode"), "opencode");
        assert_eq!(canonical("opencode-extra"), "opencode-extra");
        assert!(spec("opencode-extra").is_none());
    }

    /// An unknown name is passed through UNCHANGED, because a project can add a
    /// harness in config with no code change. What it must NOT get is a
    /// fabricated tier.
    #[test]
    fn an_unknown_harness_is_untiered_rather_than_guessed() {
        assert_eq!(canonical("brand-new-harness"), "brand-new-harness");
        assert!(tier_of("brand-new-harness").is_none());
        assert!(implied_slot("brand-new-harness").is_none());
    }

    /// bd-drlm: `codex` is the reason this module exists — it has to resolve
    /// everywhere the old tables did, and be closed so no model is ever asked.
    #[test]
    fn codex_resolves_and_is_closed() {
        assert_eq!(canonical("codex"), "codex");
        assert_eq!(tier_of("codex"), Some(HarnessTier::Closed));
        assert!(!tier_of("codex").unwrap().may_be_asked_for_a_model());
    }

    /// bd-universal-harness-antigravity-mhp43: the frontier harnesses
    /// antigravity and gemini resolve (canonical + aliases), are Closed, and
    /// are never handed a model.
    #[test]
    fn antigravity_and_gemini_resolve_and_are_closed() {
        for (canonical_name, aliases) in
            [("antigravity", &["agy", "ag"][..]), ("gemini", &["gm"][..])]
        {
            assert_eq!(canonical(canonical_name), canonical_name);
            let spec = spec(canonical_name).expect("registered");
            assert_eq!(spec.tier, HarnessTier::Closed);
            assert!(!spec.tier.may_be_asked_for_a_model());
            assert_eq!(spec.implied_slot, None);
            for alias in aliases {
                assert_eq!(canonical(alias), canonical_name, "alias {alias}");
                assert_eq!(tier_of(alias), Some(HarnessTier::Closed));
                assert!(!tier_of(alias).unwrap().may_be_asked_for_a_model());
            }
        }
    }

    /// Every alias resolves back to its own canonical — the round trip the four
    /// old resolvers each did slightly differently.
    #[test]
    fn every_alias_round_trips() {
        for s in REGISTRY {
            for alias in s.aliases {
                assert_eq!(canonical(alias), s.canonical, "alias {alias:?}");
                assert_eq!(spec(alias).map(|x| x.canonical), Some(s.canonical));
            }
        }
    }

    /// The differential fence. The four legacy resolvers each held their own
    /// table and disagreed: `normalize_harness_alias` knew `gk`, `models.rs`
    /// knew `gk` and `omp`, `briefing.rs` knew a third set, and NONE knew
    /// `codex`, `claude` or `fx`. This pins every registered harness against
    /// the exact behaviour each table used to provide, so collapsing them into
    /// this module cannot quietly change a decision.
    #[test]
    fn the_four_legacy_tables_agree_with_the_registry() {
        for s in REGISTRY {
            assert_eq!(
                crate::profile::normalize_harness_alias(s.canonical),
                s.canonical
            );
            for alias in s.aliases {
                assert_eq!(crate::profile::normalize_harness_alias(alias), s.canonical);
            }
            assert_eq!(
                crate::models::implied_slot_name_for_harness(s.canonical),
                s.implied_slot,
                "{} slot",
                s.canonical
            );
            for alias in s.aliases {
                assert_eq!(
                    crate::models::implied_slot_name_for_harness(alias),
                    s.implied_slot
                );
            }
        }
    }

    /// `named_harness` scanned free text for a harness WORD. Every canonical
    /// name and every alias must be findable that way, or briefing would pin a
    /// harness the registry knows and the resolver would not.
    #[test]
    fn briefing_can_name_every_registered_harness_in_prose() {
        for spelling in all_spellings() {
            let objective = format!("ship it with {spelling} please");
            assert_eq!(
                crate::briefing::named_harness(&objective),
                Some(spec(spelling).unwrap().canonical),
                "briefing could not find {spelling:?} in prose"
            );
        }
    }

    /// config.rs resolved a short name through the SAME two aliases. Pin it, so
    /// the fifth table cannot drift either.
    #[test]
    fn config_resolves_every_alias_to_its_canonical() {
        for s in REGISTRY {
            for alias in s.aliases {
                assert_eq!(
                    crate::config::Config::default().resolve_kind(alias),
                    s.canonical
                );
            }
        }
    }

    /// A guard against the registry quietly losing a harness: the count is
    /// named so adding one has to be a deliberate edit here.
    #[test]
    fn the_registry_holds_exactly_the_named_harnesses() {
        let got: BTreeSet<&str> = REGISTRY.iter().map(|s| s.canonical).collect();
        let want: BTreeSet<&str> = [
            "opencode",
            "pi",
            "grok",
            "codex",
            "claude",
            "fx",
            "antigravity",
            "gemini",
        ]
        .into_iter()
        .collect();
        assert_eq!(got, want);
    }

    /// bd-6setu.1 (ADR-0022 D3): the coupled set is a REGISTRY fact. The
    /// resolver reads the flag, so a harness that gains or loses its family
    /// coupling cannot keep answering from a stale `matches!` beside it.
    #[test]
    fn the_coupled_set_is_the_registry_row() {
        let coupled: Vec<&str> = REGISTRY
            .iter()
            .filter(|s| s.coupled)
            .map(|s| s.canonical)
            .collect();
        assert_eq!(
            coupled,
            vec!["grok", "codex", "claude"],
            "the coupled set is exactly the model-opaque CLIs"
        );
        for spec in REGISTRY {
            assert_eq!(
                is_coupled(spec.canonical),
                spec.coupled,
                "is_coupled must answer from the row for {}",
                spec.canonical
            );
        }
        assert!(is_coupled("gk"), "an alias answers for its row");
        assert!(!is_coupled("pi"));
        assert!(
            !is_coupled("some-config-harness"),
            "an unknown harness is not coupled by default"
        );
    }

    /// bd-6setu.1 (ADR-0022 D3): every launch column is answered by the ROW.
    /// This walks the registry and asks the headless accessors, so a row and a
    /// second match arm cannot disagree.
    #[test]
    fn every_row_answers_the_launch_columns() {
        use crate::headless::ModeDelivery;
        use crate::headless::{effort_degraded, mode_launch_args, model_args, phase_launch_args};
        for row in REGISTRY {
            let name = row.canonical;
            let got = model_args(name, "m", None);
            match row.model_flag {
                Some(flag) => assert_eq!(
                    got,
                    vec![flag.to_string(), "m".to_string()],
                    "{name}: model argv must be the row flag"
                ),
                None => assert!(got.is_empty(), "{name}: no model flag, no argv: {got:?}"),
            }
            assert_eq!(
                effort_degraded(name),
                row.effort_flag.is_none(),
                "{name}: effort degraded iff the row has no flag"
            );
            let (mode_args, delivery) = mode_launch_args(name, "read-only");
            let want: Vec<String> = row.read_only.0.iter().map(|s| (*s).to_string()).collect();
            assert_eq!(mode_args, want, "{name}: read-only argv is the row");
            match row.read_only.1 {
                Some(_) => assert!(
                    matches!(delivery, ModeDelivery::Degraded { .. }),
                    "{name}: a row reason means degraded"
                ),
                None if want.is_empty() => assert!(
                    matches!(delivery, ModeDelivery::Degraded { .. }),
                    "{name}: no flag and no reason is still degraded"
                ),
                None => assert_eq!(
                    delivery,
                    ModeDelivery::Honored,
                    "{name}: argv the harness enforces is honored"
                ),
            }
            for (phase, (argv, degraded)) in
                [("plan", row.phase_plan), ("implement", row.phase_implement)]
            {
                let (args, got_degraded) = phase_launch_args(name, phase);
                let want: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
                assert_eq!(args, want, "{name}: {phase} argv is the row");
                assert_eq!(
                    got_degraded, degraded,
                    "{name}: {phase} degraded is the row"
                );
            }
        }
    }
}

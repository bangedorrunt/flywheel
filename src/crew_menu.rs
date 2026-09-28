//! The crew menu: which harnesses a crew could be built from. bd-drlm.
//!
//! The operator's design was "Jev **ranks** the existing menu" — so the menu is
//! the deliverable and the ranking is a separate, swappable step. That split is
//! deliberate: a menu built by a model is unauditable, and a menu that does not
//! exist cannot be ranked at all.
//!
//! Everything here is DERIVED, never hardcoded. The option list comes from the
//! harness registry, filtered by the profiles that are actually configured, so
//! the menu cannot advertise a harness no profile can reach. `codex` is
//! registered and tiered, and it appears under
//! [`HarnessMenu::registered_unconfigured`] rather than as a default — it is
//! real, and it is not pretending to be wired.
//!
//! DIVERSITY IS THE POINT. `ce-brainstorm` fans out over three roles and the
//! fix for its dispatch failure was to give it three *distinct* lenses. A crew
//! of five all on one harness is five copies of one lens wearing five hats, so
//! once a crew is big enough for lenses to matter, the ranking refuses to put
//! one harness in every leading slot.

use crate::harness_registry::{self, HarnessTier};
use crate::profile::ProfileCatalog;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One harness a crew could be built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessOption {
    /// Canonical name — never an alias.
    pub harness: String,
    pub tier: HarnessTier,
    /// Profiles that resolve to this harness, sorted.
    pub roles: Vec<String>,
    /// Slot a bare spawn would ride. `None` = model-less, or a closed harness
    /// whose slot is withheld by the tier gate.
    pub implied_slot: Option<String>,
    /// Whether the orchestrator may hand this harness a model. Derived from the
    /// tier and NOTHING else, so this label can never disagree with the gate in
    /// `models.rs`.
    pub asks_for_a_model: bool,
    /// One line an operator can read without opening the registry.
    pub why: String,
}

/// The ranked menu plus the honest remainder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessMenu {
    /// Crew size this menu was built for. It changes the ranking, not the set.
    pub crew: u32,
    /// Ranked, best first. Deterministic for a given input.
    pub options: Vec<HarnessOption>,
    /// Registered harnesses no profile uses yet. Real, but not selectable
    /// without a profile. Listing them is what makes `codex` discoverable
    /// instead of merely present.
    pub registered_unconfigured: Vec<String>,
}

// bd-drlm: the menu is read by people and by Jev. These three are the reading
// half — the ranker's inputs and the floor it re-applies — and they have no
// caller until the Jev pass is wired in. Kept because a menu nobody can read
// back is the thing this module exists to prevent.
#[allow(dead_code)]
impl HarnessMenu {
    /// The leading harnesses, in rank order.
    #[must_use]
    pub fn ranked(&self) -> Vec<&str> {
        self.options.iter().map(|o| o.harness.as_str()).collect()
    }

    /// Is `harness` offered as a default?
    #[must_use]
    pub fn offers(&self, harness: &str) -> bool {
        self.options.iter().any(|o| o.harness == harness)
    }

    /// Re-establish the diversity floor after something reordered `options`.
    ///
    /// bd-drlm: the Jev ranker calls this so a model cannot collapse a crew onto
    /// one lens. It is a no-op when the floor is already satisfied, so it is safe
    /// to call unconditionally.
    pub fn reapply_diversity_floor(&mut self) {
        enforce_diversity_floor(&mut self.options, self.crew);
    }
}

/// How many distinct harnesses a crew that size should lead with.
///
/// A crew of one has no lenses to diversify, and a crew of two is a pair. From
/// three up, `ce-brainstorm`'s lesson applies: independent lenses, not one
/// harness wearing N hats.
#[must_use]
pub const fn diversity_floor(crew: u32) -> usize {
    if crew < 3 {
        1
    } else if crew <= 4 {
        2
    } else {
        3
    }
}

/// Build the menu for a crew of `crew` size.
///
/// Derived from the catalog, so it can only offer what a profile can reach.
#[must_use]
pub fn harness_menu(catalog: &ProfileCatalog, crew: u32) -> HarnessMenu {
    let mut by_harness: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, profile) in &catalog.profiles {
        // An empty harness means "inherit", so it names nothing to offer. A
        // profile that pins a harness nobody can spawn is still listed: hiding
        // it would be the menu lying, and this is a menu for an operator.
        let raw = profile.harness.as_str();
        if raw.is_empty() {
            continue;
        }
        by_harness
            .entry(harness_registry::canonical(raw))
            .or_default()
            .push(name.clone());
    }
    for roles in by_harness.values_mut() {
        roles.sort();
    }

    let mut options: Vec<HarnessOption> = by_harness
        .into_iter()
        .map(|(harness, roles)| {
            let tier = harness_registry::tier_of(&harness);
            let asks = tier.is_some_and(HarnessTier::may_be_asked_for_a_model);
            let implied_slot = asks
                .then(|| harness_registry::implied_slot(&harness).map(str::to_string))
                .flatten();
            let why = match tier {
                Some(HarnessTier::Closed) => format!(
                    "closed frontier CLI, its own model ({} role{}); never asked for one",
                    roles.len(),
                    if roles.len() == 1 { "" } else { "s" }
                ),
                Some(HarnessTier::Open) => match &implied_slot {
                    Some(slot) => format!(
                        "open, takes a model ({} role{}); bare spawn rides @{slot}",
                        roles.len(),
                        if roles.len() == 1 { "" } else { "s" }
                    ),
                    None => format!(
                        "open, takes a model or none ({} role{}); launches model-less when unnamed",
                        roles.len(),
                        if roles.len() == 1 { "" } else { "s" }
                    ),
                },
                // A profile can name a harness the registry has never heard of:
                // config may add one with no code change. The menu must still
                // offer it, and must not invent a tier for it.
                None => format!(
                    "configured but UNREGISTERED ({} role{}); no tier, so no model is substituted",
                    roles.len(),
                    if roles.len() == 1 { "" } else { "s" }
                ),
            };
            HarnessOption {
                harness,
                tier: tier.unwrap_or(HarnessTier::Open),
                roles,
                implied_slot,
                asks_for_a_model: asks,
                why,
            }
        })
        .collect();

    rank(&mut options, crew);

    let offered: BTreeSet<&str> = options.iter().map(|o| o.harness.as_str()).collect();
    let registered_unconfigured = harness_registry::REGISTRY
        .iter()
        .map(|s| s.canonical)
        .filter(|c| !offered.contains(c))
        .map(str::to_string)
        .collect();

    HarnessMenu {
        crew,
        options,
        registered_unconfigured,
    }
}

/// Deterministic ranking. No model, no clock, no randomness.
///
/// Order: more roles first (a harness carrying more of the crew is more
/// load-bearing), then the harness name, so equal inputs give equal output.
/// Then the diversity rule promotes a second (or third) distinct harness into
/// the leading slots once the crew is big enough for lenses to matter.
fn rank(options: &mut Vec<HarnessOption>, crew: u32) {
    options.sort_by(|a, b| {
        b.roles
            .len()
            .cmp(&a.roles.len())
            .then(a.harness.cmp(&b.harness))
    });
    enforce_diversity_floor(options, crew);
}

/// Test-only seam so a fence in another module can run the REAL deterministic rank
/// instead of re-implementing it. A copy of this logic in a test is a second
/// implementation that can agree with the wrong thing.
#[cfg(test)]
pub(crate) fn rank_public_for_test(options: &mut Vec<HarnessOption>, crew: u32) {
    rank(options, crew);
}

/// Promote the best harness that is not already leading into the floor-th slot.
///
/// bd-drlm: extracted from `rank` so a Jev pass can RE-ESTABLISH it. The floor is
/// a property of the crew size and the set, not of the order the deterministic
/// sort happened to produce, so anything that reorders must re-run it — otherwise
/// a confident model could put one harness in every leading slot of a five-agent
/// crew, which is the exact failure this rule exists to prevent.
pub(crate) fn enforce_diversity_floor(options: &mut Vec<HarnessOption>, crew: u32) {
    let floor = diversity_floor(crew);
    if options.len() <= floor {
        return;
    }
    // Take the best harness that is NOT already leading, and move it up to the
    // floor-th slot. Doing it by value rather than by index keeps this stable
    // when the sort above reshuffles equal-role entries.
    let Some(diverse) = options
        .iter()
        .find(|o| o.harness != options[0].harness)
        .map(|o| o.harness.clone())
    else {
        return;
    };
    if let Some(idx) = options.iter().position(|o| o.harness == diverse) {
        // Move the ACTUAL option. Cloning a neighbour and renaming it would
        // fabricate an entry — an invented role list on a real harness name,
        // which is precisely the kind of menu that lies to an operator.
        let moved = options.remove(idx);
        let at = floor.min(options.len());
        options.insert(at, moved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    fn catalog(pairs: &[(&str, &str)]) -> ProfileCatalog {
        let mut profiles = BTreeMap::new();
        for (name, harness) in pairs {
            profiles.insert(
                (*name).to_string(),
                Profile {
                    harness: (*harness).to_string(),
                    ..Profile::default()
                },
            );
        }
        ProfileCatalog {
            profiles,
            ..ProfileCatalog::default()
        }
    }

    #[test]
    fn the_menu_is_derived_and_offers_only_what_a_profile_reaches() {
        let m = harness_menu(&catalog(&[("coder", "opencode"), ("planner", "pi")]), 3);
        assert_eq!(m.ranked(), vec!["opencode", "pi"]);
        assert!(
            m.registered_unconfigured.contains(&"codex".to_string()),
            "codex must be DISCOVERABLE even though no profile uses it: registered is not \
             the same as wired, and hiding it makes the registry a secret"
        );
    }

    /// The tier is the only thing allowed to answer the model question, so the
    /// label can never drift from the gate in `models.rs`.
    #[test]
    fn a_closed_option_never_claims_a_model_and_never_shows_a_slot() {
        let m = harness_menu(&catalog(&[("oracle", "grok"), ("coder", "opencode")]), 3);
        let grok = m
            .options
            .iter()
            .find(|o| o.harness == "grok")
            .expect("grok");
        assert!(!grok.asks_for_a_model, "grok is closed");
        assert_eq!(
            grok.implied_slot, None,
            "the tier gate withholds the slot's MODEL; showing it here would advertise a \
             model a frontier CLI is never handed"
        );
        let oc = m
            .options
            .iter()
            .find(|o| o.harness == "opencode")
            .expect("oc");
        assert!(oc.asks_for_a_model);
    }

    /// Nothing in use may be invisible. A menu that silently drops a configured
    /// harness is worse than no menu, because the operator believes it.
    #[test]
    fn every_configured_harness_appears() {
        let m = harness_menu(
            &catalog(&[("a", "oc"), ("b", "gk"), ("c", "pi"), ("d", "omp")]),
            2,
        );
        for h in ["opencode", "grok", "pi", "omp"] {
            assert!(m.offers(h), "{h} is configured and must be offered");
        }
    }

    /// An alias must be normalized: `oc` and `opencode` are ONE option, not two.
    #[test]
    fn aliases_collapse_into_one_option() {
        let m = harness_menu(&catalog(&[("a", "oc"), ("b", "opencode")]), 2);
        assert_eq!(m.options.len(), 1);
        assert_eq!(m.options[0].harness, "opencode");
        assert_eq!(m.options[0].roles, vec!["a", "b"]);
    }

    /// bd-drlm: the ce-brainstorm lesson. A crew big enough for independent
    /// lenses must not lead with one harness wearing every hat.
    #[test]
    fn a_large_crew_leads_with_distinct_harnesses() {
        // opencode carries 3 roles, pi 1: the naive rank is all-opencode.
        let c = catalog(&[
            ("coder", "opencode"),
            ("reviewer", "opencode"),
            ("oracle", "opencode"),
            ("planner", "pi"),
        ]);
        assert_eq!(
            harness_menu(&c, 1).ranked(),
            vec!["opencode", "pi"],
            "a crew of 1 has no lenses to diversify"
        );
        let big = harness_menu(&c, 5);
        assert_ne!(
            big.ranked()[0],
            big.ranked()[1],
            "a crew of 5 must not open with the same harness twice: that is one lens in five hats, \
             which is the failure ce-brainstorm already paid for"
        );
    }

    #[test]
    fn a_two_harness_menu_cannot_diversify_further() {
        let c = catalog(&[("a", "opencode"), ("b", "pi")]);
        let m = harness_menu(&c, 5);
        assert_eq!(
            m.ranked().len(),
            2,
            "two configured harnesses is the whole menu"
        );
        assert_eq!(m.ranked()[0], "opencode");
    }

    /// Determinism: the menu is a proposal an operator reads, so the same
    /// catalog must always produce the same order.
    #[test]
    fn the_ranking_is_stable_across_repeated_builds() {
        let c = catalog(&[("a", "opencode"), ("b", "pi"), ("c", "omp"), ("d", "grok")]);
        let first: Vec<String> = harness_menu(&c, 4)
            .ranked()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        for _ in 0..8 {
            let again = harness_menu(&c, 4);
            assert_eq!(
                again.ranked().len(),
                first.len(),
                "the menu must be reproducible"
            );
            for (i, h) in again.ranked().iter().enumerate() {
                assert_eq!(*h, first[i], "the menu must be reproducible");
            }
        }
    }

    /// A config-added harness the registry has never heard of must still be
    /// offered, and must NOT be given an invented tier or slot.
    #[test]
    fn an_unregistered_configured_harness_is_offered_without_a_pretend_tier() {
        let m = harness_menu(&catalog(&[("scout", "brand-new-harness")]), 2);
        let o = &m.options[0];
        assert_eq!(o.harness, "brand-new-harness");
        assert!(
            !o.asks_for_a_model,
            "no tier means no substitution; guessing Open would hand a model to a harness \
             whose CLI shape nobody has read"
        );
        assert_eq!(o.implied_slot, None);
        assert!(
            o.why.contains("UNREGISTERED"),
            "the label must say so: {}",
            o.why
        );
    }

    /// The diversity rule must not invent an option or duplicate one.
    #[test]
    fn diversity_never_duplicates_or_invents_an_option() {
        let c = catalog(&[("a", "opencode"), ("b", "pi"), ("c", "omp")]);
        for crew in 1..=9 {
            let m = harness_menu(&c, crew);
            let mut seen = BTreeSet::new();
            for h in m.ranked() {
                assert!(seen.insert(h), "crew {crew}: {h} appeared twice");
                assert!(
                    harness_registry::spec(h).is_some(),
                    "crew {crew}: {h} is offered but not registered"
                );
            }
            assert_eq!(
                seen.len(),
                m.options.len(),
                "crew {crew}: a duplicate was lost"
            );
        }
    }
}

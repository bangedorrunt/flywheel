// governed-by: ADR-0019
// Wave-2 minimal principle-name registry (bd-w2-name-registry-8d6u): the 21
// canonical pstack-derived names from ADR-0019 Spec 5, names only. No leaves,
// no CLI — that is wave 3. Wave-2 lints validate `principles:` names against
// this list.
//
// Lint-bridge contract: flywheel passes `PRINCIPLE_NAMES` into toron-workflow
// lint (flywheel -> toron-workflow), keeping the dep direction clean — no
// reverse dependency.

/// The 21 canonical principle names (ADR-0019 Spec 5, pstack-derived).
pub const PRINCIPLE_NAMES: [&str; 21] = [
    // Core
    "laziness-protocol",
    "foundational-thinking",
    "redesign-from-first-principles",
    "subtract-before-you-add",
    "minimize-reader-load",
    "outcome-oriented-execution",
    "experience-first",
    "exhaust-the-design-space",
    "build-the-lever",
    // Architecture
    "model-the-domain",
    "boundary-discipline",
    "type-system-discipline",
    "make-operations-idempotent",
    "migrate-callers-then-delete-legacy-apis",
    "separate-before-serializing-shared-state",
    // Verification
    "prove-it-works",
    "fix-root-causes",
    "sequence-verifiable-units",
    // Delegation
    "guard-the-context-window",
    "never-block-on-the-human",
    // Meta
    "encode-lessons-in-structure",
];

/// Is `name` one of the 21 canonical principle names?
pub fn is_known_name(name: &str) -> bool {
    PRINCIPLE_NAMES.contains(&name)
}

/// One compiled-in principle leaf (ADR-0019 Spec 5, wave 3): the canonical
/// name, the `when` trigger, and the full `rule` — verbatim from the Spec 5
/// tables. `flywheel principles show <name>` resolves these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Principle {
    pub name: &'static str,
    pub when: &'static str,
    pub rule: &'static str,
}

/// The 21 compiled-in leaves, verbatim from ADR-0019 Spec 5 tables.
pub const PRINCIPLES: [Principle; 21] = [
    Principle {
        name: "laziness-protocol",
        when: "Refactor, sizing a diff, tempted to add a layer",
        rule: "Prefer deletion. Smallest change that solves it. Flat call hierarchy.",
    },
    Principle {
        name: "foundational-thinking",
        when: "Before writing logic",
        rule: "Data structures first. Scaffold before features. Ask what concurrent actors share.",
    },
    Principle {
        name: "redesign-from-first-principles",
        when: "Integrating a new requirement",
        rule: "Redesign as if it had been there on day one. Do not bolt on.",
    },
    Principle {
        name: "subtract-before-you-add",
        when: "Sequencing an addition or rewrite",
        rule: "Remove dead weight first, then build on the simpler base.",
    },
    Principle {
        name: "minimize-reader-load",
        when: "Shaping hard-to-trace code",
        rule: "Count layers and hidden state. Collapse one-caller wrappers.",
    },
    Principle {
        name: "outcome-oriented-execution",
        when: "Planned migrations with phase boundaries",
        rule: "Converge on the target. Do not keep throwaway compatibility.",
    },
    Principle {
        name: "experience-first",
        when: "Product / UX / scope tradeoffs",
        rule: "User delight over implementation convenience. Ship less, ship better.",
    },
    Principle {
        name: "exhaust-the-design-space",
        when: "Novel interaction or architecture, no precedent",
        rule: "Build 2–3 competing prototypes. Compare. Then commit.",
    },
    Principle {
        name: "build-the-lever",
        when: "Any non-trivial work",
        rule: "Build the tool that does or proves it. The tool is what a reviewer reruns.",
    },
    Principle {
        name: "model-the-domain",
        when: "Stateful logic, branching, repeated shape",
        rule: "Encode the domain in a structure, not scattered ifs.",
    },
    Principle {
        name: "boundary-discipline",
        when: "Validation, errors, adapters",
        rule: "Guards at the boundary. Trust internal types. Pure core.",
    },
    Principle {
        name: "type-system-discipline",
        when: "Types and signatures",
        rule: "Illegal states unrepresentable. Brand primitives. Parse at the boundary.",
    },
    Principle {
        name: "make-operations-idempotent",
        when: "Commands, loops, crashes, retries",
        rule: "Converge to the same end state from any partial prior run.",
    },
    Principle {
        name: "migrate-callers-then-delete-legacy-apis",
        when: "New internal API, old callers exist",
        rule: "Migrate and delete in one wave. No compatibility layer.",
    },
    Principle {
        name: "separate-before-serializing-shared-state",
        when: "Concurrent writers",
        rule: "Eliminate sharing first. Lock only when one writer is a real invariant.",
    },
    Principle {
        name: "prove-it-works",
        when: "After a task, before \"done\"",
        rule: "Check the real artifact. Not a proxy, not \"it compiles\", not a self-report.",
    },
    Principle {
        name: "fix-root-causes",
        when: "Debugging",
        rule: "Reproduce first. Ask why until the root. No nil-check silencers.",
    },
    Principle {
        name: "sequence-verifiable-units",
        when: "Multi-step work, stacking commits",
        rule: "Each unit ends in a check. Verify before the next.",
    },
    Principle {
        name: "guard-the-context-window",
        when: "Large outputs, fan-out, long files",
        rule: "Bulk to subagents. Summaries in the main thread.",
    },
    Principle {
        name: "never-block-on-the-human",
        when: "Tempted to ask \"should I?\" on reversible work",
        rule: "Classify the question first: if the answer is a fact observable by running something, it is not the human's — prototype/sketch it and let the result decide. Otherwise proceed and present the result. Reserve asks for irreversible writes or genuine preference calls.",
    },
    Principle {
        name: "encode-lessons-in-structure",
        when: "Writing the same instruction a second time",
        rule: "Lint, type, preflight, or script. Then delete the instruction.",
    },
];

/// Resolve one principle leaf by exact name.
pub fn show(name: &str) -> Option<&'static Principle> {
    PRINCIPLES.iter().find(|p| p.name == name)
}

/// A suggestion for an unknown name: the canonical name the typo is a
/// strict prefix of, or a canonical name the typo extends by at most three
/// chars. `None` when nothing is close (or the typo is too short to be
/// meaningful).
pub fn suggest(name: &str) -> Option<&'static str> {
    if name.len() < 3 {
        return None;
    }
    PRINCIPLES.iter().map(|p| p.name).find(|n| {
        (n.starts_with(name) && n.len() > name.len())
            || (name.starts_with(n) && name.len() <= n.len() + 3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly 21 canonical names, every one known.
    #[test]
    fn exactly_twenty_one_canonical_names_each_known() {
        assert_eq!(PRINCIPLE_NAMES.len(), 21);
        let mut sorted = PRINCIPLE_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 21, "names must be unique");
        for name in PRINCIPLE_NAMES {
            assert!(is_known_name(name), "known name must resolve: {name}");
        }
    }

    /// A plausible fake (missing the final 's') is NOT known.
    #[test]
    fn plausible_fake_is_unknown() {
        assert!(!is_known_name("prove-it-work"));
        assert!(!is_known_name(""));
        assert!(!is_known_name("prove-it-works "));
        assert!(!is_known_name("PROVE-IT-WORKS"));
        assert!(!is_known_name("laziness"));
    }

    /// Wave 3: exactly 21 leaves; names match the wave-2 registry; every
    /// leaf carries a non-empty when + rule.
    #[test]
    fn exactly_twenty_one_leaves_with_when_and_rule() {
        assert_eq!(PRINCIPLES.len(), 21);
        for p in &PRINCIPLES {
            assert!(!p.name.is_empty());
            assert!(!p.when.is_empty(), "{}: when must be non-empty", p.name);
            assert!(!p.rule.is_empty(), "{}: rule must be non-empty", p.name);
        }
        // The leaf names are byte-identical to the wave-2 name registry.
        let leaf_names: Vec<&str> = PRINCIPLES.iter().map(|p| p.name).collect();
        let mut registry = PRINCIPLE_NAMES.to_vec();
        registry.sort_unstable();
        let mut leaves = leaf_names;
        leaves.sort_unstable();
        assert_eq!(registry, leaves, "leaf names must match the name registry");
    }

    /// show() resolves every canonical name with the FULL rule text (the
    /// acceptance: `flywheel principles show prove-it-works` prints the full
    /// rule, not a gist).
    #[test]
    fn show_resolves_every_name_to_full_rule() {
        for p in &PRINCIPLES {
            let leaf = show(p.name).expect("show must resolve every name");
            assert_eq!(leaf.name, p.name);
            assert_eq!(leaf.rule, p.rule);
        }
        // The canonical example: prove-it-works prints the full rule.
        let leaf = show("prove-it-works").expect("prove-it-works resolves");
        assert_eq!(
            leaf.rule,
            "Check the real artifact. Not a proxy, not \"it compiles\", not a self-report."
        );
    }

    /// Unknown name -> None (the CLI turns it into a non-zero exit) + a
    /// suggestion for a plausible typo.
    #[test]
    fn show_unknown_returns_none_with_suggestion() {
        assert!(show("prove-it-work").is_none());
        assert_eq!(suggest("prove-it-work"), Some("prove-it-works"));
        assert_eq!(suggest("laziness-protoco"), Some("laziness-protocol"));
        assert_eq!(suggest(""), None);
        assert_eq!(suggest("zzz-nothing"), None);
    }
}

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
}

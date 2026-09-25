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

/// The canonical 21-name registry as a slice — the ADR-0019 wave-2 bridge
/// (bd-4spn): flywheel's workflow-lint call sites pass THIS into
/// `toron_workflow::lint_workflow_with_principles`, keeping the lint
/// registry identical to the check_principles registry (no silent drift).
pub fn names() -> &'static [&'static str] {
    &PRINCIPLE_NAMES
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

/// Result of checking a report's PRINCIPLES field (ADR-0019 Spec 6
/// delivery 3 + the risks-table rule: a citation with no decision is a
/// skip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrinciplesCheck {
    /// Required for priority <= 2 and every line is `name — decision it
    /// changed` with a known name.
    Complete,
    /// Missing or insufficient citations.
    Incomplete { reason: String },
}

/// Check a report's PRINCIPLES section (bd-w3-brief-principles-yy39):
/// - priority <= 2: the field is REQUIRED (empty -> incomplete);
/// - every line must be `name — decision it changed`; a name-only line has
///   no decision clause and is a cargo-cult citation (incomplete);
/// - names must be from the canonical 21-name registry (unknown -> fail,
///   regardless of priority).
///
/// The builder emits the field; this checker enforces it (the ce-review
/// stage BLOCKER language is the human-side of the same rule).
pub fn check_principles(priority: u8, principles: &str) -> PrinciplesCheck {
    let lines: Vec<&str> = principles
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if priority <= 2 && lines.is_empty() {
        return PrinciplesCheck::Incomplete {
            reason: format!(
                "PRINCIPLES is required for priority <= 2 beads (P{priority} report has none)"
            ),
        };
    }
    for line in &lines {
        // The brief shapes citations as `<name> — <the decision it
        // changed>` (em-dash). Anything without the separator is a
        // name-only cargo-cult citation.
        let Some((name, decision)) = line.split_once('—') else {
            return PrinciplesCheck::Incomplete {
                reason: format!("citation has no decision clause (name-only is a skip): {line:?}"),
            };
        };
        let name = name.trim();
        let decision = decision.trim();
        if decision.is_empty() {
            return PrinciplesCheck::Incomplete {
                reason: format!("citation has no decision clause: {line:?}"),
            };
        }
        if !is_known_name(name) {
            return PrinciplesCheck::Incomplete {
                reason: format!(
                    "unknown principle name {name:?} in citation (canonical 21, see `flywheel principles list`): {line:?}"
                ),
            };
        }
    }
    PrinciplesCheck::Complete
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

    // ---- bd-w3-brief-principles-yy39: report PRINCIPLES checker ----

    /// Empty PRINCIPLES on a P0-P2 report -> incomplete; on P3/P4 the field
    /// is optional.
    #[test]
    fn principles_brief_checker_requires_field_on_p0_p2() {
        assert!(matches!(
            check_principles(2, ""),
            PrinciplesCheck::Incomplete { .. }
        ));
        assert!(matches!(
            check_principles(0, "  \n "),
            PrinciplesCheck::Incomplete { .. }
        ));
        assert!(matches!(
            check_principles(1, ""),
            PrinciplesCheck::Incomplete { .. }
        ));
        assert_eq!(check_principles(3, ""), PrinciplesCheck::Complete);
        assert_eq!(check_principles(4, ""), PrinciplesCheck::Complete);
    }

    /// `name — decision it changed` passes, single and multi-line.
    #[test]
    fn principles_brief_checker_accepts_name_decision() {
        assert_eq!(
            check_principles(0, "prove-it-works — checked the ledger row before closing"),
            PrinciplesCheck::Complete
        );
        assert_eq!(
            check_principles(
                2,
                "fix-root-causes — reproduced the failure first\nsequence-verifiable-units — one check per bead before the next"
            ),
            PrinciplesCheck::Complete
        );
        assert_eq!(
            check_principles(4, "laziness-protocol — deleted a dead wrapper"),
            PrinciplesCheck::Complete
        );
    }

    /// Every rejection shape in one table: name-only lines (no decision
    /// clause) are cargo-cult citations, and an unknown name fails
    /// regardless of priority.
    #[test]
    fn principles_brief_checker_rejects_bad_citations() {
        let cases: [(&str, u8, &str); 6] = [
            ("name_only", 2, "prove-it-works"),
            ("name_only_dangling_dash", 2, "prove-it-works —"),
            ("name_only_multiline", 2, "fix-root-causes\nprove-it-works"),
            ("unknown_name_typo", 2, "prove-it-work — did something"),
            ("unknown_name_truncated", 4, "laziness — deleted a wrapper"),
            // A known name in a garbled line still fails on the name parse.
            ("garbled_line", 2, "prove-it-works - checked the ledger"),
        ];
        for (case, priority, text) in cases {
            assert!(
                matches!(
                    check_principles(priority, text),
                    PrinciplesCheck::Incomplete { .. }
                ),
                "{case}"
            );
        }
    }

    /// bd-4spn GAP 2 cross-repo bridge: tamper one name in the flywheel
    /// registry -> toron-workflow lint rejects the citation with
    /// unknown-principle. Proves the flywheel list (not the toron in-crate
    /// fallback, which still has the original name) drives the lint.
    #[test]
    fn principle_registry_drives_workflow_lint_bridge() {
        use toron_workflow::schema::Workflow;
        let wf = Workflow {
            name: "bridge-fixture".into(),
            description: None,
            trigger: None,
            policy: None,
            guard: None,
            principles: vec!["laziness-protocol".into()],
            steps: vec![],
            graph: None,
            inputs: vec![],
            crew: None,
        };
        let unknown = |r: &toron_workflow::LintReport| {
            r.issues
                .iter()
                .any(|i| i.message.contains("unknown principle name"))
        };
        // The real flywheel registry resolves the citation.
        let real = toron_workflow::lint_workflow_with_principles(&wf, names());
        assert!(
            !unknown(&real),
            "canonical registry accepts: {:?}",
            real.issues
        );
        // Tamper: drop laziness-protocol from the passed list.
        let tampered: Vec<&str> = names()
            .iter()
            .copied()
            .filter(|n| *n != "laziness-protocol")
            .collect();
        let report = toron_workflow::lint_workflow_with_principles(&wf, &tampered);
        assert!(
            unknown(&report),
            "tampered registry must reject the citation: {:?}",
            report.issues
        );
        // The in-crate fallback still knows the name — the tampered list,
        // not the fallback, was live.
        let fallback = toron_workflow::lint_workflow(&wf);
        assert!(!unknown(&fallback), "fallback still resolves the name");
    }
}

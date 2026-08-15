//! Stage library (ADR-0005 Spec 2a / P3.3).
//!
//! Elevates flywheel's hardcoded `STAGES` const (the captain's p1/p2/p4/p5/p6/p8
//! daily-driver prompts) into a named, overridable library with **semantic
//! names** — p-codes stay as aliases (backward compat).
//!
//! - Compiled-in defaults ship the p1–p8 prompts verbatim (byte-identical to
//!   the former `STAGES` const — `flywheel send --stage p6` keeps working).
//! - `~/.config/flywheel/stages.toml` overrides (field-level: a `[stages.x]`
//!   entry replaces or adds; p-code aliases resolve to the same prompt).
//! - A workflow step may be `{ stage: systematic-execute }` OR `{ stage: p6 }`
//!   — both resolve to the same prompt (the `stage_alias` contract).
//!
//! governed-by: ADR-0005 Spec 2a, P3.3

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

/// The semantic-name → p-code alias map (Spec 2a). p-codes stay as aliases;
/// the semantic names are the authoring surface.
pub const SEMANTIC_ALIASES: &[(&str, &str)] = &[
    // (semantic, p-code)
    ("fresh-eyes-explore", "p1"),
    ("plan-review", "p5"),
    ("systematic-execute", "p6"),
];

/// A named stage: a reusable prompt primitive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    /// Canonical name (semantic, e.g. `systematic-execute`).
    pub name: String,
    /// The p-code alias (e.g. `p6`) — backward compat.
    pub alias: String,
    /// The prompt text.
    pub prompt: String,
}

/// The stages library: semantic name → stage.
#[derive(Debug, Clone, Default)]
pub struct StageLibrary {
    stages: BTreeMap<String, Stage>,
}

impl StageLibrary {
    /// Build the library: compiled-in defaults layered under a user
    /// `~/.config/flywheel/stages.toml` override (field-level — a user entry
    /// replaces the prompt for an existing stage or adds a new one).
    pub fn load() -> Result<Self> {
        let mut lib = Self::compiled_in();
        lib.override_from_toml()?;
        Ok(lib)
    }

    /// The compiled-in defaults (the former `STAGES` const, verbatim) under
    /// their semantic names + p-code aliases.
    pub fn compiled_in() -> Self {
        let defaults: &[(&str, &str, &str)] = &[
            (
                "fresh-eyes-explore",
                "p1",
                "I want you to sort of randomly explore the code files in this project, choosing code files to deeply investigate and understand and trace their functionality and execution flows through the related code files which they import or which they are imported by. Once you understand the purpose of the code in the larger context of the workflows, I want you to do a super careful, methodical, and critical check with \"fresh eyes\" to find any obvious bugs, problems, errors, issues, silly mistakes, etc. and then systematically and meticulously and intelligently correct them. Be sure to comply with ALL rules in AGENTS dot md.",
            ),
            (
                "code-review",
                "p2",
                "Review the code written by your fellow agents — issues, bugs, errors, problems, inefficiencies, security problems, reliability issues, etc. — and carefully diagnose their underlying root causes using first-principle analysis. Don't restrict yourself to the latest commits; cast a wider net and go super deep. Use ultrathink. Principles in force: prove-it-works (check the real artifact — run the change's VERIFY or the ledger proof, never a self-report), fix-root-causes (reproduce first; name the root, not the symptom), minimize-reader-load (count layers and hidden state; collapse one-caller wrappers). BLOCKER: a REPORT line that cites a PRINCIPLES name without its decision (`<name> — <the decision it changed>`) is a cargo-cult citation — reject it and require the decision the principle changed; on a non-trivial bead (priority <= 2) an empty PRINCIPLES field is an incomplete report (the loop's `principles` checker enforces the same rule: known name + decision clause, required on P0-P2).",
            ),
            (
                "decompose-beads",
                "p4",
                "OK so please take ALL of that and elaborate on it more and then create a comprehensive and granular set of beads for all this with tasks, subtasks, and dependency structure overlaid, with detailed comments so that the whole thing is totally self-contained and self-documenting (including relevant background, reasoning/justification, considerations, etc.-- anything we'd want our \"future self\" to know about the goals and intentions and thought process and how it serves the over-arching goals of the project.) Follow the beads-workflow discipline: check your beads N times before implementing, keep each bead self-contained (background, reasoning, acceptance criteria), and link dependencies with `br dep add`.",
            ),
            (
                "plan-review",
                "p5",
                "Check over each bead super carefully-- are you sure it makes sense? Is it optimal? Could we change anything to make the system work better for users? If so, revise the beads. It's a lot easier and faster to operate in \"plan space\" before we start implementing these things!",
            ),
            (
                "systematic-execute",
                "p6",
                "Execute the beads via the brief contract. Every dispatched bead carries a filled brief (GOAL / SCOPE / CONTEXT / ACCEPTANCE / VERIFY / TIMEBOX / FORBIDDEN / REPORT / PRINCIPLES / STANDING) — the brief IS the work contract, and the lifecycle lives in the standing-orders file named in its STANDING field (read it once before starting; never re-derive it from memory). For each bead: run the VERIFY command from the brief, write the receipt to `.flywheel/receipts/<bead-id>-<sha>.<kind>.txt`, then close only with a REPORT that states status, commit sha (commit BEFORE close; bead id IN the commit message), verdict kind, what you actually ran, deviations, and the PRINCIPLES cited (`<name> — <the decision it changed>`). Principles in force: prove-it-works (the close is not done without a legal ledger row — verify with `flywheel ledger check <bead-id>`; the receipt is evidence, the verdict is the proof), fix-root-causes (reproduce before diagnosing; no silencers), sequence-verifiable-units (each bead ends in a check before the next starts). Prefer delegating to the `beads` subagent when available. If another agent (peer) is working in this project, join its shared room and steer on plane S (ADR-0020): `toron channel join steer-<bead-id> --project <slug> --as \"$AGENT_NAME\"` then accept/claim jobs posted there via `toron job list --state open --project <slug> --as \"$AGENT_NAME\"` + `toron job claim <job-id> --project <slug> --as \"$AGENT_NAME\"` (first claimer wins; a second claimer exits 1). Keep shared work on the room; keep secrets on plane P (`toron mail`). No worktree.",
            ),
            (
                "commit-and-push",
                "p8",
                "Now, based on your knowledge of the project, commit all changed files now in a series of logically connected groupings with super detailed commit messages for each and then push. Take your time to do it right. Don't edit the code at all. Don't commit obviously ephemeral files. Use ultrathink.",
            ),
            (
                "repo-map",
                "p3",
                "Cold-start mapping (onboard step 1): read AGENTS.md and the top-level docs in this repo, list the crate/module structure and the main entry points, and trace how the pieces connect (entry points, core modules, tests, build/test/lint commands). Produce a concise map the newcomer can act from — paths, responsibilities, conventions, and the exact commands to build, test, and lint.",
            ),
            (
                "seed-first-task",
                "p7",
                "Onboard step 2: from the repo map produced in the previous step, seed the newcomer's context — summarize the map, state the conventions they MUST follow (TDD, skills, code style), and propose ONE narrow first task with [paths] [change] [acceptance] so they can start immediately.",
            ),
            (
                "bv-triage",
                "p9",
                "Deep-triage step 1: run `bv --robot-triage` (and `br ready --json` for the actionable subset) over this project's beads, then produce a compact summary: counts by status/priority, the top actionable items, blockers, and any cycles or stale items worth attention.",
            ),
            (
                "prioritize-assign",
                "p10",
                "Deep-triage step 2: from the bv-triage summary, prioritize the actionable backlog in dependency order and assign each item to an owner (agent identity registered in the project). Output an ordered, owned list the crew can execute.",
            ),
            (
                "spawn-crew",
                "p11",
                "Deep-triage step 3: spawn the assigned crew for the prioritized backlog — `flywheel spawn <project> --recipe <squad>` (or the explicit counts) and hand each owner their assigned items. Confirm the panes are up and the crew is ready before finishing.",
            ),
        ];
        let mut stages = BTreeMap::new();
        for (name, alias, prompt) in defaults {
            stages.insert(
                name.to_string(),
                Stage {
                    name: name.to_string(),
                    alias: alias.to_string(),
                    prompt: prompt.to_string(),
                },
            );
        }
        StageLibrary { stages }
    }

    /// Apply the user `stages.toml` override (field-level).
    fn override_from_toml(&mut self) -> Result<()> {
        let path = config_dir().join("stages.toml");
        if !path.exists() {
            return Ok(());
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        #[derive(Deserialize)]
        struct UserStages {
            #[serde(default)]
            stages: BTreeMap<String, Stage>,
        }
        let user: UserStages =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        for (name, stage) in user.stages {
            self.stages.insert(name, stage);
        }
        Ok(())
    }

    /// Resolve a stage name (semantic OR p-code alias) to its prompt.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        if let Some(stage) = self.stages.get(name) {
            return Some(&stage.prompt);
        }
        // p-code alias → find the stage whose alias matches.
        self.stages
            .values()
            .find(|s| s.alias == name)
            .map(|s| s.prompt.as_str())
    }

    /// True when `name` resolves (semantic or alias).
    pub fn has(&self, name: &str) -> bool {
        self.resolve(name).is_some()
    }

    /// All stages as (canonical_name, alias) pairs — for `flywheel stage list`.
    pub fn list(&self) -> Vec<(&str, &str)> {
        self.stages
            .values()
            .map(|s| (s.name.as_str(), s.alias.as_str()))
            .collect()
    }

    /// The semantic aliases table (Spec 2a).
    pub fn semantic_aliases(&self) -> Vec<(&'static str, &'static str)> {
        SEMANTIC_ALIASES.to_vec()
    }
}

/// The user override path (`~/.config/flywheel/stages.toml`), if present.
pub fn override_path() -> Option<PathBuf> {
    let path = config_dir().join("stages.toml");
    path.is_file().then_some(path)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// `stage: p6` == `stage: systematic-execute` (the `stage_alias`
    /// acceptance) — the same resolved prompt.
    #[test]
    fn stage_alias_p6_equals_systematic_execute() {
        let lib = StageLibrary::compiled_in();
        let by_alias = lib.resolve("p6").expect("p6 resolves");
        let by_semantic = lib
            .resolve("systematic-execute")
            .expect("semantic resolves");
        assert_eq!(by_alias, by_semantic, "p6 == systematic-execute");
    }

    /// W3 (bd-w3-stages-o8lg): p2/p6 resolved prompts name the three
    /// verification principles; p6 cites the `flywheel ledger` CLI; p2
    /// carries the cargo-cult BLOCKER language. The stage_alias contract
    /// survives the prompt edits.
    #[test]
    fn p2_p6_cite_verification_principles_and_ledger() {
        let lib = StageLibrary::compiled_in();
        let p6 = lib.resolve("p6").expect("p6 resolves");
        for name in [
            "prove-it-works",
            "fix-root-causes",
            "sequence-verifiable-units",
        ] {
            assert!(p6.contains(name), "p6 must name {name}");
        }
        assert!(
            p6.contains("flywheel ledger check"),
            "p6 cites the ledger CLI"
        );
        let p2 = lib.resolve("p2").expect("p2 resolves");
        for name in ["prove-it-works", "fix-root-causes", "minimize-reader-load"] {
            assert!(p2.contains(name), "p2 must name {name}");
        }
        assert!(p2.contains("BLOCKER"), "p2 carries BLOCKER language");
        assert!(p2.contains("cargo-cult"), "p2 rejects cargo-cult citations");
        assert_eq!(
            lib.resolve("p6"),
            lib.resolve("systematic-execute"),
            "p6 == systematic-execute after the edits"
        );
    }

    /// The compiled-in library exposes the p-codes (backward compat: existing
    /// `flywheel send --stage p6` keeps working).
    #[test]
    fn compiled_in_exposes_p_codes() {
        let lib = StageLibrary::compiled_in();
        for code in ["p1", "p2", "p4", "p5", "p6", "p8"] {
            assert!(lib.has(code), "compiled-in stage {code} resolves");
        }
    }

    /// bd-oj9o #2: the onboard + triage-deep workflows resolve their stages.
    #[test]
    fn onboard_and_triage_deep_stages_resolve() {
        let lib = StageLibrary::compiled_in();
        for semantic in [
            "repo-map",
            "seed-first-task",
            "bv-triage",
            "prioritize-assign",
            "spawn-crew",
        ] {
            assert!(
                lib.resolve(semantic).is_some(),
                "stage {semantic} resolves (bd-oj9o #2)"
            );
        }
    }

    /// The semantic aliases table maps the three named stages.
    #[test]
    fn semantic_aliases_cover_named_stages() {
        let lib = StageLibrary::compiled_in();
        let aliases = lib.semantic_aliases();
        for (semantic, code) in aliases {
            assert!(lib.has(semantic), "semantic {semantic} resolves");
            assert!(lib.has(code), "p-code {code} resolves");
        }
    }

    /// A user `stages.toml` override replaces a compiled-in prompt.
    #[test]
    fn user_override_replaces_prompt() {
        // XDG_CONFIG_HOME mutation is process-global — serialize (193.2).
        let _g = crate::config::lock_env();
        let lib = StageLibrary::compiled_in();
        let old = lib.resolve("p6").unwrap().to_string();
        let dir = std::env::temp_dir().join(format!("fw-stages-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::create_dir_all(dir.join("flywheel")).unwrap();
        std::fs::write(
            dir.join("flywheel/stages.toml"),
            "[stages.systematic-execute]\nname = \"systematic-execute\"\nalias = \"p6\"\nprompt = \"MY OVERRIDE PROMPT\"\n",
        )
        .unwrap();
        // Rebuild with the override applied.
        let lib2 = StageLibrary::load().unwrap();
        assert_eq!(lib2.resolve("p6"), Some("MY OVERRIDE PROMPT"));
        assert_ne!(old, "MY OVERRIDE PROMPT");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

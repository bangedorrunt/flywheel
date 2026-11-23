//! Stage library (ADR-0005 Spec 2a / P3.3; reworked by ADR-0009).
//!
//! A named, overridable library of prompt primitives. The compound-engineering
//! loop stages carry the `ce-` prefix (ADR-0009); the p-code aliases are
//! RETIRED — semantic names are the only names (clean cutover, no shims).
//!
//! - Compiled-in defaults seed the daily-driver prompts under their semantic
//!   names; `~/.config/flywheel/stages.toml` and `<project>/.flywheel/stages.toml`
//!   override field-level (seed < repo < user < project).
//! - A workflow step names a stage as `{ stage: ce-execute }`; a compose part
//!   does the same bare (`--parts coder:ce-execute`).
//!
//! governed-by: ADR-0005 Spec 2a, P3.3; ADR-0009

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

/// Registry tier (ADR-0022 §3.8). Override precedence follows the Ord order:
/// project wins over user, user over repo data, repo data over the compiled
/// seed (`seed < repo < user < project`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    /// Compiled-in bootstrap prompts (nobody writable).
    Seed,
    /// Promoted DATA shipped in the flywheel repo (`repo-stages.toml`) —
    /// reached only through the §3.8 evidence bar.
    Repo,
    /// `~/.config/flywheel/stages.toml` (this machine).
    User,
    /// `<project>/.flywheel/stages.toml` (ships with the repo, git).
    Project,
}

impl Tier {
    /// The tag surfaced by `stage list --all` / `stage show`.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Repo => "repo",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// The §4 lifecycle phases — the only values a stage's `phase` metadata may
/// carry (stage lint rejects anything else). `compound` is ADR-0009: the
/// learning-capture closer, mirrored in toron-workflow's closed `Phase` enum.
pub const PHASES: &[&str] = &[
    "resolve",
    "plan",
    "implement",
    "review",
    "verify",
    "compound",
];

/// Prompt size cap enforced by `stage lint` (§3.8 "size cap"). Generous —
/// the largest seed prompt is ~1.5k chars — but bounded: stages are prompts,
/// not documents.
pub const PROMPT_SIZE_CAP: usize = 20_000;

/// Dispatch-channel contract for a stage step (ADR-0021 D3): where the step
/// may write. Closed set — unknown values fail parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StageMode {
    #[serde(rename = "read-only")]
    ReadOnly,
    #[serde(rename = "workspace-write")]
    WorkspaceWrite,
    #[serde(rename = "isolated")]
    Isolated,
}

/// Execution-context contract (ADR-0021 D3; toron ContextMode Inherit|Fresh):
/// `warm` inherits the session (= Inherit), `fresh` starts a new one.
/// Closed set — unknown values fail parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StageContext {
    #[serde(rename = "warm")]
    Warm,
    #[serde(rename = "fresh")]
    Fresh,
}

/// How a stage that NO shipped workflow dispatches is still reachable
/// (ADR-0021 D6, bd-jhz8x.3): a compose token (`scout:fresh-eyes-explore`) or a
/// by-hand `flywheel send --stage`. Declared, it counts as referenced; silent,
/// it stays a dead stage and `workflow verify` fails on it. Closed set —
/// unknown values fail parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StageReach {
    #[serde(rename = "compose")]
    Compose,
    #[serde(rename = "manual")]
    Manual,
}

/// Bounded repetition contract (ADR-0021 D3/D7): run at least `min`, at most
/// `max`, until `until` holds. Validated: 1 <= min <= max <= 32.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repeat {
    pub min: u32,
    pub max: u32,
    pub until: String,
}

/// A named stage: a reusable prompt primitive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    /// Canonical semantic name (e.g. `ce-execute`).
    pub name: String,
    /// The prompt text.
    pub prompt: String,
    /// Default role affinity (§3.8): the `@profile` a bare-name compose part
    /// dispatches to. Must name a catalog profile (lint validates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The §4 lifecycle phase this stage serves (lint validates the name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Declared principles (ADR-0019 registry names; lint validates).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub principles: Vec<String>,
    /// Write channel the step may use (ADR-0021 D3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<StageMode>,
    /// Session context: warm inherits, fresh starts new (ADR-0021 D3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<StageContext>,
    /// Model-effort knob (ADR-0021 D3; mapped per harness by ADR-0022).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Typed inputs the step needs (`inputs.*` or `steps.*` refs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<String>,
    /// Named witness contract checked at the step exit (ADR-0021 D4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witness: Option<String>,
    /// Bounded repetition contract (ADR-0021 D3/D7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<Repeat>,
    /// One-line note on when to dispatch this stage (stage list groups by it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_when: Option<String>,
    /// Declared reach for a stage no shipped workflow dispatches (ADR-0021 D6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reached_by: Option<StageReach>,
    /// Provenance (§3.8): who authored it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Provenance: the run that produced it (composed/kept lineage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_run: Option<String>,
    /// Required D1 payload params (ADR-0006 §3 D1): every name here must be
    /// supplied via `send --param k=v` before the prompt renders. Empty for
    /// all seed stages → zero-param sends stay byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<String>,
}

/// Field-level stage merge (bd-cfgv): `over` fields that are present
/// (Some / non-empty) win; absent fields inherit `base`. `prompt` is
/// serde-required so it always comes from `over`; `name` is the map key.
fn merge_stage(base: Stage, over: Stage) -> Stage {
    Stage {
        name: over.name,
        prompt: over.prompt,
        role: over.role.or(base.role),
        phase: over.phase.or(base.phase),
        principles: if over.principles.is_empty() {
            base.principles
        } else {
            over.principles
        },
        author: over.author.or(base.author),
        origin_run: over.origin_run.or(base.origin_run),
        params: if over.params.is_empty() {
            base.params
        } else {
            over.params
        },
        mode: over.mode.or(base.mode),
        context: over.context.or(base.context),
        effort: over.effort.or(base.effort),
        reads: if over.reads.is_empty() {
            base.reads
        } else {
            over.reads
        },
        witness: over.witness.or(base.witness),
        repeat: over.repeat.or(base.repeat),
        use_when: over.use_when.or(base.use_when),
        reached_by: over.reached_by.or(base.reached_by),
    }
}

/// The stages library: semantic name → stage.
#[derive(Debug, Clone, Default)]
pub struct StageLibrary {
    stages: BTreeMap<String, Stage>,
    /// The tier each entry came from (§3.8). Same keys as `stages`.
    tiers: BTreeMap<String, Tier>,
}

/// Seed role affinities (§3.8): the default `@profile` a bare-name compose
/// part dispatches to. Data, not code — authored stages carry their own.
const SEED_AFFINITY: &[(&str, &str)] = &[
    ("ce-brainstorm", "planner"),
    ("ce-plan", "planner"),
    ("ce-work", "coder"),
    ("ce-review", "reviewer"),
    ("ce-verify", "reviewer"),
    ("ce-compound", "housekeeper"),
    ("ce-debug", "scout"),
    ("ce-explore", "scout"),
];

/// Seed phase metadata for the compound-engineering loop stages (ADR-0009):
/// the loop is brainstorm → plan → plan-review → execute → simplify → review
/// → verify → compound, and `stage list` should say so.
const SEED_PHASE: &[(&str, &str)] = &[
    ("ce-brainstorm", "plan"),
    ("ce-plan", "plan"),
    ("ce-work", "implement"),
    ("ce-review", "review"),
    ("ce-verify", "verify"),
    ("ce-compound", "compound"),
    ("ce-debug", "resolve"),
    ("ce-explore", "resolve"),
];

/// Seed witness contracts (ADR-0021 D4.2): the named contract checked at the
/// step exit. Every name must be in `phase_contracts::WITNESSES` (lint
/// enforces on every tier); a typo'd witness is a silent never-exit.
const SEED_WITNESS: &[(&str, &str)] = &[
    ("ce-plan", "plan"),
    ("ce-work", "implement"),
    ("ce-review", "review"),
    ("ce-verify", "verify"),
];

/// Seed one-line dispatch notes (ADR-0021 D6): when to use each stage.
const SEED_USE_WHEN: &[(&str, &str)] = &[
    (
        "ce-explore",
        "a codebase needs a cold-start bug sweep with fresh eyes",
    ),
    (
        "ce-brainstorm",
        "a rough objective needs sharpening into testable requirements",
    ),
    (
        "ce-review",
        "written code needs an independent defect review before verify",
    ),
    (
        "ce-plan",
        "requirements need expanding into a tracked, verifiable bead set",
    ),
    (
        "ce-work",
        "a target with a verify command needs building until it passes",
    ),
    (
        "ce-compound",
        "a verified solution may hold a durable learning",
    ),
    (
        "ce-debug",
        "a reported symptom needs a failing repro baseline",
    ),
    (
        "ce-verify",
        "a change needs independent proof against its VERIFY",
    ),
];

const SEED_READS: &[(&str, &[&str])] = &[
    ("ce-review", &["steps.arms"]),
    ("ce-plan", &["inputs.objective"]),
];

/// Seed effort knobs (ADR-0021 D3).
const SEED_EFFORT: &[(&str, &str)] = &[("ce-plan", "max")];

/// Stages no shipped workflow dispatches, and HOW they are reached instead
/// (ADR-0021 D6). Everything here must name a real seed stage; everything
/// else unreferenced stays a dead stage `workflow verify` fails on.
const SEED_REACH: &[(&str, StageReach)] = &[
    ("ce-explore", StageReach::Compose),
    ("ce-review", StageReach::Manual),
    ("ce-verify", StageReach::Manual),
    ("ce-debug", StageReach::Manual),
];

/// bd-ihiqo.4: old stage names that are true renames resolve to their ce-
/// stage for one release, with a notice at resolution time. A name that did
/// the same job under another name belongs here; one that was mechanics
/// belongs in [`RETIRED_STAGES`].
const STAGE_ALIASES: &[(&str, &str)] = &[
    ("fresh-eyes-explore", "ce-explore"),
    ("repo-map", "ce-explore"),
    ("diagnose-only", "ce-debug"),
    ("repro-baseline", "ce-debug"),
    ("ce-execute", "ce-work"),
    ("ce-simplify", "ce-work"),
    ("tdd-red", "ce-work"),
    ("minimal-fix", "ce-work"),
    ("ce-plan-review", "ce-review"),
    ("judge-arms", "ce-review"),
    ("plan-polish", "ce-plan"),
    ("ce-evaluate", "ce-verify"),
    ("prove-verify", "ce-verify"),
    ("wave-verify", "ce-verify"),
];

/// Old stage names that were mechanics, not work a stage prompt can do.
/// They resolve to nothing, and the lookup error names what replaced them.
const RETIRED_STAGES: &[(&str, &str)] = &[
    (
        "commit-and-push",
        "committing is part of ce-work; the captain lands with `flywheel ship land`, or run the ce-ship workflow",
    ),
    (
        "spawn-crew",
        "seats change only through `flywheel swarm apply`",
    ),
    (
        "wave-dispatch",
        "waves are engine steps: run the ce-wave workflow",
    ),
    (
        "wave-gate",
        "waves are engine steps: run the ce-wave workflow",
    ),
    (
        "wave-observe",
        "waves are engine steps: run the ce-wave workflow",
    ),
    ("seed-first-task", "run the ce-triage workflow"),
    ("prioritize-assign", "run the ce-triage workflow"),
    ("bv-triage", "run the ce-triage workflow"),
    ("baseline-measure", "run the ce-measure workflow"),
    ("measure-keep-or-discard", "run the ce-measure workflow"),
    ("resume-pickup", "run the ce-resume workflow"),
];

/// What replaced a retired stage name, for the lookup error.
pub fn retired_stage(name: &str) -> Option<&'static str> {
    RETIRED_STAGES
        .iter()
        .find(|(old, _)| *old == name)
        .map(|(_, pointer)| *pointer)
}

/// Seed repetition contracts (min, max, until).
const SEED_REPEAT: &[(&str, (u32, u32, &str))] = &[("ce-plan", (2, 5, "converged"))];

/// Review + verify seats run fresh and read-only (ADR-0021 D3).
const SEED_FRESH_READONLY: &[&str] = &["ce-review", "ce-verify"];

/// bd-ceiling-report-template-vz8a: the stages that hit or carry a ceiling
/// report gain [`CEILING_REPORT_TEMPLATE`]. `ce-execute` renders it (the
/// executor; the doorbell fold escalates it, bd-jhz8x.2) and `wave-verify`
/// accepts the amendment close that cites it. The retired wave-gate /
/// wave-dispatch / wave-observe stages are engine steps now, so they have no
/// prompt to carry it.
const CEILING_TEMPLATE_STAGES: &[&str] = &["ce-work", "ce-verify"];

/// bd-ceiling-report-template-vz8a (encode-lessons-in-structure,
/// minimize-reader-load): ONE ceiling shape for every pane, so the captain's
/// amendment close is mechanical — cite the inventory, file each residual as
/// its own bead, close — instead of re-deriving from free-form prose each
/// time. An honest ceiling is a report, never a falsified close.
const CEILING_REPORT_TEMPLATE: &str = "\
CEILING REPORT — an honest ceiling is a report, never a falsified close. When\nthe bead's acceptance is reachable only with work outside its scope, render\nEXACTLY this shape:\n  status: CEILING\n  done:\n    - <what shipped> (receipt <path>, commit <sha>)\n  residual inventory (one row per residual):\n    - <path> — <row or function> — <what remains> — enabler: module <the module that unblocks it> | machinery <the missing machinery> | none <no new structure needed; the next pass can finish it as-is>\n  recommendation: close with amendment + residual | continue\nThe captain's amendment close reads this inventory mechanically: cite it, file\neach residual as its own bead, close. A report missing any section is\nincomplete — the ceiling is the inventory, not the refusal.";

impl StageLibrary {
    /// Build the library: compiled-in defaults, then the repo DATA tier
    /// (`repo-stages.toml`, promotion-gated), the user `stages.toml`
    /// override, and — when `project_stages` is given — the project tier
    /// `<project>/.flywheel/stages.toml`. Later tiers win field-level.
    pub fn load() -> Result<Self> {
        Self::load_layered(None)
    }

    /// [`load`] plus the project tier (ADR-0022 §3.8): pass the project's
    /// `.flywheel/stages.toml` path when one may exist.
    pub fn load_layered(project_stages: Option<&std::path::Path>) -> Result<Self> {
        let mut lib = Self::compiled_in();
        lib.apply_embedded_repo_data()?;
        lib.override_from_toml()?;
        if let Some(path) = project_stages {
            lib.apply_toml_path(path, Tier::Project)?;
        }
        Ok(lib)
    }

    /// The compiled-in defaults (the former `STAGES` const) under their
    /// semantic names. The `ce-` family is the compound-engineering loop
    /// (ADR-0009); p-code aliases are retired.
    pub fn compiled_in() -> Self {
        let defaults: &[(&str, &str)] = &[
            (
                "ce-brainstorm",
                "Sharpen a rough objective into requirements before any work starts. Ground first: AGENTS.md, the relevant ADRs, and `flywheel memory check-trap \"<objective keywords>\"`. Ask the owner only questions whose answer changes the result, one at a time; never ask what the repo can answer. Write requirements, not implementation: the target, explicit non-goals, open questions each naming the decision it blocks, and what already exists to build on. Every acceptance names its evidence; a requirement you cannot test is a wish. Reply with ONLY {\"target\": \"...\", \"non_goals\": [\"...\"], \"open_questions\": [\"...\"], \"data_shape\": \"...\"}.",
            ),
            (
                "ce-plan",
                "Turn the requirements into the smallest set of beads that ships them. Each bead stands alone (background, why, acceptance) and names the command that proves it: `br create -d <description> --verify <command>`, plus `--principle \"<name> — <decision>\"` at P≤2; order them with `br dep add`. Keep a coverage map so no requirement is dropped. Each behaviour names its unit test and one end-to-end check. On a repeat pass, polish the set in place and set converged when a pass changes nothing. Reply with ONLY {\"beads\": [{\"id\": \"bd-…\", \"acceptance\": \"...\", \"verify\": \"...\"}], \"coverage\": {\"<requirement>\": [\"bd-…\"]}, \"converged\": true}.",
            ),
            (
                "ce-work",
                "Build the TARGET until its VERIFY command passes. Run VERIFY at HEAD first: already green means already done, so say so and stop. TDD: write the failing test, make it pass, prove the test can fail (a mutation or a deliberate break you revert), then make one simplifying pass and re-run VERIFY. Trace the change from its production caller; code nothing calls is not done. Your crew rules cover leases, committing only your paths and mailing the captain. When a bead is bound, claim it first (`br update <id> --claim --actor <your pin>`) and cite its id in the commit; the captain lands it with `ship land`, so never close it yourself. Never self-answer an ask-user finding: restoring accepted behaviour proceeds, an expansion parks for the captain. Reply with ONLY {\"commit_sha\": \"...\", \"commit_message\": \"...\", \"bead\": \"bd-… or null\", \"gate\": {\"command\": \"<VERIFY>\", \"exit\": 0, \"output\": \"<tail>\"}}.",
            ),
            (
                "ce-review",
                "Review the change for defects: wrong behaviour, bugs, security and reliability problems. Report only; never fix. A finding names its detection condition (what in the change triggers it), quoted evidence (file:line and the text), a severity P0–P3 and a confidence; unquoted high confidence is rejected. Two lenses in one pane are one witness, not two. SEAM CHECK: for each new or changed function, flag or field, find its production caller from the binary entry point and read the call. A test on code nothing in production calls proves nothing; report it as P1. Preferring another approach than a settled captain decision is report-only. An ask-user finding is never self-answered; it parks for the captain. Size the review to the risk: a sound change has no findings. When judging arms, read {{steps.arms}}. Reply with ONLY {\"findings\": [{\"id\", \"severity\", \"condition\", \"evidence\", \"confidence\"}], \"verdict\": \"AGREE|AGREE-WITH-CHANGES|REJECT\"}.",
            ),
            (
                "ce-verify",
                "Independently prove the change from a FRESH context and stay READ-ONLY: never re-implement. Run the VERIFY command yourself and record its exit code and output; a self-report is not proof. For a fix, also show the original repro is gone. SEAM CHECK: confirm the changed code is reached from its production caller by tracing the call from the binary entry point; a green unit test on unwired code is a fail. You are advisory: VerifySink is never the pass/fail authority, the gate exit is. Reply with ONLY {\"gate\": {\"command\": \"...\", \"exit\": 0, \"output\": \"<tail>\"}, \"seam\": \"<caller file:line>\", \"remaining_work\": [\"...\"], \"validation_evidence\": [\"...\"]}.",
            ),
            (
                "ce-compound",
                "Decide whether verified work left a durable learning, then capture at most one. All three must hold: non-obvious (not recoverable from the code, tests or types), durable (an invariant, root cause, rejected approach or convention), material (forgetting it would cause a repeat). Otherwise reply {\"terminal\": \"clean_skip\"}; effort and diff size are not evidence. Recall first, one labeled lane each: [wiki] `chie qmd search \"<q>\" --agent`, [memory] `chie memory recall \"<q>\"`, [traps] `flywheel memory check-trap \"<q>\"`. If a lesson already covers it, re-record it updated with reinforced: <today>; otherwise run `flywheel memory record-lesson \"<X> breaks when <Y>; do <Z>\"` citing the commit. Prefer a test or lint rule when one fits. Chiebukuro is the only learning store; pruning is `chie memory promote` / `chie memory forget` in ce-retro. A failed capture never blocks the run. Reply with ONLY {\"terminal\": \"recorded|clean_skip\", \"lesson\": \"...\"}.",
            ),
            (
                "ce-debug",
                "Diagnose the symptom; do not fix it. Reproduce first: find a command that shows the failure and record its exact output. Then find the root cause: instrument the live surface or read the artifact, and test each hypothesis against evidence. Read-only: no changes to the repo or the target. Reply with ONLY {\"repro\": \"<command>\", \"root_cause\": \"...\", \"evidence\": [\"file:line or output\"], \"confidence\": \"high|medium|low\"}.",
            ),
            (
                "ce-explore",
                "Explore the codebase with fresh eyes. Pick code files, trace their execution flow through what they import and what imports them, and learn what each does in the larger workflow. Then check critically for bugs, mistakes and wiring gaps: a function or flag nothing in production calls is a finding. Fix what you find under the repo's AGENTS.md rules, failing test first. Report each fix with its commit, and each unfixed problem with file:line evidence.",
            ),
        ];
        let mut stages = BTreeMap::new();
        for (name, prompt) in defaults {
            stages.insert(
                name.to_string(),
                Stage {
                    name: name.to_string(),
                    prompt: prompt.to_string(),
                    role: None,
                    phase: None,
                    principles: vec![],
                    author: None,
                    origin_run: None,
                    params: vec![],
                    mode: None,
                    context: None,
                    effort: None,
                    reads: vec![],
                    witness: None,
                    repeat: None,
                    use_when: None,
                    reached_by: None,
                },
            );
        }
        for (name, role) in SEED_AFFINITY {
            if let Some(stage) = stages.get_mut(*name) {
                stage.role = Some(role.to_string());
            }
        }
        for (name, phase) in SEED_PHASE {
            if let Some(stage) = stages.get_mut(*name) {
                stage.phase = Some(phase.to_string());
            }
        }
        for (name, witness) in SEED_WITNESS {
            if let Some(stage) = stages.get_mut(*name) {
                stage.witness = Some(witness.to_string());
            }
        }
        for (name, use_when) in SEED_USE_WHEN {
            if let Some(stage) = stages.get_mut(*name) {
                stage.use_when = Some(use_when.to_string());
            }
        }
        for (name, reach) in SEED_REACH {
            if let Some(stage) = stages.get_mut(*name) {
                stage.reached_by = Some(*reach);
            }
        }
        for (name, reads) in SEED_READS {
            if let Some(stage) = stages.get_mut(*name) {
                stage.reads = reads.iter().map(|r| r.to_string()).collect();
            }
        }
        for (name, effort) in SEED_EFFORT {
            if let Some(stage) = stages.get_mut(*name) {
                stage.effort = Some(effort.to_string());
            }
        }
        for (name, (min, max, until)) in SEED_REPEAT {
            if let Some(stage) = stages.get_mut(*name) {
                stage.repeat = Some(Repeat {
                    min: *min,
                    max: *max,
                    until: until.to_string(),
                });
            }
        }
        for name in SEED_FRESH_READONLY {
            if let Some(stage) = stages.get_mut(*name) {
                stage.context = Some(StageContext::Fresh);
                stage.mode = Some(StageMode::ReadOnly);
            }
        }
        for name in CEILING_TEMPLATE_STAGES {
            if let Some(stage) = stages.get_mut(*name) {
                stage.prompt.push_str("\n\n");
                stage.prompt.push_str(CEILING_REPORT_TEMPLATE);
            }
        }
        let tiers: BTreeMap<String, Tier> =
            stages.keys().map(|k| (k.clone(), Tier::Seed)).collect();
        Self { stages, tiers }
    }

    /// The compiled-in library, built once per process. Hot callers (workflow
    /// lowering stamps every workflow it resolves) must not rebuild ~40
    /// prompts on each call.
    pub fn shared() -> &'static StageLibrary {
        static LIB: std::sync::OnceLock<StageLibrary> = std::sync::OnceLock::new();
        LIB.get_or_init(Self::compiled_in)
    }

    /// The repo DATA tier: `repo-stages.toml` compiled into the binary
    /// (include_str!). Empty until a stage is promoted to the repo tier
    /// through the §3.8 evidence bar.
    fn apply_embedded_repo_data(&mut self) -> Result<()> {
        const REPO_STAGES_TOML: &str = include_str!("repo-stages.toml");
        self.apply_toml(REPO_STAGES_TOML, Tier::Repo)
    }

    /// Apply one tier's `stages.toml` (field-level merge, later tier wins).
    /// A tier entry carrying only some fields MERGES over the record below
    /// it: present fields win, absent fields inherit (bd-cfgv) — a
    /// prompt-only user override no longer silently wipes seed role,
    /// phase, principles, or the D1 required-params contract.
    fn apply_toml(&mut self, raw: &str, tier: Tier) -> Result<()> {
        #[derive(Deserialize)]
        struct TierStages {
            #[serde(default)]
            stages: BTreeMap<String, Stage>,
        }
        let parsed: TierStages =
            toml::from_str(raw).with_context(|| format!("parse {tier:?} stages.toml"))?;
        for (name, stage) in parsed.stages {
            self.tiers.insert(name.clone(), tier);
            match self.stages.remove(&name) {
                Some(base) => self.stages.insert(name, merge_stage(base, stage)),
                None => self.stages.insert(name, stage),
            };
        }
        Ok(())
    }

    /// Read + apply a tier file from disk. Missing file = empty tier (Ok).
    fn apply_toml_path(&mut self, path: &std::path::Path, tier: Tier) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        self.apply_toml(&raw, tier)
    }

    /// Apply one per-entry stage file. The file is a complete stage record;
    /// unlike the aggregate tier format, its filename is only a locator.
    fn apply_stage_entry_path(&mut self, path: &std::path::Path, tier: Tier) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let stage: Stage = toml::from_str(&raw)
            .with_context(|| format!("parse per-entry stage {}", path.display()))?;
        self.tiers.insert(stage.name.clone(), tier);
        self.stages.insert(stage.name.clone(), stage);
        Ok(())
    }

    /// Apply the user `stages.toml` override, then per-entry user files.
    /// Per-entry files are the preferred format and win over the legacy
    /// aggregate when both contain the same stage.
    fn override_from_toml(&mut self) -> Result<()> {
        let config_root = config_dir();
        self.apply_toml_path(&config_root.join("stages.toml"), Tier::User)?;
        let entries = config_root.join("stages");
        if !entries.is_dir() {
            return Ok(());
        }
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&entries)
            .with_context(|| format!("read {}", entries.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            self.apply_stage_entry_path(&path, Tier::User)?;
        }
        Ok(())
    }

    /// The tier a resolved name came from (§3.8 tier tags).
    pub fn tier_of(&self, name: &str) -> Option<Tier> {
        let canonical = self.canonical(name);
        self.tiers.get(&canonical).copied()
    }

    /// Canonical name (identity — ADR-0009 retired the alias indirection).
    fn canonical(&self, name: &str) -> String {
        STAGE_ALIASES
            .iter()
            .find(|(old, _)| *old == name)
            .map(|(_, new)| (*new).to_string())
            .unwrap_or_else(|| name.to_string())
    }

    /// The full stage record for a name.
    pub fn get_stage(&self, name: &str) -> Option<&Stage> {
        let canonical = self.canonical(name);
        if canonical != name {
            eprintln!("flywheel: stage '{name}' is deprecated; use '{canonical}' (one release)");
        }
        self.stages.get(&canonical)
    }

    /// Build the durable-body witness seam from this catalog: unknown
    /// stages and witness-less stages yield None, so the step settles
    /// unchanged; a declared witness runs `check_witness` on the output.
    pub fn witness_checker(&self) -> crate::durable_body::WitnessCheck {
        let stages = self.stages.clone();
        std::sync::Arc::new(move |stage: &str, output: &serde_json::Value| {
            let witness = stages.get(stage)?.witness.clone()?;
            Some(crate::phase_contracts::check_witness(&witness, output))
        })
    }

    /// Fail-closed seam for an unloadable catalog: every staged step is
    /// refused with the load error. A broken catalog never reads as
    /// "no witnesses".
    pub fn witness_checker_unavailable(reason: String) -> crate::durable_body::WitnessCheck {
        std::sync::Arc::new(move |_stage: &str, _output: &serde_json::Value| {
            Some(crate::phase_contracts::PhaseContractReport {
                phase: "witness",
                ok: false,
                evidence: format!("stage catalog unavailable: {reason}"),
            })
        })
    }

    /// Stage names that DECLARE how they are reached without a workflow
    /// (ADR-0021 D6). The verify gate counts these as referenced.
    pub fn declared_reach(&self) -> Vec<String> {
        self.stages
            .values()
            .filter(|s| s.reached_by.is_some())
            .map(|s| s.name.clone())
            .collect()
    }

    /// One `stage list --all` row: name + tier + the §3.8 metadata.
    pub fn list_tiered(&self) -> Vec<StageRow> {
        self.stages
            .values()
            .filter_map(|s| {
                Some(StageRow {
                    name: s.name.clone(),
                    tier: self.tiers.get(&s.name).copied()?,
                    role: s.role.clone(),
                    phase: s.phase.clone(),
                })
            })
            .collect()
    }

    /// Resolve a stage name to its prompt.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        self.stages
            .get(&self.canonical(name))
            .map(|s| s.prompt.as_str())
    }

    /// True when `name` resolves.
    pub fn has(&self, name: &str) -> bool {
        self.resolve(name).is_some()
    }

    /// All stage names — for `flywheel stage list`.
    pub fn list(&self) -> Vec<&str> {
        self.stages.values().map(|s| s.name.as_str()).collect()
    }
}

/// The user override path, preferring the per-entry directory.
pub fn override_path() -> Option<PathBuf> {
    let entries = config_dir().join("stages");
    if entries.is_dir() {
        Some(entries)
    } else {
        let path = config_dir().join("stages.toml");
        path.is_file().then_some(path)
    }
}

/// One `stage list` row (§3.8: tier-tagged, phase + role affinity).
#[derive(Debug, Clone, Serialize)]
pub struct StageRow {
    pub name: String,
    pub tier: Tier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

/// Scaffold a new stage (§3.8 `stage new`): named body with role/phase/
/// principles frontmatter slots left for the author. Pure.
pub fn scaffold(name: &str) -> Stage {
    Stage {
        name: name.to_string(),
        prompt: format!("TODO: the {name} prompt body."),
        role: None,
        phase: None,
        principles: vec![],
        author: None,
        origin_run: None,
        params: vec![],
        mode: None,
        context: None,
        effort: None,
        reads: vec![],
        witness: None,
        repeat: None,
        use_when: None,
        reached_by: None,
    }
}

/// Merge a stage into a tier file on disk (`stage new` project tier,
/// `stage promote --to user`). Creates the file when missing; an existing
/// same-name entry is REPLACED (field-level tier semantics).
pub fn upsert_stage_in_file(path: &std::path::Path, stage: &Stage) -> Result<()> {
    #[derive(Default, Deserialize, Serialize)]
    struct FileStages {
        #[serde(default)]
        stages: BTreeMap<String, Stage>,
    }
    let mut file = FileStages::default();
    if path.is_file() {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        file = toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    }
    file.stages.insert(stage.name.clone(), stage.clone());
    let out = toml::to_string(&file).context("serialize stages.toml")?;
    crate::project_state::write_atomic(path, out.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

/// Write one complete stage record to the preferred per-entry user format.
pub fn upsert_stage_entry(path: &std::path::Path, stage: &Stage) -> Result<()> {
    let rendered = render_stage_entry(stage)?;
    crate::project_state::write_atomic(path, rendered.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

/// The one and only way a per-entry stage file is rendered.
///
/// bd-3o5t follow-on: `workflow seed --overwrite` rewrites these files in
/// place, so the create path and the overwrite path must not be able to produce
/// different bytes for the same stage — otherwise a fresh seed and a restored
/// backup would disagree, and drift would call a correct file stale.
pub fn render_stage_entry(stage: &Stage) -> Result<String> {
    toml::to_string(stage).context("serialize per-entry stage")
}

/// Lint one stage against the §3.8 rules: shape, size cap, affinity validity
/// (must name a catalog profile), phase validity (§4 vocabulary), principle
/// validity (ADR-0019 registry). Returns issue strings; empty = clean.
/// Pure — the caller supplies the valid-role names.
pub fn lint_stage(stage: &Stage, valid_roles: &[String]) -> Vec<String> {
    let mut issues = Vec::new();
    let label = format!("stage {}", stage.name);
    if stage.name.trim().is_empty() {
        issues.push(format!("{label}: empty name"));
    }
    if stage.prompt.trim().is_empty() {
        issues.push(format!("{label}: empty prompt"));
    }
    if stage.prompt.len() > PROMPT_SIZE_CAP {
        issues.push(format!(
            "{label}: prompt {} bytes exceeds the {} cap",
            stage.prompt.len(),
            PROMPT_SIZE_CAP
        ));
    }
    if let Some(role) = &stage.role
        && !valid_roles.iter().any(|r| r == role)
    {
        issues.push(format!(
            "{label}: unknown role affinity {role:?} (not a catalog profile)"
        ));
    }
    if let Some(phase) = &stage.phase
        && !PHASES.contains(&phase.as_str())
    {
        issues.push(format!(
            "{label}: unknown phase {phase:?} (valid: {})",
            PHASES.join(", ")
        ));
    }
    if let Some(witness) = &stage.witness
        && !crate::phase_contracts::WITNESSES.contains(&witness.as_str())
    {
        issues.push(format!(
            "{label}: unknown witness {witness:?} (valid: {})",
            crate::phase_contracts::WITNESSES.join(", ")
        ));
    }
    for p in &stage.principles {
        if !crate::principles::is_known_name(p) {
            issues.push(format!(
                "{label}: unknown principle {p:?} (ADR-0019 registry)"
            ));
        }
    }
    if stage.phase.as_deref().is_none_or(|p| p.trim().is_empty()) {
        issues.push(format!(
            "{label}: missing phase (valid: {})",
            PHASES.join(", ")
        ));
    }
    if let Some(repeat) = &stage.repeat {
        for e in validate_repeat(repeat) {
            issues.push(format!("{label}: {e}"));
        }
    }
    issues
}

/// Repeat bounds: 1 <= min <= max <= 32 (ADR-0021 D3). Pure.
#[must_use]
pub fn validate_repeat(repeat: &Repeat) -> Vec<String> {
    let mut issues = Vec::new();
    if repeat.min < 1 {
        issues.push(format!("repeat min {} < 1", repeat.min));
    }
    if repeat.max > 32 {
        issues.push(format!("repeat max {} > 32", repeat.max));
    }
    if repeat.min > repeat.max {
        issues.push(format!("repeat min {} > max {}", repeat.min, repeat.max));
    }
    if repeat.until.trim().is_empty() {
        issues.push("repeat until is empty".to_string());
    }
    issues
}

/// Tier-aware stage lint (ADR-0021 D3): missing `phase` is an ERROR on seed
/// and repo tiers, a WARNING on user and project tiers (operator stage
/// files predate the field). Returns (errors, warnings). Pure.
#[must_use]
pub fn lint_stage_tiered(
    stage: &Stage,
    valid_roles: &[String],
    tier: Tier,
) -> (Vec<String>, Vec<String>) {
    let issues = lint_stage(stage, valid_roles);
    match tier {
        Tier::Seed | Tier::Repo => (issues, Vec::new()),
        Tier::User | Tier::Project => {
            let (warn, errors): (Vec<String>, Vec<String>) = issues
                .into_iter()
                .partition(|i| i.contains("missing phase"));
            (errors, warn)
        }
    }
}

/// The pure reads gate (ADR-0021 D3): every `inputs.*` read must name a
/// declared input root; every `steps.*` read must name a prior step.
/// Anything else must exactly match a prior step. Returns the missing reads.
/// Pure — the workflow lint calls this once per dispatch step.
#[must_use]
pub fn reads_unsatisfied(
    stage_reads: &[String],
    declared_inputs: &[String],
    prior_steps: &[String],
) -> Vec<String> {
    let mut missing = Vec::new();
    for read in stage_reads {
        if let Some(key) = read.strip_prefix("inputs.") {
            let root = key.split('.').next().unwrap_or(key);
            if !declared_inputs.iter().any(|d| d == root) {
                missing.push(read.clone());
            }
        } else if let Some(key) = read.strip_prefix("steps.") {
            let root = key.split('.').next().unwrap_or(key);
            if !prior_steps.iter().any(|s| s == root) {
                missing.push(read.clone());
            }
        } else if !prior_steps.iter().any(|s| s == read) {
            missing.push(read.clone());
        }
    }
    missing
}

/// Pure `--phase` filter for `stage list`: keep rows serving one phase.
#[must_use]
pub fn filter_rows_by_phase(rows: &[StageRow], phase: &str) -> Vec<StageRow> {
    rows.iter()
        .filter(|r| r.phase.as_deref() == Some(phase))
        .cloned()
        .collect()
}

/// Pure `--unused` set difference: names with no catalog `stage:` reference.
#[must_use]
pub fn unreferenced_stages(
    all: &[String],
    referenced: &std::collections::HashSet<String>,
) -> Vec<String> {
    // bd-ihiqo.4: compare canonical names on both sides. Workflows still name
    // the old stages for one release, and the operator's tiers still carry
    // them; neither should make a live canonical stage look unused.
    let canonical = |n: &str| {
        STAGE_ALIASES
            .iter()
            .find(|(old, _)| *old == n)
            .map(|(_, new)| (*new).to_string())
            .unwrap_or_else(|| n.to_string())
    };
    let wanted: std::collections::HashSet<String> =
        referenced.iter().map(|n| canonical(n)).collect();
    let out: std::collections::BTreeSet<String> = all
        .iter()
        .map(|n| canonical(n))
        .filter(|n| !wanted.contains(n))
        .collect();
    out.into_iter().collect()
}

/// bd-jhz8x.4 (ADR-0021 D6): an unused stage fails `workflow verify`.
/// Pure: `recent_stage_uses` is an INPUT (the CLI reads the run log; the proof
/// has no history, and a gate whose answer depends on the operator's runs is
/// ambient state). A stage is unused when the catalog never references it and
/// nothing used it in the window.
#[must_use]
pub fn unused_stages(
    all: &[String],
    referenced: &std::collections::HashSet<String>,
) -> Vec<String> {
    unreferenced_stages(all, referenced)
}

/// Collect every `stage:` value in a workflow JSON tree (spec.stage at any
/// depth: steps, graph nodes, loop bodies). Pure — feeds `--unused`.
/// Templated values ride along verbatim; they never match a seed name.
pub fn collect_stage_refs(value: &serde_json::Value, out: &mut std::collections::HashSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                if key == "stage"
                    && let Some(name) = nested.as_str()
                {
                    out.insert(name.to_string());
                }
                collect_stage_refs(nested, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_stage_refs(item, out);
            }
        }
        _ => {}
    }
}

/// The §3.8 repo-tier evidence bar: green across at least two distinct
/// objectives or projects, PLUS a review bead. Projects are not recorded in
/// the run log today, so the check enforces the objectives arm (at least 2
/// distinct prompt hashes among successful runs) and requires the review bead —
/// refusal is the honest default when evidence cannot be proven.
pub fn check_repo_evidence(distinct_objective_greens: usize, review_bead_present: bool) -> bool {
    distinct_objective_greens >= 2 && review_bead_present
}

/// Parse repeatable `--param k=v` argv pairs into a map. A pair without `=`
/// is a usage error naming the offending argument.
pub fn parse_params(raw: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for pair in raw {
        let Some((k, v)) = pair.split_once('=') else {
            return Err(anyhow::anyhow!("--param {pair:?} is not k=v (missing '=')"));
        };
        if k.is_empty() || v.is_empty() {
            return Err(anyhow::anyhow!("--param {pair:?} needs both k and v"));
        }
        if out.insert(k.to_string(), v.to_string()).is_some() {
            return Err(anyhow::anyhow!(
                "--param {k} given twice with different values"
            ));
        }
    }
    Ok(out)
}

/// Render a stage prompt through the D1 substitution engine.
///
/// - Every declared required param must be present in `params`, else a usage
///   error NAMING each missing slot.
/// - Each provided param substitutes its `{{k}}` slots verbatim (prompt text,
///   never shell interpolation).
/// - No params + no declared params → byte-identical passthrough.
pub fn render_stage(
    prompt: &str,
    required: &[String],
    params: &BTreeMap<String, String>,
) -> Result<String> {
    let missing: Vec<&str> = required
        .iter()
        .map(std::string::String::as_str)
        .filter(|r| !params.contains_key(*r))
        .collect();
    if !missing.is_empty() {
        return Err(anyhow::anyhow!(
            "stage requires --param for missing slot(s): {}",
            missing.join(", ")
        ));
    }
    let mut out = prompt.to_string();
    for (k, v) in params {
        let slot = format!("{{{{{k}}}}}");
        if prompt.contains(&slot) {
            out = out.replace(&slot, v);
        } else if k != "task_id" {
            eprintln!(
                "flywheel: warn: --param {k} matches no {{{{{k}}}}} slot in stage — value ignored"
            );
        }
    }
    Ok(out)
}

/// Learning corpus decay model (bd-fmya). Frontmatter carries
/// `tier: pinned|aging|perishable` + `reinforced: YYYY-MM-DD` (perishable
/// docs additionally name a checkable `expires:`). The GATE grounding read
/// is a BUDGETED PROJECTION of `chie/`, never the corpus itself
/// (guard-the-context-window); decay is frontmatter, not a prompt
/// (encode-lessons-in-structure). Archiving is ce-sweep's job — this layer
/// only ranks + bounds, never deletes.
pub mod learning {
    use chrono::NaiveDate;

    /// Default GATE grounding budget (~4k tokens).
    pub const FLYWHEEL_GATE_BUDGET: usize = 4000;
    /// aging: drops out of the projection when unreinforced beyond this.
    pub const AGING_TTL_DAYS: i64 = 30;
    /// perishable: drops out beyond this; the doc must name a checkable
    /// `expires:` (ce-sweep archives it when past).
    pub const PERISHABLE_TTL_DAYS: i64 = 7;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
    pub enum LearningTier {
        /// Never decays out of the projection (invariants).
        Pinned,
        /// Default (migration backfill); decays after `AGING_TTL_DAYS`
        /// unreinforced.
        Aging,
        /// Short-lived; decays after `PERISHABLE_TTL_DAYS` unreinforced.
        Perishable,
    }

    /// Grounding lane (bd-0km8): which return-arrow source an entry came
    /// from. Lanes compete for ONE shared budget inside [`gate_projection`]
    /// — the cap is never per-lane.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
    #[serde(rename_all = "lowercase")]
    pub enum LearningLane {
        /// `chie/` corpus files.
        Corpus,
        /// Curated wiki pages (chiebukuro knowledge plane export).
        Wiki,
        /// Prior session decisions (chiebukuro memory plane export).
        Memory,
    }

    impl LearningLane {
        /// Lowercase lane name for reports and per-lane counts.
        pub fn name(&self) -> &'static str {
            match self {
                Self::Corpus => "corpus",
                Self::Wiki => "wiki",
                Self::Memory => "memory",
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
    pub struct LearningEntry {
        pub path: String,
        pub tier: LearningTier,
        /// `reinforced: YYYY-MM-DD` (None = never re-verified).
        pub reinforced: Option<String>,
        /// Estimated token cost of grounding on this entry (frontmatter +
        /// rg context). The projection sums these under the budget.
        pub cost_tokens: usize,
        /// Source lane — see [`LearningLane`]. Defaults to corpus at
        /// construction sites that predate lanes.
        pub lane: LearningLane,
    }

    /// Default grounding cost for lane-file entries without an explicit
    /// `cost_tokens`: the same ~40-line rg-context allowance the corpus
    /// arm uses.
    pub const LANE_DEFAULT_COST_TOKENS: usize = 150;

    /// Parse a lane export file (bd-0km8): a JSON array of
    /// `{path, tier?, reinforced?, cost_tokens?}` objects. Returns
    /// `(entries, skipped)` — malformed rows skip (the caller warns; the
    /// gate never fails a wave on a bad lane file). Unknown tier strings
    /// fall back to Aging, matching [`parse_learning_frontmatter`].
    pub fn parse_lane_entries(json: &str, lane: LearningLane) -> (Vec<LearningEntry>, usize) {
        let mut out = Vec::new();
        let mut skipped = 0usize;
        let value: serde_json::Value = match serde_json::from_str(json) {
            Ok(v) => v,
            Err(_) => return (out, 1),
        };
        let arr = match value.as_array() {
            Some(a) => a,
            None => return (out, 1),
        };
        for row in arr {
            let Some(path) = row.get("path").and_then(|p| p.as_str()) else {
                skipped += 1;
                continue;
            };
            let tier = match row.get("tier").and_then(|t| t.as_str()).unwrap_or("aging") {
                "pinned" => LearningTier::Pinned,
                "perishable" => LearningTier::Perishable,
                _ => LearningTier::Aging,
            };
            let reinforced = row
                .get("reinforced")
                .and_then(|r| r.as_str())
                .map(str::to_string);
            let cost_tokens = row
                .get("cost_tokens")
                .and_then(|c| c.as_u64())
                .map(|c| c as usize)
                .unwrap_or(LANE_DEFAULT_COST_TOKENS);
            out.push(LearningEntry {
                path: path.to_string(),
                tier,
                reinforced,
                cost_tokens,
                lane,
            });
        }
        (out, skipped)
    }

    /// Parse `tier` + `reinforced` from a learning file's frontmatter
    /// (dashes block optional). Unknown/missing tier = Aging (the
    /// migration default); missing reinforced = None.
    pub fn parse_learning_frontmatter(src: &str) -> Option<(LearningTier, Option<String>)> {
        let block = src.strip_prefix("---\n")?;
        let fm = block.split("\n---").next()?;
        let mut tier = None;
        let mut reinforced = None;
        for line in fm.lines() {
            if let Some(v) = line.strip_prefix("tier:") {
                tier = Some(match v.trim() {
                    "pinned" => LearningTier::Pinned,
                    "perishable" => LearningTier::Perishable,
                    _ => LearningTier::Aging,
                });
            } else if let Some(v) = line.strip_prefix("reinforced:") {
                reinforced = Some(v.trim().to_string());
            }
        }
        Some((tier.unwrap_or(LearningTier::Aging), reinforced))
    }

    fn age_days(reinforced: &str, today: NaiveDate) -> Option<i64> {
        let r = NaiveDate::parse_from_str(reinforced, "%Y-%m-%d").ok()?;
        Some((today - r).num_days())
    }

    /// The GATE projection: corpus IN (ranking over a borrowed slice), a
    /// bounded ranked list OUT — top-N by reinforcement recency within
    /// `budget_tokens`. pinned entries never age out; aging/perishable
    /// decay past their TTL unreinforced; never-reinforced entries rank
    /// last but stay eligible ONLY when pinned.
    pub fn gate_projection(
        entries: &[LearningEntry],
        today: NaiveDate,
        budget_tokens: usize,
    ) -> Vec<&LearningEntry> {
        let mut eligible: Vec<&LearningEntry> = entries
            .iter()
            .filter(|e| match e.tier {
                LearningTier::Pinned => true,
                LearningTier::Aging => e
                    .reinforced
                    .as_deref()
                    .and_then(|r| age_days(r, today))
                    .is_some_and(|age| age <= AGING_TTL_DAYS),
                LearningTier::Perishable => e
                    .reinforced
                    .as_deref()
                    .and_then(|r| age_days(r, today))
                    .is_some_and(|age| age <= PERISHABLE_TTL_DAYS),
            })
            .collect();
        eligible.sort_by(|a, b| b.reinforced.as_deref().cmp(&a.reinforced.as_deref()));
        let mut out = Vec::new();
        let mut spent = 0usize;
        for e in eligible {
            if spent + e.cost_tokens > budget_tokens {
                continue;
            }
            spent += e.cost_tokens;
            out.push(e);
        }
        out
    }
}

/// bd-ihiqo.4: the 8-stage cutover keeps one release of old names as
/// aliases, so a running workflow does not break mid-release.
#[test]
fn old_stage_names_resolve_as_aliases_for_one_release() {
    let lib = StageLibrary::compiled_in();
    assert_eq!(lib.stages.len(), 8, "the canonical seed has 8 stages");
    assert_eq!(lib.canonical("ce-execute"), "ce-work");
    assert_eq!(lib.canonical("prove-verify"), "ce-verify");
    assert_eq!(lib.canonical("fresh-eyes-explore"), "ce-explore");
    assert!(lib.has("ce-execute"), "the old name still resolves");
    assert!(lib.get_stage("ce-execute").is_some());
}
#[cfg(test)]
mod adr0010_d12_tests {
    // governed-by: ADR-0010 D12 (bd-fmya) — learnings tier/reinforced +
    use super::learning::{
        LearningEntry, LearningLane, LearningTier, gate_projection, parse_lane_entries,
        parse_learning_frontmatter,
    };
    use chrono::NaiveDate;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date")
    }

    fn entry(
        path: &str,
        tier: LearningTier,
        reinforced: Option<&str>,
        tokens: usize,
    ) -> LearningEntry {
        LearningEntry {
            path: path.to_string(),
            tier,
            reinforced: reinforced.map(String::from),
            cost_tokens: tokens,
            lane: LearningLane::Corpus,
        }
    }

    fn lane_entry(path: &str, lane: LearningLane, tokens: usize) -> LearningEntry {
        LearningEntry {
            path: path.to_string(),
            tier: LearningTier::Pinned,
            reinforced: Some("2026-09-24".to_string()),
            cost_tokens: tokens,
            lane,
        }
    }

    /// bd-0km8: the 4k budget is SHARED across lanes — corpus + wiki +
    /// memory entries compete for one pool, and the pool never exceeds
    /// the cap no matter how many lanes feed it.
    #[test]
    fn gate_budget_is_shared_across_corpus_wiki_memory_lanes() {
        let today = d("2026-09-24");
        let entries = vec![
            lane_entry("chie/a.md", LearningLane::Corpus, 2000),
            lane_entry("wiki:attention", LearningLane::Wiki, 2000),
            lane_entry("memory:conclusion-1", LearningLane::Memory, 2000),
        ];
        let projected = gate_projection(&entries, today, 4000);
        let spent: usize = projected.iter().map(|e| e.cost_tokens).sum();
        assert!(spent <= 4000, "shared pool never exceeds cap: {spent}");
        assert_eq!(
            projected.len(),
            2,
            "two 2000-token entries fit, third loses"
        );
        let lanes: Vec<LearningLane> = projected.iter().map(|e| e.lane).collect();
        assert!(
            lanes.contains(&LearningLane::Wiki) || lanes.contains(&LearningLane::Memory),
            "lanes compete on equal footing, no lane starved by kind: {lanes:?}"
        );
    }

    /// bd-0km8: lane-file parsing tags every entry and skips malformed
    /// rows loudly (counted, never silent).
    #[test]
    fn lane_files_parse_and_count_skips() {
        let (entries, skipped) = parse_lane_entries(
            r#"[
                {"path": "wiki:attention", "tier": "pinned", "reinforced": "2026-09-24", "cost_tokens": 120},
                {"path": "memory:c1", "cost_tokens": 80},
                {"nope": true},
                {"path": "wiki:x", "tier": "nonsense"}
            ]"#,
            LearningLane::Memory,
        );
        assert_eq!(skipped, 1, "one row without a path skips");
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.lane == LearningLane::Memory));
        assert_eq!(
            entries[1].tier,
            LearningTier::Aging,
            "tier defaults to Aging"
        );
        assert_eq!(entries[1].cost_tokens, 80);
        assert_eq!(
            entries[2].tier,
            LearningTier::Aging,
            "unknown tier falls back"
        );
        let (none, skipped_all) = parse_lane_entries("not json", LearningLane::Wiki);
        assert_eq!(none, [] as [LearningEntry; 0]);
        assert_eq!(skipped_all, 1);
    }

    #[test]
    fn adr0010_d12_projection_bounded_under_budget_with_50_seeded() {
        let today = d("2026-09-07");
        let entries: Vec<LearningEntry> = (0..50)
            .map(|i| {
                entry(
                    &format!("chie/k/l{i:02}.md"),
                    LearningTier::Aging,
                    Some("2026-09-07"),
                    100,
                )
            })
            .collect();
        let out = gate_projection(&entries, today, 500);
        assert_eq!(out.len(), 5, "projection stops at the budget");
        assert!(
            out.iter()
                .all(|e| e.reinforced.as_deref() == Some("2026-09-07"))
        );
    }

    #[test]
    fn adr0010_d12_unreinforced_40d_aging_entry_absent_from_projection() {
        let today = d("2026-09-07");
        let entries = vec![
            entry(
                "chie/k/stale.md",
                LearningTier::Aging,
                Some("2026-07-29"),
                100,
            ),
            entry(
                "chie/k/fresh.md",
                LearningTier::Aging,
                Some("2026-09-01"),
                100,
            ),
        ];
        let out = gate_projection(&entries, today, 10_000);
        assert!(
            !out.iter().any(|e| e.path.contains("stale")),
            "40d aging decays out"
        );
        assert!(out.iter().any(|e| e.path.contains("fresh")));
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn adr0010_d12_tier_matrix() {
        let today = d("2026-09-07");
        let entries = vec![
            entry("chie/k/pin.md", LearningTier::Pinned, None, 100),
            entry(
                "chie/k/edge30.md",
                LearningTier::Aging,
                Some("2026-08-08"),
                100,
            ),
            entry(
                "chie/k/perish-fresh.md",
                LearningTier::Perishable,
                Some("2026-09-03"),
                100,
            ),
            entry(
                "chie/k/perish-old.md",
                LearningTier::Perishable,
                Some("2026-08-20"),
                100,
            ),
        ];
        let out = gate_projection(&entries, today, 10_000);
        let paths: Vec<&str> = out.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"chie/k/pin.md"));
        assert!(paths.contains(&"chie/k/edge30.md"));
        assert!(paths.contains(&"chie/k/perish-fresh.md"));
        assert!(!paths.contains(&"chie/k/perish-old.md"));
        assert_eq!(paths[0], "chie/k/perish-fresh.md");
    }

    #[test]
    fn adr0010_d12_frontmatter_parse() {
        let fm = "---\ntrack: bug\ntier: aging\nreinforced: 2026-09-07\n---\n# body";
        let (tier, reinforced) = parse_learning_frontmatter(fm).expect("parses");
        assert_eq!(tier, LearningTier::Aging);
        assert_eq!(reinforced, Some("2026-09-07".to_string()));
        let fm2 = "---\ntrack: bug\n---\n# body";
        let (tier2, r2) = parse_learning_frontmatter(fm2).expect("parses");
        assert_eq!(tier2, LearningTier::Aging);
        assert_eq!(r2, None);
        assert!(parse_learning_frontmatter("# just a doc").is_none());
    }

    #[test]
    fn adr0010_d12_unknown_tier_string_is_aging_not_panic() {
        let fm = "---\ntier: broadcast\n---\n# body";
        let (tier, _) = parse_learning_frontmatter(fm).expect("parses");
        assert_eq!(tier, LearningTier::Aging);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reserved D1 payload slots (ADR-0006 §3 D1). `bead` is
    /// production-consumed (cmd_send headers every dispatch with it);
    /// the rest substitute into templates that declare them.
    const RESERVED_SLOTS: &[&str] = &["bead", "scope", "exit", "channel", "pin", "report_file"];

    /// `--param k=v` parses into a map; a pair without `=` is an error naming
    /// the offending argument.
    #[test]
    fn stage_param_parse_k_v_and_reject_bare() {
        let raw = vec![
            "bead=br-123".to_string(),
            "scope=src/main.rs,integrations/**".to_string(),
        ];
        let m = parse_params(&raw).expect("k=v pairs parse");
        assert_eq!(m.get("bead").map(String::as_str), Some("br-123"));
        assert_eq!(
            m.get("scope").map(String::as_str),
            Some("src/main.rs,integrations/**")
        );
        let bare = vec!["nobang".to_string()];
        let err = format!("{}", parse_params(&bare).unwrap_err());
        assert!(err.contains("nobang"), "error names the bad pair: {err}");
        let empty_v = vec!["k=".to_string()];
        assert!(parse_params(&empty_v).is_err(), "empty value rejected");
    }

    /// `{{slot}}` substitution is verbatim (prompt text, not argv) and hits
    /// repeated occurrences of the same slot.
    #[test]
    fn stage_param_substitutes_slots_verbatim() {
        let prompt = "Work {{bead}} in {{scope}}; report on {{channel}}; bead again: {{bead}}.";
        let params = parse_params(&[
            "bead=bd-x-1".to_string(),
            "scope=a,b".to_string(),
            "channel=adr0006-steer".to_string(),
        ])
        .expect("params parse");
        let out = render_stage(prompt, &[], &params).expect("renders");
        assert_eq!(
            out,
            "Work bd-x-1 in a,b; report on adr0006-steer; bead again: bd-x-1."
        );
    }

    /// A declared required param that was not supplied is a usage error
    /// NAMING each missing slot.
    #[test]
    fn stage_param_missing_required_names_slot() {
        let prompt = "do {{bead}} within {{exit}}";
        let required = vec!["bead".to_string(), "exit".to_string()];
        let params = parse_params(&["bead=br-1".to_string()]).expect("parses");
        let err = format!("{}", render_stage(prompt, &required, &params).unwrap_err());
        assert!(err.contains("exit"), "missing slot named in error: {err}");
        assert!(!err.contains("bead,"), "supplied param not flagged: {err}");
    }

    /// Zero params + no declared required params → byte-identical passthrough
    /// for EVERY compiled-in stage (the golden back-compat bar).
    #[test]
    fn stage_param_zero_param_byte_identical_golden() {
        let lib = StageLibrary::compiled_in();
        for name in lib.list() {
            let stage = lib.get_stage(name).expect("stage exists");
            let empty = BTreeMap::new();
            let out = render_stage(&stage.prompt.clone(), &stage.params, &empty)
                .unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(out, stage.prompt, "{name} zero-param byte-identical");
        }
    }

    /// The reserved D1 slot vocabulary is present ({{bead}} … {{report_file}}).
    #[test]
    fn stage_param_reserved_slot_vocabulary() {
        for slot in RESERVED_SLOTS {
            let rendered = render_stage(
                &format!("x {{{{{slot}}}}} y"),
                &[],
                &parse_params(&[format!("{slot}=v")]).expect("parses"),
            )
            .expect("renders");
            assert_eq!(rendered, "x v y", "{{{slot}}} substitutes");
        }
    }

    /// ADR-0009: the p-code aliases are RETIRED — `p6` no longer resolves;
    /// `ce-work` is the name (clean cutover, no shims).
    #[test]
    fn stage_p_codes_retired_ce_names_resolve() {
        let lib = StageLibrary::compiled_in();
        for code in ["p1", "p2", "p4", "p5", "p6", "p8"] {
            assert!(lib.resolve(code).is_none(), "{code} must not resolve");
        }
        assert!(lib.has("ce-work"), "ce-work resolves");
    }

    /// ADR-0009 contracts that stay in the stage prompts; the lease, commit
    /// and landing lifecycle lives in the crew skill, not here.
    #[test]
    fn ce_work_and_review_keep_their_adr0009_contracts() {
        let lib = StageLibrary::compiled_in();
        let work = lib.resolve("ce-work").expect("ce-work resolves");
        for needle in [
            "already green means already done",
            "prove the test can fail",
            "production caller",
        ] {
            assert!(work.contains(needle), "ce-work must carry {needle:?}");
        }
        let review = lib.resolve("ce-review").expect("ce-review resolves");
        for needle in [
            "detection condition",
            "quoted evidence",
            "one witness, not two",
        ] {
            assert!(review.contains(needle), "ce-review must carry {needle:?}");
        }
    }

    /// bd-ceiling-report-template-vz8a: ONE ceiling shape, rendered into the
    /// stages that hit (ce-work) and carry (ce-verify accepts the
    /// amendment close) a ceiling. Pinned on
    /// `compiled_in()` — the tier this change edits; a user-tier copy
    /// legitimately differs and `load()` would flake on machines holding one.
    #[test]
    fn ceiling_template_renders() {
        for needle in [
            "status: CEILING",
            "done:",
            "residual inventory",
            "enabler: module",
            "| machinery ",
            "| none ",
            "recommendation: close with amendment + residual",
            "| continue",
        ] {
            assert!(
                CEILING_REPORT_TEMPLATE.contains(needle),
                "ceiling template must carry {needle:?}: {CEILING_REPORT_TEMPLATE}"
            );
        }
        let lib = StageLibrary::compiled_in();
        for stage in CEILING_TEMPLATE_STAGES {
            let prompt = lib
                .resolve(stage)
                .unwrap_or_else(|| panic!("{stage} resolves"));
            assert!(
                prompt.contains(CEILING_REPORT_TEMPLATE),
                "{stage} must render the ceiling template"
            );
        }
    }

    /// ADR-0009: the compiled-in library exposes the ce- loop — brainstorm,
    /// plan, plan-review, execute, simplify, review, compound — plus
    /// ce-work.
    #[test]
    fn compiled_in_exposes_ce_loop() {
        let lib = StageLibrary::compiled_in();
        for name in [
            "ce-brainstorm",
            "ce-plan",
            "ce-review",
            "ce-work",
            "ce-work",
            "ce-review",
            "ce-verify",
            "ce-compound",
            "ce-work",
        ] {
            assert!(lib.has(name), "compiled-in stage {name} resolves");
        }
        assert!(
            PHASES.contains(&"compound"),
            "compound is phase vocabulary (ADR-0009)"
        );
    }

    /// ADR-0009: the ce- loop stages carry seed phase metadata matching the
    /// loop order, and ce-compound is the compound phase.
    #[test]
    fn ce_loop_seed_phases_tagged() {
        let lib = StageLibrary::compiled_in();
        assert_eq!(
            lib.get_stage("ce-compound").unwrap().phase.as_deref(),
            Some("compound")
        );
        assert_eq!(
            lib.get_stage("ce-compound").unwrap().role.as_deref(),
            Some("housekeeper")
        );
        assert_eq!(
            lib.get_stage("ce-work").unwrap().phase.as_deref(),
            Some("implement")
        );
        assert_eq!(
            lib.get_stage("ce-verify").unwrap().phase.as_deref(),
            Some("verify")
        );
        assert_eq!(
            lib.get_stage("ce-verify").unwrap().role.as_deref(),
            Some("reviewer")
        );
    }

    /// The evaluator stage is advisory and structured; host verification remains authoritative.
    #[test]
    fn ce_evaluate_is_fresh_context_advisory_contract() {
        let lib = StageLibrary::compiled_in();
        let evaluate = lib.resolve("ce-verify").expect("ce-verify resolves");
        for needle in [
            "FRESH context",
            "remaining_work",
            "validation_evidence",
            "READ-ONLY",
            "VerifySink",
            "never the pass/fail authority",
        ] {
            assert!(evaluate.contains(needle), "ce-verify must name {needle}");
        }
    }

    /// A crew claims a bound bead with the guarded `--claim --actor <pin>`
    /// (bd-close-ceremony-gate-sha-68ms: without `--claim` a second lane
    /// overwrites the first) and never closes it: the captain lands every
    /// bead with `ship land`, so a crew-side close is a second close path.
    #[test]
    fn ce_work_claims_with_the_pin_and_never_closes() {
        let lib = StageLibrary::compiled_in();
        let work = lib.resolve("ce-work").expect("ce-work resolves");
        assert!(work.contains("--claim --actor"), "{work}");
        assert!(work.contains("ship land"), "{work}");
        for forbidden in [
            "br close",
            "--to closed",
            "--status=in_progress --assignee",
            "ask the captain for the bead id",
        ] {
            assert!(!work.contains(forbidden), "ce-work teaches {forbidden:?}");
        }
    }

    /// Every seed prompt is short enough to read in one pass. The ceiling
    /// template rides two stages and is counted separately.
    #[test]
    fn seed_stage_prompts_stay_short() {
        let lib = StageLibrary::compiled_in();
        for name in lib.list() {
            let prompt = lib.resolve(name).expect("resolves");
            let body = prompt.split("\n\n").next().expect("a prompt has a body");
            assert!(
                body.chars().count() <= 950,
                "{name} prompt is {} chars",
                body.chars().count()
            );
        }
    }

    /// Three beads passed their unit tests while nothing in production
    /// called the new code; review and verify must trace the caller.
    #[test]
    fn review_and_verify_trace_the_production_caller() {
        let lib = StageLibrary::compiled_in();
        for name in ["ce-review", "ce-verify"] {
            let prompt = lib.resolve(name).expect("resolves");
            assert!(prompt.contains("SEAM CHECK"), "{name}: {prompt}");
            assert!(prompt.contains("production caller"), "{name}: {prompt}");
        }
    }

    /// Only true renames alias; a mechanics name resolves to nothing and
    /// its lookup error says what replaced it.
    #[test]
    fn aliases_are_renames_and_mechanics_names_are_retired() {
        let lib = StageLibrary::compiled_in();
        for (old, new) in STAGE_ALIASES {
            assert!(lib.stages.contains_key(*new), "{old} aliases a seed stage");
            assert!(
                retired_stage(old).is_none(),
                "{old} is both alias and retired"
            );
        }
        for (old, pointer) in RETIRED_STAGES {
            assert!(!lib.has(old), "retired {old} must not resolve");
            assert_eq!(retired_stage(old), Some(*pointer));
        }
        assert!(lib.get_stage("wave-dispatch").is_none());
        assert_eq!(lib.canonical("ce-execute"), "ce-work");
    }

    /// bd-oj9o #2: the onboard + triage-deep workflows resolve their stages.
    #[test]
    fn onboard_and_triage_deep_stages_resolve() {
        let lib = StageLibrary::compiled_in();
        for semantic in ["ce-explore", "ce-plan", "ce-plan", "ce-plan"] {
            assert!(
                lib.resolve(semantic).is_some(),
                "stage {semantic} resolves (bd-oj9o #2)"
            );
        }
    }

    /// bd-h9xn: the ADR-0019 playbook stages (extracted from the inline
    /// task: strings) all resolve — ce-debug, ce-work, ce-work,
    /// ce-verify, ce-verify, ce-verify, ce-review,
    /// ce-debug, ce-explore. Each is p-code-free (empty alias).
    #[test]
    fn playbook_stages_resolve() {
        let lib = StageLibrary::compiled_in();
        for semantic in [
            "ce-debug",
            "ce-work",
            "ce-work",
            "ce-verify",
            "ce-verify",
            "ce-verify",
            "ce-review",
            "ce-debug",
            "ce-explore",
        ] {
            let stage = lib
                .stages
                .get(semantic)
                .unwrap_or_else(|| panic!("stage {semantic} present"));
            assert_eq!(stage.name, semantic);
            assert!(
                !stage.prompt.trim().is_empty(),
                "{semantic} prompt is non-empty"
            );
            assert!(lib.resolve(semantic).is_some(), "{semantic} resolves");
        }
    }

    /// bd-h9xn: a `stages.toml` override for a NEW playbook stage changes the
    /// resolved prompt (the override contract proof — user overrides reach
    /// the playbook stages, not just the p-code drivers).
    #[test]
    fn playbook_stage_override_changes_prompt() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-stages-h9xn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::create_dir_all(dir.join("flywheel")).unwrap();
        std::fs::write(
            dir.join("flywheel/stages.toml"),
            "[stages.ce-review]\nname = \"ce-review\"\nprompt = \"MY JUDGE OVERRIDE\"\n",
        )
        .unwrap();
        let lib = StageLibrary::load().unwrap();
        assert_eq!(
            lib.resolve("ce-review"),
            Some("MY JUDGE OVERRIDE"),
            "toml override reaches the playbook stage"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0009: a user `stages.toml` override replaces a compiled-in prompt
    /// (the ce-work rename survives overrides — the override keys the
    /// semantic name).
    #[test]
    fn user_override_replaces_prompt() {
        let _g = crate::config::lock_env();
        let lib = StageLibrary::compiled_in();
        let old = lib.resolve("ce-work").unwrap().to_string();
        let dir = std::env::temp_dir().join(format!("fw-stages-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::create_dir_all(dir.join("flywheel")).unwrap();
        std::fs::write(
            dir.join("flywheel/stages.toml"),
            "[stages.ce-work]\nname = \"ce-work\"\nprompt = \"MY OVERRIDE PROMPT\"\n",
        )
        .unwrap();
        let lib2 = StageLibrary::load().unwrap();
        assert_eq!(lib2.resolve("ce-work"), Some("MY OVERRIDE PROMPT"));
        assert_ne!(old, "MY OVERRIDE PROMPT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// W6: seed role affinities exist so stage-first compose can dispatch
    /// the §3.8 example parts without extra declaration.
    #[test]
    fn w6_seed_affinities_present() {
        let lib = StageLibrary::compiled_in();
        assert_eq!(
            lib.get_stage("ce-explore").unwrap().role.as_deref(),
            Some("scout")
        );
        assert_eq!(
            lib.get_stage("ce-work").unwrap().role.as_deref(),
            Some("coder")
        );
    }

    /// W6: tier precedence is seed < repo < user < project — a project-tier
    /// entry shadows user, user shadows seed, and tier tags report honestly.
    #[test]
    fn w6_tier_precedence_seed_user_project() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-w6-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("flywheel")).unwrap();
        std::fs::create_dir_all(dir.join("proj/.flywheel")).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::write(
            dir.join("flywheel/stages.toml"),
            "[stages.w6-layered]\nname = \"w6-layered\"\nprompt = \"USER TIER\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("proj/.flywheel/stages.toml"),
            "[stages.w6-layered]\nname = \"w6-layered\"\nprompt = \"PROJECT TIER\"\n",
        )
        .unwrap();
        let lib =
            StageLibrary::load_layered(Some(&dir.join("proj/.flywheel/stages.toml"))).unwrap();
        assert_eq!(lib.resolve("w6-layered"), Some("PROJECT TIER"));
        assert_eq!(lib.tier_of("w6-layered"), Some(Tier::Project));
        let lib_user_only = StageLibrary::load().unwrap();
        assert_eq!(lib_user_only.resolve("w6-layered"), Some("USER TIER"));
        assert_eq!(lib_user_only.tier_of("w6-layered"), Some(Tier::User));
        assert_eq!(lib.tier_of("ce-work"), Some(Tier::Seed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-cfgv: a prompt-only tier override MERGES field-level — seed
    /// role/phase/params survive; a tier that re-declares a field wins it.
    #[test]
    fn cfgv_stage_override_field_level_merge() {
        let mut lib = StageLibrary::compiled_in();
        assert_eq!(
            lib.get_stage("ce-work").unwrap().role.as_deref(),
            Some("coder")
        );
        lib.apply_toml(
            "[stages.ce-work]\nname = \"ce-work\"\nprompt = \"USER PROMPT\"\n",
            Tier::User,
        )
        .unwrap();
        let s = lib.get_stage("ce-work").unwrap();
        assert_eq!(s.prompt, "USER PROMPT", "override wins its own field");
        assert_eq!(
            s.role.as_deref(),
            Some("coder"),
            "seed role survives prompt-only override"
        );
        assert_eq!(s.phase.as_deref(), Some("implement"), "seed phase survives");
        assert_eq!(
            lib.tier_of("ce-work"),
            Some(Tier::User),
            "tier tag reports the top tier"
        );
        lib.apply_toml(
            "[stages.ce-work]\nname = \"ce-work\"\nprompt = \"P2\"\nrole = \"reviewer\"\nparams = [\"bead\"]\n",
            Tier::Project,
        )
        .unwrap();
        let s = lib.get_stage("ce-work").unwrap();
        assert_eq!(s.role.as_deref(), Some("reviewer"), "declared field wins");
        assert_eq!(s.params, vec!["bead".to_string()], "declared params win");
        assert_eq!(
            s.phase.as_deref(),
            Some("implement"),
            "undeclared phase still inherits"
        );
        lib.apply_toml(
            "[stages.fresh]\nname = \"fresh\"\nprompt = \"ONLY\"\n",
            Tier::User,
        )
        .unwrap();
        assert_eq!(lib.get_stage("fresh").unwrap().prompt, "ONLY");
        assert_eq!(lib.get_stage("fresh").unwrap().role, None);
    }

    #[test]
    fn per_entry_stage_overrides_legacy_aggregate() {
        let mut lib = StageLibrary::compiled_in();
        lib.apply_toml(
            "[stages.ce-work]\nname = \"ce-work\"\nprompt = \"legacy\"\n",
            Tier::User,
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("fw-stage-entry-{}", uuid::Uuid::new_v4()));
        let mut stage = StageLibrary::compiled_in()
            .get_stage("ce-work")
            .unwrap()
            .clone();
        stage.prompt = "per-entry".into();
        let path = dir.join("ce-work.toml");
        upsert_stage_entry(&path, &stage).unwrap();
        lib.apply_stage_entry_path(&path, Tier::User).unwrap();
        assert_eq!(lib.resolve("ce-work"), Some("per-entry"));
        assert_eq!(lib.tier_of("ce-work"), Some(Tier::User));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// W6: `stage lint` rejects bad affinity + phase names (and unknown
    /// principles), accepts a well-formed authored stage.
    #[test]
    fn w6_lint_rejects_bad_affinity_and_phase() {
        let valid_roles = vec!["coder".to_string(), "reviewer".to_string()];
        let mut bad = scaffold("w6-bad");
        bad.role = Some("wizard".to_string());
        bad.phase = Some("vibes".to_string());
        bad.principles = vec!["not-a-principle".to_string()];
        let issues = lint_stage(&bad, &valid_roles);
        assert!(
            issues.iter().any(|i| i.contains("unknown role affinity")),
            "affinity rejected: {issues:?}"
        );
        assert!(
            issues.iter().any(|i| i.contains("unknown phase")),
            "phase rejected: {issues:?}"
        );
        assert!(
            issues.iter().any(|i| i.contains("unknown principle")),
            "principle rejected: {issues:?}"
        );
        let mut good = scaffold("w6-good");
        good.role = Some("coder".to_string());
        good.phase = Some("implement".to_string());
        good.principles = vec!["prove-it-works".to_string()];
        assert!(
            lint_stage(&good, &valid_roles).is_empty(),
            "well-formed stage lints clean"
        );
        let mut compound = scaffold("w6-compound");
        compound.role = Some("housekeeper".to_string());
        compound.phase = Some("compound".to_string());
        assert!(
            lint_stage(&compound, &["housekeeper".to_string()]).is_empty(),
            "compound phase lints clean (ADR-0009)"
        );
    }

    /// W6: the size cap fires on oversized prompts.
    #[test]
    fn w6_lint_size_cap() {
        let mut big = scaffold("w6-big");
        big.prompt = "x".repeat(PROMPT_SIZE_CAP + 1);
        let issues = lint_stage(&big, &[]);
        assert!(issues.iter().any(|i| i.contains("exceeds")), "{issues:?}");
    }

    /// W6: `stage new` scaffolds with frontmatter slots; the TOML file
    /// round-trips (`upsert_stage_in_file`) and loads back at the right tier.
    #[test]
    fn w6_scaffold_and_upsert_roundtrip() {
        let mut s = scaffold("w6-roundtrip");
        s.role = Some("coder".to_string());
        s.phase = Some("implement".to_string());
        let dir = std::env::temp_dir().join(format!("fw-w6rt-{}", uuid::Uuid::new_v4()));
        let path = dir.join(".flywheel/stages.toml");
        upsert_stage_in_file(&path, &s).unwrap();
        assert!(path.is_file(), "tier file created");
        let lib = StageLibrary::load_layered(Some(&path)).unwrap();
        let loaded = lib.get_stage("w6-roundtrip").unwrap();
        assert_eq!(loaded.role.as_deref(), Some("coder"));
        assert_eq!(loaded.phase.as_deref(), Some("implement"));
        assert_eq!(lib.tier_of("w6-roundtrip"), Some(Tier::Project));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// W6: the §3.8 repo evidence bar — <2 objective greens refuses, 2 greens
    /// without a review bead refuses, both present passes.
    #[test]
    fn w6_repo_evidence_bar() {
        assert!(!check_repo_evidence(0, false));
        assert!(!check_repo_evidence(1, true));
        assert!(!check_repo_evidence(2, false), "review bead required");
        assert!(check_repo_evidence(2, true));
    }

    /// bd-ihiqo.4: the 8 canonical ce- stages are compiled in; the retired
    /// wave engine steps resolve as aliases, not as seed entries.
    #[test]
    fn wave_swarm_stages_resolve_compiled_in() {
        let lib = StageLibrary::compiled_in();
        for name in [
            "ce-brainstorm",
            "ce-plan",
            "ce-work",
            "ce-review",
            "ce-verify",
            "ce-compound",
            "ce-debug",
            "ce-explore",
        ] {
            assert!(lib.has(name), "compiled-in stage {name} resolves");
        }
        assert_eq!(lib.stages.len(), 8, "the seed has exactly 8 stages");
    }
}

#[cfg(test)]
mod stage_contract_tests {
    use super::*;

    /// ADR-0021 D3: closed mode/context enums + repeat bounds
    /// (bd-stage-contract-fields-t36sj). Unknown values fail parse;
    /// repeat validates 1 <= min <= max <= 32.
    #[test]
    fn stage_contract_fields_parse_and_validate() {
        let ok: Stage = toml::from_str(
            "name = \"x\"\nprompt = \"y\"\nmode = \"read-only\"\ncontext = \"fresh\"\n",
        )
        .expect("closed values parse");
        assert_eq!(ok.mode, Some(StageMode::ReadOnly));
        assert_eq!(ok.context, Some(StageContext::Fresh));
        assert!(
            toml::from_str::<Stage>("name = \"x\"\nprompt = \"y\"\nmode = \"turbo\"\n").is_err(),
            "unknown mode fails parse"
        );
        assert!(
            toml::from_str::<Stage>("name = \"x\"\nprompt = \"y\"\ncontext = \"lukewarm\"\n")
                .is_err(),
            "unknown context fails parse"
        );
        let good = Repeat {
            min: 2,
            max: 5,
            until: "converged".to_string(),
        };
        assert!(validate_repeat(&good).is_empty(), "valid repeat passes");
        for bad in [
            Repeat {
                min: 0,
                max: 5,
                until: "c".into(),
            },
            Repeat {
                min: 6,
                max: 5,
                until: "c".into(),
            },
            Repeat {
                min: 1,
                max: 33,
                until: "c".into(),
            },
        ] {
            assert!(
                !validate_repeat(&bad).is_empty(),
                "repeat bounds rejected: {bad:?}"
            );
        }
    }

    /// ADR-0021 D3/D6: every seed stage carries a phase + use_when; the
    /// named seeds carry their contract data (ce-review reads, ce-plan
    /// effort/reads/repeat, review + verify seats fresh/read-only).
    #[test]
    fn stage_contract_seed_phases_complete() {
        let lib = StageLibrary::compiled_in();
        let names = [
            "ce-explore",
            "ce-brainstorm",
            "ce-review",
            "ce-verify",
            "ce-plan",
            "ce-review",
            "ce-plan",
            "ce-work",
            "ce-work",
            "ce-compound",
            "ce-work",
            "ce-explore",
            "ce-plan",
            "ce-plan",
            "ce-plan",
            "ce-debug",
            "ce-work",
            "ce-work",
            "ce-verify",
            "ce-verify",
            "ce-verify",
            "ce-review",
            "ce-debug",
            "ce-explore",
            "ce-verify",
        ];
        assert_eq!(names.len(), 25, "all seeds enumerated");
        for name in names {
            let st = lib
                .get_stage(name)
                .unwrap_or_else(|| panic!("{name} seeded"));
            assert!(
                st.phase.as_deref().is_some_and(|p| PHASES.contains(&p)),
                "{name} has a closed phase"
            );
            assert!(
                st.use_when.as_deref().is_some_and(|u| !u.is_empty()),
                "{name} has a use_when line"
            );
        }
        let judge = lib.get_stage("ce-review").expect("ce-review");
        assert_eq!(judge.reads, vec!["steps.arms".to_string()]);
        let polish = lib.get_stage("ce-plan").expect("ce-plan");
        assert_eq!(polish.effort.as_deref(), Some("max"));
        assert_eq!(polish.reads, vec!["inputs.objective".to_string()]);
        assert_eq!(
            polish.repeat,
            Some(Repeat {
                min: 2,
                max: 5,
                until: "converged".to_string()
            })
        );
        for name in ["ce-review", "ce-verify", "ce-verify", "ce-verify"] {
            let st = lib
                .get_stage(name)
                .unwrap_or_else(|| panic!("{name} seeded"));
            assert_eq!(st.context, Some(StageContext::Fresh), "{name} fresh");
            assert_eq!(st.mode, Some(StageMode::ReadOnly), "{name} read-only");
        }
    }

    /// bd-6setu.6 (V10 at the claim): a REVIEW step's stage-contract reads ride
    /// its job spec as `exclude_claimants_of`, so toron's claim can refuse the
    /// pin that authored one of those nodes in the same run. A step whose stage
    /// is not a review, or reads no `steps.*`, carries nothing.
    #[test]
    fn a_review_steps_reads_ride_its_job_spec() {
        let dir = std::env::temp_dir().join(format!("fd-claim-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("claim-review.toml");
        std::fs::write(
            &path,
            "name = \"claim-review\"\nprompt = \"review the work\"\nwitness = \"review\"\nreads = [\"steps.execute\", \"inputs.objective\"]\n",
        )
        .expect("stage file");
        let mut lib = StageLibrary::compiled_in();
        lib.apply_stage_entry_path(&path, Tier::Project)
            .expect("load stage");

        let mut wf = toron_workflow::schema::parse_workflow(
            "name: stamp\nsteps:\n  - name: execute\n    action: dispatch_job\n    assignee: x\n    spec: { task: go, stage: ce-work }\n    on_error: fail_run\n  - name: review\n    action: dispatch_job\n    assignee: x\n    spec: { task: check, stage: claim-review }\n    on_error: fail_run\n  - name: verify\n    action: dispatch_job\n    assignee: x\n    spec: { task: verify, stage: ce-verify }\n    on_error: fail_run\n",
        )
        .expect("parse workflow");
        assert_eq!(
            crate::workflow_lowering::stamp_claim_exclusions(&mut wf, &lib),
            1,
            "exactly the review step is stamped"
        );
        let spec_of = |name: &str| -> toron_workflow::schema::DispatchSpec {
            let step = wf
                .steps
                .iter()
                .find(|s| s.name.as_deref() == Some(name))
                .expect("step");
            match &step.action {
                toron_workflow::schema::Action::DispatchJob { spec, .. } => spec.clone(),
                other => panic!("{name} is a dispatch: {}", other.type_name()),
            }
        };
        assert_eq!(
            spec_of("review").extra.get("exclude_claimants_of"),
            Some(&serde_json::json!(["execute"])),
            "only steps.* refs become claim exclusions (inputs.* never do)"
        );
        assert!(
            !spec_of("execute")
                .extra
                .contains_key("exclude_claimants_of"),
            "an execute step carries no exclusion"
        );
        assert!(
            !spec_of("verify").extra.contains_key("exclude_claimants_of"),
            "ce-verify is not a review-witness step"
        );

        // Idempotent: re-stamping writes the same list, never a duplicate key.
        assert_eq!(
            crate::workflow_lowering::stamp_claim_exclusions(&mut wf, &lib),
            1
        );
        let spec_of = |name: &str| -> toron_workflow::schema::DispatchSpec {
            let step = wf
                .steps
                .iter()
                .find(|s| s.name.as_deref() == Some(name))
                .expect("step");
            match &step.action {
                toron_workflow::schema::Action::DispatchJob { spec, .. } => spec.clone(),
                other => panic!("{name} is a dispatch: {}", other.type_name()),
            }
        };
        assert_eq!(
            spec_of("review").extra.get("exclude_claimants_of"),
            Some(&serde_json::json!(["execute"]))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A prompt-only user-tier override keeps the seed's phase, reads, and
    /// context (field-level merge, bd-cfgv).
    #[test]
    fn stage_contract_merge_keeps_seed_fields() {
        let lib = StageLibrary::compiled_in();
        let base = lib.get_stage("ce-review").expect("seed").clone();
        let over = Stage {
            name: base.name.clone(),
            prompt: "new prompt".to_string(),
            role: None,
            phase: None,
            principles: vec![],
            author: None,
            origin_run: None,
            params: vec![],
            mode: None,
            context: None,
            effort: None,
            reads: vec![],
            witness: None,
            repeat: None,
            use_when: None,
            reached_by: None,
        };
        let merged = merge_stage(base, over);
        assert_eq!(merged.phase.as_deref(), Some("review"));
        assert_eq!(merged.reads, vec!["steps.arms".to_string()]);
        assert_eq!(merged.prompt, "new prompt");
    }

    /// The pure reads gate: inputs.* resolves against declared inputs,
    /// steps.* against prior step names.
    #[test]
    fn stage_contract_reads_unsatisfied_pure() {
        let reads = vec!["steps.arms".to_string()];
        assert_eq!(
            reads_unsatisfied(&reads, &[], &[]),
            vec!["steps.arms".to_string()]
        );
        assert!(reads_unsatisfied(&reads, &[], &["arms".to_string()]).is_empty());
        let io = vec!["inputs.objective".to_string()];
        assert!(reads_unsatisfied(&io, &["objective".to_string()], &[]).is_empty());
        assert_eq!(
            reads_unsatisfied(&io, &["other".to_string()], &[]),
            vec!["inputs.objective".to_string()]
        );
    }

    /// Pure list filters: --phase narrows rows, --unused is set difference.
    #[test]
    fn stage_contract_list_filters() {
        let rows = vec![
            StageRow {
                name: "a".into(),
                tier: Tier::Seed,
                role: None,
                phase: Some("review".into()),
            },
            StageRow {
                name: "b".into(),
                tier: Tier::Seed,
                role: None,
                phase: Some("plan".into()),
            },
        ];
        let got = filter_rows_by_phase(&rows, "review");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "a");
        let all = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let used: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        assert_eq!(
            unreferenced_stages(&all, &used),
            vec!["b".to_string(), "c".to_string()]
        );
        let mut refs = std::collections::HashSet::new();
        collect_stage_refs(
            &serde_json::json!({"spec": {"stage": "ce-review"}}),
            &mut refs,
        );
        assert!(refs.contains("ce-review"));
    }

    /// Missing phase is an ERROR on seed/repo tiers, a WARNING on
    /// user/project tiers (operator stage files predate the field).
    #[test]
    fn stage_contract_tiered_phase_severity() {
        let mut bare = scaffold("tiered-bare");
        bare.phase = None;
        let (errors, warnings) = lint_stage_tiered(&bare, &[], Tier::Seed);
        assert!(
            errors.iter().any(|e| e.contains("phase")),
            "seed errors: {errors:?}"
        );
        assert!(warnings.is_empty());
        let (errors, warnings) = lint_stage_tiered(&bare, &[], Tier::User);
        assert!(
            errors.iter().all(|e| !e.contains("phase")),
            "user warns: {errors:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("phase")),
            "user warns: {warnings:?}"
        );
        let (errors, warnings) = lint_stage_tiered(&bare, &[], Tier::Project);
        assert!(errors.iter().all(|e| !e.contains("phase")));
        assert!(warnings.iter().any(|w| w.contains("phase")));
    }

    /// New fields round-trip through the per-entry TOML renderer.
    #[test]
    fn stage_contract_render_round_trip() {
        let mut st = scaffold("tiered-rt");
        st.phase = Some("plan".to_string());
        st.mode = Some(StageMode::ReadOnly);
        st.context = Some(StageContext::Fresh);
        st.effort = Some("max".to_string());
        st.reads = vec!["inputs.objective".to_string()];
        st.witness = Some("ce-plan.v1".to_string());
        st.repeat = Some(Repeat {
            min: 2,
            max: 5,
            until: "converged".to_string(),
        });
        st.use_when = Some("a bead set exists".to_string());
        let text = render_stage_entry(&st).expect("renders");
        let back: Stage = toml::from_str(&text).expect("parses back");
        assert_eq!(back, st);
    }

    /// bd-witnessed-exits-hd035: an unknown witness is an ERROR on every
    /// tier (a typo'd witness is a silent never-exit).
    #[test]
    fn lint_stage_rejects_unknown_witness_on_every_tier() {
        for tier in [Tier::Seed, Tier::Repo, Tier::User, Tier::Project] {
            let mut st = scaffold("witness-bad");
            st.phase = Some("plan".to_string());
            st.witness = Some("nonsense".to_string());
            let (errors, _) = lint_stage_tiered(&st, &[], tier);
            assert!(
                errors.iter().any(|e| e.contains("nonsense")),
                "{tier:?} must error: {errors:?}"
            );
        }
    }

    /// bd-witnessed-exits-hd035: the seed witnesses are present and known,
    /// and no seed stage trips the witness rule.
    #[test]
    fn seed_witnesses_are_known_and_lint_clean() {
        let lib = StageLibrary::compiled_in();
        assert!(!SEED_WITNESS.is_empty(), "the seed table exists");
        for (name, witness) in SEED_WITNESS {
            let st = lib
                .get_stage(name)
                .unwrap_or_else(|| panic!("seed stage {name}"));
            assert_eq!(st.witness.as_deref(), Some(*witness), "{name}");
            assert!(
                crate::phase_contracts::WITNESSES.contains(witness),
                "{witness} is a registered witness"
            );
            let issues = lint_stage(st, &[]);
            assert!(
                issues.iter().all(|i| !i.contains("witness")),
                "{name} trips the witness rule: {issues:?}"
            );
        }
    }
}

#[cfg(test)]
mod unused_stage_gate_tests {
    use super::{StageLibrary, unused_stages};
    use std::collections::HashSet;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// ADR-0021 D6 (bd-jhz8x.4): an unreferenced stage with NO declaration is
    /// a verify failure; a workflow reference or a declared reach
    /// (`reached_by = compose|manual`) clears it. The gate takes no run log.
    #[test]
    fn an_undeclared_unreferenced_stage_fails_and_a_declaration_clears_it() {
        let all = vec![
            "referenced".to_string(),
            "declared".to_string(),
            "dead".to_string(),
        ];
        assert_eq!(
            unused_stages(&all, &set(&["referenced"])),
            vec!["dead".to_string(), "declared".to_string()],
            "a stage with neither a reference nor a declaration is unused"
        );
        assert_eq!(
            unused_stages(&all, &set(&["referenced", "declared"])),
            vec!["dead".to_string()],
            "a declaration clears exactly the stage that declares it"
        );
        assert!(unused_stages(&all, &set(&["referenced", "declared", "dead"])).is_empty());
    }

    /// The shipped declarations are exactly the stages reached without a
    /// workflow step — and every one of them must exist (a typo'd name would
    /// silently protect nothing while the real stage stayed dead).
    #[test]
    fn declared_reach_names_only_real_stages() {
        let lib = StageLibrary::compiled_in();
        let mut declared = lib.declared_reach();
        declared.sort();
        assert_eq!(
            declared,
            vec![
                "ce-debug".to_string(),
                "ce-explore".to_string(),
                "ce-review".to_string(),
                "ce-verify".to_string(),
            ],
            "the declared manual/compose surface is explicit, not incidental"
        );
        for name in declared {
            assert!(lib.has(&name), "{name} declares a reach, so it must exist");
        }
        assert_eq!(
            lib.get_stage("ce-explore").and_then(|s| s.reached_by),
            Some(super::StageReach::Compose),
            "ce-explore is reached by a compose token"
        );
    }
}

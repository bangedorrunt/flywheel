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
            Tier::Seed => "seed",
            Tier::Repo => "repo",
            Tier::User => "user",
            Tier::Project => "project",
        }
    }
}

/// The §4 lifecycle phases — the only values a stage's `phase` metadata may
/// carry (stage lint rejects anything else).
pub const PHASES: &[&str] = &["resolve", "plan", "implement", "review", "verify"];

/// Prompt size cap enforced by `stage lint` (§3.8 "size cap"). Generous —
/// the largest seed prompt is ~1.5k chars — but bounded: stages are prompts,
/// not documents.
pub const PROMPT_SIZE_CAP: usize = 20_000;

/// A named stage: a reusable prompt primitive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    /// Canonical name (semantic, e.g. `systematic-execute`).
    pub name: String,
    /// The p-code alias (e.g. `p6`) — backward compat.
    #[serde(default)]
    pub alias: String,
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
    ("fresh-eyes-explore", "scout"),
    ("code-review", "reviewer"),
    ("decompose-beads", "planner"),
    ("plan-review", "planner"),
    ("systematic-execute", "coder"),
    ("repo-map", "scout"),
    ("repro-baseline", "coder"),
    ("tdd-red", "coder"),
    ("minimal-fix", "coder"),
    ("prove-verify", "reviewer"),
    ("baseline-measure", "coder"),
    ("measure-keep-or-discard", "coder"),
    ("judge-arms", "oracle"),
    ("diagnose-only", "scout"),
    ("resume-pickup", "housekeeper"),
];

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
                "Check over each bead super carefully-- are you sure it makes sense? Is it optimal? Could we change anything to make the system work better for users? If so, revise the beads. It's a lot easier and faster to operate in \"plan space\" before we start implementing these things! Every generated bead description MUST carry a ## VERIFY fence (the exact command that proves acceptance) — a bead without a verifiable gate is not implementation-ready.",
            ),
            (
                "systematic-execute",
                "p6",
                "Execute the beads via the brief contract. Every dispatched bead carries a filled brief (GOAL / SCOPE / CONTEXT / ACCEPTANCE / VERIFY / TIMEBOX / FORBIDDEN / REPORT / PRINCIPLES / STANDING) — the brief IS the work contract, and the lifecycle lives in the standing-orders file named in its STANDING field (read it once before starting; never re-derive it from memory). For each bead: run the VERIFY command from the brief, write the receipt to `.flywheel/receipts/<bead-id>-<sha>.<kind>.txt`, then close only with a REPORT that states status, commit sha (commit BEFORE close; bead id IN the commit message), verdict kind, what you actually ran, deviations, and the PRINCIPLES cited (`<name> — <the decision it changed>`). Principles in force: prove-it-works (the close is not done without a legal ledger row — verify with `flywheel ledger check <bead-id>`; the receipt is evidence, the verdict is the proof), fix-root-causes (reproduce before diagnosing; no silencers), sequence-verifiable-units (each bead ends in a check before the next starts). Prefer delegating to the `beads` subagent when available. If another agent (peer) is working in this project, join its shared room and steer on plane S (ADR-0020): `toron channel join steer-<bead-id> --project <slug> --as \"$AGENT_NAME\"` then accept/claim jobs posted there via `toron job list --state open --project <slug> --as \"$AGENT_NAME\"` + `toron job claim <job-id> --project <slug> --as \"$AGENT_NAME\"` (first claimer wins; a second claimer exits 1). Keep shared work on the room; keep secrets on plane P (`toron mail`). No worktree.",
            ),
            (
                "commit-and-push",
                "p8",
                "Now, based on your knowledge of the project, commit all changed files now in a series of logically connected groupings with super detailed commit messages for each and then push. Take your time to do it right. Don't edit the code at all. Don't commit obviously ephemeral files. Use ultrathink. ADR-0019 commit contract: put the bead id IN the commit message; stage ONLY your reserved paths (never `git add -A` — cross-sweep race); commit BEFORE closing the bead; the close reason carries the VERIFY tail (what you actually ran).",
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
            // ---- ADR-0019 playbook stages (bd-h9xn): the discipline text is
            // extracted from the playbooks' inline task: strings — the stage
            // carries the shared discipline, the workflow's `plan:` carries
            // per-run interpolation. p-code-free: alias stays empty.
            (
                "repro-baseline",
                "",
                "Run or write the repro for the reported symptom. ACCEPTANCE: the command exits non-zero (or visibly shows the bug). Record the exact failing output as the baseline.",
            ),
            (
                "tdd-red",
                "",
                "Write the FAILING test first (RED). VERIFY: the new test FAILS. Do not fix anything yet.",
            ),
            (
                "minimal-fix",
                "",
                "Apply the MINIMAL fix against the failing test. VERIFY: the test PASSES AND the original repro no longer reproduces.",
            ),
            (
                "prove-verify",
                "",
                "Independently prove the change (different model family, ADR-0019 Spec 4). VERIFY from the bead: test green + repro gone. Record unit-test-verified. Do not re-implement.",
            ),
            (
                "baseline-measure",
                "",
                "Run the baseline/invariant command and store the receipt — this is the baseline that must stay green.",
            ),
            (
                "measure-keep-or-discard",
                "",
                "Measure with the SAME command against the change. If improved: KEEP — the improved state becomes the new baseline. If NOT improved: DISCARD — a revert, never a leftover.",
            ),
            (
                "judge-arms",
                "",
                "Judge the arms for the question. Compare on the named criteria (fit to the question, design quality, feasibility, blast radius). Arms (branch envelope — the collected arm reports live under outputs.arm): {{steps.arms}}. Pick ONE; the others are DISCARDED. No merge of a bit-of-each unless a graft bead is written. Report: chosen arm + the comparison table.",
            ),
            (
                "diagnose-only",
                "",
                "Diagnose the symptom — instrument the live surface or read the artifact; the deliverable is the DIAGNOSIS with the reproducing evidence, root cause, and confidence. Read-only: no mutations to the repo or the target.",
            ),
            (
                "resume-pickup",
                "",
                "Session pickup (ADR-0019): read .flywheel/agent-state/, `br list --status in_progress --json` for stranded beads, ledger gaps, and the last-sent registry; respawn context-exhausted panes; resume briefs for stranded in_progress beads. Idempotent: a done pickup finds nothing to do. Keep SUMMARIES in your report, not raw dumps (guard-the-context-window).",
            ),
            // ---- ADR-0006 D3 wave-swarm recipe stages (bd-wave-swarm-recipe-
            // wsot): one stage per composed phase. p-code-free: alias empty.
            (
                "wave-gate",
                "",
                "Wave gate (ADR-0006 D3): run `flywheel preflight --mode workflow` — a failed check STOPS the wave (post `[gate] blocked: <check>` to the steer channel; never fix infra yourself mid-gate). Then scope the ready set for this wave: `br ready --json` intersected with the wave label, and if ANY bead with an earlier wave-M (M<N) is open or in_progress, post `[gate] held: <bead ids>` and STOP (waves run in order, never around them). BINDING CONTRACT (bd-wave-swarm-graduation-fix-98io — GATE output is the DISPATCH input): `mkdir -p .flywheel/wave` then write `.flywheel/wave/<wave-label>-ready.json` containing EXACTLY {\"wave_label\": \"<label>\", \"held\": <bool>, \"blockers\": [\"<bead-id>\"], \"ready\": [{\"id\": \"<bead-id>\", \"title\": \"...\", \"verify\": \"<VERIFY command>\", \"priority\": <n>}]}. Reply with ONLY that JSON object. Never dispatch, never adjudicate — gate, write the artifact, report.",
            ),
            (
                "wave-dispatch",
                "",
                "Wave dispatch (ADR-0006 D3/D1/D4). FIRST read the gate artifact `.flywheel/wave/<wave-label>-ready.json` (bd-wave-swarm-graduation-fix-98io — you dispatch ONLY beads the GATE wrote there; if the file is missing or \"held\": true, post `[dispatch] held` and STOP). For EACH bead in ready[] emit ONE payload to its owner pane: `flywheel send --to <owner-pin> --stage p1 --param bead=<id> --param scope=<owned globs> --param exit=<the bead's VERIFY command from the artifact> --param channel=<steer-channel> --param pin=<owner-pin> --param report_file=/tmp/<id>-report-<epoch>.md`. Every payload NAMES its bead id and carries the CLAIM/CLOSE CONTRACT — also post it once on the steer channel: claim before work (`br update <id> --status=in_progress --assignee <pin>`), reserve files before editing, commit BEFORE close with the bead id in the message, close only with VERIFY tail + principles citations. Full reports go to report_file; the steer channel gets a <=700-char summary plus the file path.",
            ),
            (
                "wave-observe",
                "",
                "Wave observe (ADR-0006 D3): watch the steer channel for claims, receipts, and blocks. ESCALATE, never adjudicate: a bead blocked >10min, a reservation-ladder timeout, a fix-vs-hand-over fork, regression suspicion, or any judgment call -> mail the captain on the task thread with the evidence. You do NOT decide fixes, reassign work, or close beads. Escalations wake the captain; everything else runs unattended.",
            ),
            (
                "wave-verify",
                "",
                "Wave verify (ADR-0006 D3): for each close claim confirm ALL of: (1) the receipt exists at the committed HEAD — `flywheel ledger check <bead-id>`; (2) the close reason carries its VERIFY tail (what was actually run + the result tail); (3) PRINCIPLES citations name decisions (`<name> — <decision>`, cargo-cult bare names are rejects). A close missing any of these REOPENS: post `[verify] reopen <id>: <missing>` on the steer channel. Verify against the real artifact, never a self-report.",
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
                    role: None,
                    phase: None,
                    principles: vec![],
                    author: None,
                    origin_run: None,
                    params: vec![],
                },
            );
        }
        // Seed role affinities (§3.8 stage-first compose).
        for (name, role) in SEED_AFFINITY {
            if let Some(stage) = stages.get_mut(*name) {
                stage.role = Some(role.to_string());
            }
        }
        let tiers: BTreeMap<String, Tier> =
            stages.keys().map(|k| (k.clone(), Tier::Seed)).collect();
        StageLibrary { stages, tiers }
    }

    /// The repo DATA tier: `repo-stages.toml` compiled into the binary
    /// (include_str!). Empty until a stage is promoted to the repo tier
    /// through the §3.8 evidence bar.
    fn apply_embedded_repo_data(&mut self) -> Result<()> {
        const REPO_STAGES_TOML: &str = include_str!("repo-stages.toml");
        self.apply_toml(REPO_STAGES_TOML, Tier::Repo)
    }

    /// Apply one tier's `stages.toml` (field-level merge, later tier wins).
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
            self.stages.insert(name, stage);
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

    /// Apply the user `stages.toml` override (field-level).
    fn override_from_toml(&mut self) -> Result<()> {
        let path = config_dir().join("stages.toml");
        self.apply_toml_path(&path, Tier::User)
    }

    /// The tier a resolved name came from (§3.8 tier tags).
    pub fn tier_of(&self, name: &str) -> Option<Tier> {
        let canonical = self.canonical(name);
        self.tiers.get(&canonical).copied()
    }

    /// Map an alias (p-code) to its canonical semantic name.
    fn canonical(&self, name: &str) -> String {
        if self.stages.contains_key(name) {
            return name.to_string();
        }
        self.stages
            .values()
            .find(|s| s.alias == name)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| name.to_string())
    }

    /// The full stage record for a name (semantic or p-code alias).
    pub fn get_stage(&self, name: &str) -> Option<&Stage> {
        let canonical = self.canonical(name);
        self.stages.get(&canonical)
    }

    /// One `stage list --all` row: names + tier + the §3.8 metadata.
    pub fn list_tiered(&self) -> Vec<StageRow> {
        self.stages
            .values()
            .filter_map(|s| {
                Some(StageRow {
                    name: s.name.clone(),
                    alias: s.alias.clone(),
                    tier: self.tiers.get(&s.name).copied()?,
                    role: s.role.clone(),
                    phase: s.phase.clone(),
                })
            })
            .collect()
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

/// One `stage list` row (§3.8: tier-tagged, phase + role affinity).
#[derive(Debug, Clone, Serialize)]
pub struct StageRow {
    pub name: String,
    pub alias: String,
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
        alias: String::new(),
        prompt: format!("TODO: the {name} prompt body."),
        role: None,
        phase: None,
        principles: vec![],
        author: None,
        origin_run: None,
        params: vec![],
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let out = toml::to_string(&file).context("serialize stages.toml")?;
    std::fs::write(path, out).with_context(|| format!("write {}", path.display()))
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
    for p in &stage.principles {
        if !crate::principles::is_known_name(p) {
            issues.push(format!(
                "{label}: unknown principle {p:?} (ADR-0019 registry)"
            ));
        }
    }
    issues
}

/// The §3.8 repo-tier evidence bar: green across at least two distinct
/// objectives or projects, PLUS a review bead. Projects are not recorded in
/// the run log today, so the check enforces the objectives arm (at least 2
/// distinct prompt hashes among successful runs) and requires the review bead —
/// refusal is the honest default when evidence cannot be proven.
pub fn check_repo_evidence(distinct_objective_greens: usize, review_bead_present: bool) -> bool {
    distinct_objective_greens >= 2 && review_bead_present
}
/// The reserved D1 payload slots (ADR-0006 §3 D1). Documented vocabulary;
/// substitution itself accepts any `--param k=v` key. Consumed by the test
/// vocabulary gate and by future D3/D4 payload emitters.
#[expect(dead_code)]
pub const RESERVED_SLOTS: &[&str] =
    &["bead", "scope", "exit", "channel", "pin", "report_file"];

/// Parse repeatable `--param k=v` argv pairs into a map. A pair without `=`
/// is a usage error naming the offending argument.
pub fn parse_params(raw: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for pair in raw {
        let Some((k, v)) = pair.split_once('=') else {
            return Err(anyhow::anyhow!(
                "--param {pair:?} is not k=v (missing '=')"
            ));
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
        .map(|r| r.as_str())
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
            // verbatim: values are prompt text, never interpreted
            out = out.replace(&slot, v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ADR-0006 D1: parameterized stage payloads (bd-stage-params-pqo9) ----

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
        assert!(
            err.contains("exit"),
            "missing slot named in error: {err}"
        );
        assert!(!err.contains("bead,"), "supplied param not flagged: {err}");
    }

    /// Zero params + no declared required params → byte-identical passthrough
    /// for EVERY compiled-in stage (the golden back-compat bar).
    #[test]
    fn stage_param_zero_param_byte_identical_golden() {
        let lib = StageLibrary::compiled_in();
        for (name, _) in lib.list() {
            let stage = lib.get_stage(name).expect("stage exists");
            let empty = BTreeMap::new();
            let out =
                render_stage(&stage.prompt.clone(), &stage.params, &empty)
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

    /// bd-h9xn: the ADR-0019 playbook stages (extracted from the inline
    /// task: strings) all resolve — repro-baseline, tdd-red, minimal-fix,
    /// prove-verify, baseline-measure, measure-keep-or-discard, judge-arms,
    /// diagnose-only, resume-pickup. Each is p-code-free (empty alias).
    #[test]
    fn playbook_stages_resolve_pcode_free() {
        let lib = StageLibrary::compiled_in();
        for semantic in [
            "repro-baseline",
            "tdd-red",
            "minimal-fix",
            "prove-verify",
            "baseline-measure",
            "measure-keep-or-discard",
            "judge-arms",
            "diagnose-only",
            "resume-pickup",
        ] {
            let stage = lib
                .stages
                .get(semantic)
                .unwrap_or_else(|| panic!("stage {semantic} present"));
            assert_eq!(stage.name, semantic);
            assert!(stage.alias.is_empty(), "{semantic} has no p-code alias");
            assert!(
                !stage.prompt.trim().is_empty(),
                "{semantic} prompt is non-empty"
            );
            // The discipline text survives extraction — the playbook stages
            // keep the marker words the playbook control-flow relies on.
            assert!(
                lib.resolve(semantic).is_some(),
                "{semantic} resolves (semantic or alias)"
            );
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
            "[stages.judge-arms]\nname = \"judge-arms\"\nalias = \"\"\nprompt = \"MY JUDGE OVERRIDE\"\n",
        )
        .unwrap();
        let lib = StageLibrary::load().unwrap();
        assert_eq!(
            lib.resolve("judge-arms"),
            Some("MY JUDGE OVERRIDE"),
            "toml override reaches the playbook stage"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    // ---- ADR-0022 W6 (bd-adr22-w6-stage-registry-0jgr) ----

    /// W6: seed role affinities exist so stage-first compose can dispatch
    /// the §3.8 example parts without extra declaration.
    #[test]
    fn w6_seed_affinities_present() {
        let lib = StageLibrary::compiled_in();
        assert_eq!(
            lib.get_stage("fresh-eyes-explore").unwrap().role.as_deref(),
            Some("scout")
        );
        assert_eq!(
            lib.get_stage("systematic-execute").unwrap().role.as_deref(),
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
            "[stages.w6-layered]\nname = \"w6-layered\"\nprompt = \"USER TIER\"\nalias = \"w6l\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("proj/.flywheel/stages.toml"),
            "[stages.w6-layered]\nname = \"w6-layered\"\nprompt = \"PROJECT TIER\"\nalias = \"w6l\"\n",
        )
        .unwrap();
        let lib =
            StageLibrary::load_layered(Some(&dir.join("proj/.flywheel/stages.toml"))).unwrap();
        assert_eq!(lib.resolve("w6-layered"), Some("PROJECT TIER"));
        assert_eq!(lib.resolve("w6l"), Some("PROJECT TIER"), "alias follows");
        assert_eq!(lib.tier_of("w6-layered"), Some(Tier::Project));
        // Without the project file, the user tier still resolves.
        let lib_user_only = StageLibrary::load().unwrap();
        assert_eq!(lib_user_only.resolve("w6-layered"), Some("USER TIER"));
        assert_eq!(lib_user_only.tier_of("w6-layered"), Some(Tier::User));
        // Seed tag on an untouched compiled entry.
        assert_eq!(lib.tier_of("systematic-execute"), Some(Tier::Seed));
        let _ = std::fs::remove_dir_all(&dir);
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

    /// ADR-0006 D3: the wave-swarm recipe's compiled-in stages resolve.
    #[test]
    fn wave_swarm_stages_resolve_compiled_in() {
        let lib = StageLibrary::compiled_in();
        for stage in ["wave-gate", "wave-dispatch", "wave-observe", "wave-verify"] {
            assert!(lib.has(stage), "compiled-in stage {stage} resolves");
        }
    }
}

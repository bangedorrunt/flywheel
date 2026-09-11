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
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

// (p-code aliases retired by ADR-0009 — semantic names only.)

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
    ("fresh-eyes-explore", "scout"),
    ("ce-brainstorm", "planner"),
    ("ce-review", "reviewer"),
    ("ce-plan", "planner"),
    ("ce-plan-review", "planner"),
    ("ce-execute", "coder"),
    ("ce-simplify", "coder"),
    ("ce-compound", "housekeeper"),
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
    ("wave-gate", "planner"),
    ("wave-dispatch", "planner"),
    ("wave-observe", "scout"),
    ("wave-verify", "reviewer"),
];

/// Seed phase metadata for the compound-engineering loop stages (ADR-0009):
/// the loop is brainstorm → plan → plan-review → execute → simplify → review
/// → verify → compound, and `stage list` should say so.
const SEED_PHASE: &[(&str, &str)] = &[
    ("ce-brainstorm", "plan"),
    ("ce-plan", "plan"),
    ("ce-plan-review", "review"),
    ("ce-execute", "implement"),
    ("ce-simplify", "implement"),
    ("ce-review", "review"),
    ("ce-compound", "compound"),
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

    /// The compiled-in defaults (the former `STAGES` const) under their
    /// semantic names. The `ce-` family is the compound-engineering loop
    /// (ADR-0009); p-code aliases are retired.
    pub fn compiled_in() -> Self {
        let defaults: &[(&str, &str)] = &[
            (
                "fresh-eyes-explore",
                "I want you to sort of randomly explore the code files in this project, choosing code files to deeply investigate and understand and trace their functionality and execution flows through the related code files which they import or which are imported by. Once you understand the purpose of the code in the larger context of the workflows, I want you to do a super careful, methodical, and critical check with \"fresh eyes\" to find any obvious bugs, problems, errors, issues, silly mistakes, etc. and then systematically and meticulously and intelligently correct them. Be sure to comply with ALL rules in AGENTS dot md.",
            ),
            // ---- ADR-0009 compound-engineering loop stages (ce- family) ----
            (
                "ce-brainstorm",
                "Sharpen a rough objective into REQUIREMENTS before any bead exists (ADR-0009 ce-brainstorm). Ask ONE question at a time, and only questions whose answer changes the artifact — a decision the owner must make, never something the repo can answer. Ground first: read AGENTS.md, the relevant ADRs, and `rg -il \"<objective keywords>\" docs/learnings/` — a prior learning that touches this area carries its constraints in. The output is REQUIREMENTS-ONLY (never implementation): one unified statement of what this must be, explicit non-goals, open questions each naming the decision it blocks, and a data-shape note (what exists to work with). A requirement you cannot test is a wish — every acceptance names its evidence. Post the artifact to the steer channel; reply with ONLY a JSON object {\"target\": \"...\", \"non_goals\": [\"...\"], \"open_questions\": [\"...\"], \"data_shape\": \"...\"}.",
            ),
            (
                "ce-review",
                "Review the code written by your fellow agents - issues, bugs, errors, problems, inefficiencies, security problems, reliability issues, etc. - and carefully diagnose their underlying root causes using first-principle analysis. Don't restrict yourself to the latest commits; cast a wider net and go super deep. Use ultrathink. Principles in force: prove-it-works (check the real artifact - run the change's VERIFY or the ledger proof, never a self-report), fix-root-causes (reproduce first; name the root, not the symptom), minimize-reader-load (count layers and hidden state; collapse one-caller wrappers). BLOCKER: a REPORT line that cites a PRINCIPLES name without its decision (`<name> - <the decision it changed>`) is a cargo-cult citation - reject it and require the decision the principle changed; on a non-trivial bead (priority <= 2) an empty PRINCIPLES field is an incomplete report (the loop's `principles` checker enforces the same rule: known name + decision clause, required on P0-P2). Review contract (ADR-0009): a finding exists only under a DETECTION CONDITION - the observable circumstance in the work under review, never a topic to opine on; HIGH confidence requires quoted evidence (file:line + the offending text), unquoted high confidence is a reject; INDEPENDENCE - two lenses reasoned in one pane are two perspectives, not two witnesses, so claim corroboration only across separately-dispatched contexts. Each finding carries severity (P0 breakage ... P3 advisory) and an AUTOFIX CLASS: gated_auto | manual | advisory. A finding that merely prefers a different approach than a `settled:` captain decision routes report-only (settled_conflict); a real defect inside a settled approach keeps full severity. Findings land as JSON {\"findings\": [{\"id\", \"severity\", \"condition\", \"evidence\", \"autofix\", \"confidence\"}]}. Review is REPORT-ONLY - applying fixes is a separate authorized pass. Ask-user authority (ADR-0010 D15): a review finding that asks the user a question (an ask-user finding) is never self-answered by the reviewer. Findings that merely restore accepted behavior note the restoration and its evidence. Findings that expand scope (new guarantee, threat model, architecture, destructive, security) escalate report-only with the five-element escalation (original criterion, proposed expansion, smallest compliant alternative, accept-vs-decline consequences, recommendation) and park in OPEN DECISIONS. Only an answer/--verdict closes the call.",
            ),
            (
                "ce-plan",
                "OK so please take ALL of that and elaborate on it more and then create a comprehensive and granular set of beads for all this with tasks, subtasks, and dependency structure overlaid, with detailed comments so that the whole thing is totally self-contained and self-documenting (including relevant background, reasoning/justification, considerations, etc.-- anything we'd want our \"future self\" to know about the goals and intentions and thought process and how it serves the over-arching goals of the project.) Follow the beads-workflow discipline: check your beads N times before implementing, keep each bead self-contained (background, reasoning, acceptance criteria), and link dependencies with `br dep add`. Every bead description carries a ## VERIFY fence (the exact command that proves acceptance) — a bead without a verifiable gate is not implementation-ready (plan review re-checks this).",
            ),
            (
                "ce-plan-review",
                "Check over each bead super carefully-- are you sure it makes sense? Is it optimal? Could we change anything to make the system work better for users? If so, revise the beads. It's a lot easier and faster to operate in \"plan space\" before we start implementing these things! Every generated bead description MUST carry a ## VERIFY fence (the exact command that proves acceptance) — a bead without a verifiable gate is not implementation-ready.",
            ),
            (
                "ce-execute",
                "Execute the beads via the brief contract. Every dispatched bead carries a filled brief (GOAL / SCOPE / CONTEXT / LEARNINGS / ACCEPTANCE / VERIFY / TIMEBOX / FORBIDDEN / REPORT / PRINCIPLES / STANDING) — the brief IS the work contract, and the lifecycle lives in the standing-orders file named in its STANDING field (read it once before starting; never re-derive it from memory). LEARNINGS carries the docs/learnings/ entries matching this scope — their constraints ride the brief; never re-derive a lesson the corpus already holds. IDEMPOTENT RE-ENTRY: before starting a bead, run its VERIFY at HEAD — already green means already done: mark complete and skip (a resumed wave never redoes shipped work). SIMPLIFY AS YOU GO: after each green VERIFY, one behavior-preserving pass (delete weightless code, collapse one-caller wrappers), then re-run the VERIFY. For each bead: run the VERIFY command from the brief, write the receipt to `.flywheel/receipts/<bead-id>-<sha>.<kind>.txt`, then close only with a REPORT that states status, commit sha (commit BEFORE close; bead id IN the commit message), verdict kind, what you actually ran, deviations, and the PRINCIPLES cited (`<name> — <the decision it changed>`). Principles in force: prove-it-works (the close is not done without a legal ledger row — verify with `flywheel ledger check <bead-id>`; the receipt is evidence, the verdict is the proof), fix-root-causes (reproduce before diagnosing; no silencers), sequence-verifiable-units (each bead ends in a check before the next starts). Prefer delegating to the `beads` subagent when available. If another agent (peer) is working in this project, join its shared room and steer on plane S (ADR-0020): `toron channel join steer-<bead-id> --project <slug> --as \"$AGENT_NAME\"` then accept/claim jobs posted there via `toron job list --state open --project <slug> --as \"$AGENT_NAME\"` + `toron job claim <job-id> --project <slug> --as \"$AGENT_NAME\"` (first claimer wins; a second claimer exits 1). Keep shared work on the room; keep secrets on plane P (`toron mail`). No worktree. If the dispatch names no bead binding (no BEAD line), do NOT act on the TASK-ID: stop and ask the captain for the bead id. Ask-user authority (ADR-0010 D15): never self-answer an ask-user finding. Unambiguous restorations of accepted behavior auto-decide — Publish the corr (journaled) and proceed. Expansions (new guarantee, threat model, architecture, destructive, security) stop the stage, Publish the corr, and park on WaitCorr with the five-element escalation (original criterion, proposed expansion, smallest compliant alternative, accept-vs-decline consequences, recommendation) into OPEN DECISIONS. Only an answer/--verdict closes the call; worker-side auto-close is a LoudMiss.",
            ),
            (
                "ce-simplify",
                "Behavior-preserving refinement of FRESHLY changed code (ADR-0009 ce-simplify) — run after a green VERIFY, before review. Delete weightless code; collapse one-caller wrappers and needless indirection; refuse new abstractions. VERIFY with the SAME command before and after — byte-identical behavior is the contract; any behavior change is a bug you introduced. Scope ONLY the change's own files (fix-owned files), never adjacent work-in-progress. If nothing improves by deletion, say so and stop — a no-op simplify is a clean skip (terminal: clean_skip), not a failure.",
            ),
            (
                "ce-compound",
                "Compound gate (ADR-0009): after a VERIFIED solution, decide whether the work produced a durable learning, then capture it. The gate is three-part and ALL must hold: NON-OBVIOUS (the reasoning is not recoverable from the final code, tests, types, or comments), DURABLE (an invariant, root cause, rejected approach, or convention useful beyond this diff), MATERIAL (forgetting it would plausibly cause recurrence or substantial rediscovery). Counterfactual: if the learning doc vanished, would the next crew redo this investigation? If not, reply {\"terminal\": \"clean_skip\"} — completion alone never qualifies; effort and diff size are not evidence. ONE learning per run. Qualifying: check overlap FIRST (`rg -il \"<keywords>\" docs/learnings/`) — high overlap UPDATES the existing doc (add last_updated + re-stamp reinforced: <today> — ADR-0010 D12 reinforcement), never a duplicate. Otherwise write `docs/learnings/<category>/<slug>.md` with frontmatter track: bug|knowledge, module, problem_type, tags, bead, created, tier: aging (pinned ONLY for invariants that must never decay out of the GATE projection; perishable ONLY when the doc names a checkable expiry via `expires:`), reinforced: <today> — bug track sections: Symptoms / What Didn't Work / Solution / Why This Works / Prevention; knowledge track: Context / Guidance / Why This Matters / When to Apply. The learning ships in the SAME commit as the work it came from. Reply with ONLY {\"terminal\": \"written|updated|clean_skip\", \"path\": \"...\", \"track\": \"...\"}. A failed capture NEVER blocks the run — report and move on.",
            ),
            (
                "commit-and-push",
                "Now, based on your knowledge of the project, commit all changed files now in a series of logically connected groupings with super detailed commit messages for each and then push. Take your time to do it right. Don't edit the code at all. Don't commit obviously ephemeral files. Use ultrathink. ADR-0019 commit contract: put the bead id IN the commit message; stage ONLY your reserved paths (never `git add -A` — cross-sweep race); commit BEFORE closing the bead; the close reason carries the VERIFY tail (what you actually ran).",
            ),
            (
                "repo-map",
                "Cold-start mapping (onboard step 1): read AGENTS.md and the top-level docs in this repo, list the crate/module structure and the main entry points, and trace how the pieces connect (entry points, core modules, tests, build/lint commands). Produce a concise map the newcomer can act from — paths, responsibilities, conventions, and the exact commands to build, test, and lint.",
            ),
            (
                "seed-first-task",
                "Onboard step 2: from the repo map produced in the previous step, seed the newcomer's context — summarize the map, state the conventions they MUST follow (TDD, skills, code style), and propose ONE narrow first task with [paths] [change] [acceptance] so they can start immediately.",
            ),
            (
                "bv-triage",
                "Deep-triage step 1: run `bv --robot-triage` (and `br ready --json` for the actionable subset) over this project's beads, then produce a compact summary: counts by status/priority, the top actionable items, blockers, and any cycles or stale items worth attention.",
            ),
            (
                "prioritize-assign",
                "Deep-triage step 2: from the bv-triage summary, prioritize the actionable backlog in dependency order and assign each item to an owner (agent identity registered in the project). Output an ordered, owned list the crew can execute.",
            ),
            (
                "spawn-crew",
                "Deep-triage step 3: spawn the assigned crew for the prioritized backlog — `flywheel spawn <project> --recipe <squad>` (or the explicit counts) and hand each owner their assigned items. Confirm the panes are up and the crew is ready before finishing.",
            ),
            // ---- ADR-0019 playbook stages (bd-h9xn): the discipline text is
            // extracted from the playbooks' inline task: strings — the stage
            // carries the shared discipline, the workflow's `plan:` carries
            // per-run interpolation.
            (
                "repro-baseline",
                "Run or write the repro for the reported symptom. ACCEPTANCE: the command exits non-zero (or visibly shows the bug). Record the exact failing output as the baseline.",
            ),
            (
                "tdd-red",
                "Write the FAILING test first (RED). VERIFY: the new test FAILS. Do not fix anything yet.",
            ),
            (
                "minimal-fix",
                "Apply the MINIMAL fix against the failing test. VERIFY: the test PASSES AND the original repro no longer reproduces.",
            ),
            (
                "prove-verify",
                "Independently prove the change (different model family, ADR-0019 Spec 4). VERIFY from the bead: test green + repro gone. Record unit-test-verified. Do not re-implement.",
            ),
            (
                "baseline-measure",
                "Run the baseline/invariant command and store the receipt — this is the baseline that must stay green.",
            ),
            (
                "measure-keep-or-discard",
                "Measure with the SAME command against the change. If improved: KEEP — the improved state becomes the new baseline. If NOT improved: DISCARD — a revert, never a leftover.",
            ),
            (
                "judge-arms",
                "Judge the arms for the question. Compare on the named criteria (fit to the question, design quality, feasibility, blast radius). Arms (branch envelope — the collected arm reports live under outputs.arm): {{steps.arms}}. Pick ONE; the others are DISCARDED. No merge of a bit-of-each unless a graft bead is written. Report: chosen arm + the comparison table.",
            ),
            (
                "diagnose-only",
                "Diagnose the symptom — instrument the live surface or read the artifact; the deliverable is the DIAGNOSIS with the reproducing evidence, root cause, and confidence. Read-only: no mutations to the repo or the target.",
            ),
            (
                "resume-pickup",
                "Session pickup (ADR-0019): read .flywheel/agent-state/, `br list --status in_progress --json` for stranded beads, ledger gaps, and the last-sent registry; respawn context-exhausted panes; resume briefs for stranded in_progress beads. Idempotent: a done pickup finds nothing to do. Keep SUMMARIES in your report, not raw dumps (guard-the-context-window).",
            ),
            // ---- ADR-0006 D3 wave-swarm compose-token stages (bd-wave-swarm-
            // recipe-wsot): one stage per composed phase; ADR-0009 adds the
            // grounding read, the escalation lesson line, and the residual
            // gate.
            (
                "wave-gate",
                "Wave gate (ADR-0006 D3): run `flywheel preflight --mode workflow` — a failed check STOPS the wave (post `[gate] blocked: <check>` to the steer channel; never fix infra yourself mid-gate). Then scope the ready set for this wave: `br ready --json` intersected with the wave label, and if ANY bead with an earlier wave-M (M<N) is open or in_progress, post `[gate] held: <bead ids>` and STOP (waves run in order, never around them). GROUNDING READ (ADR-0009, bounded by ADR-0010 D12): `flywheel __gate-learnings --budget ${FLYWHEEL_GATE_BUDGET:-4000}` — a budgeted projection of docs/learnings (top-N by reinforcement recency within ~4k tokens; pinned never decays, aging 30d / perishable 7d unreinforced decays OUT of the projection). Read ONLY the projected paths — frontmatter + rg context, under the budget; matching entries become the wave's carried constraints. BINDING CONTRACT (bd-wave-swarm-graduation-fix-98io — GATE output is the DISPATCH input): `mkdir -p .flywheel/wave` then write `.flywheel/wave/<wave-label>-ready.json` containing EXACTLY {\"wave_label\": \"<label>\", \"held\": <bool>, \"blockers\": [\"<bead-id>\"], \"ready\": [{\"id\": \"<bead-id>\", \"title\": \"...\", \"verify\": \"<VERIFY command>\", \"priority\": <n>}]} plus, when the grounding read matched, \"learnings\": [\"docs/learnings/<...>\"]. Reply with ONLY that JSON object. Never dispatch, never adjudicate — gate, write the artifact, report.",
            ),
            (
                "wave-dispatch",
                "Wave dispatch (ADR-0006 D3/D1/D4). FIRST read the gate artifact `.flywheel/wave/<wave-label>-ready.json` (bd-wave-swarm-graduation-fix-98io — you dispatch ONLY beads the GATE wrote there; if the file is missing or \"held\": true, post `[dispatch] held` and STOP). For EACH bead in ready[] emit ONE payload to the crew's coder pane (find it: `herdr agent list`, this workspace, profile coder): `flywheel send --to <coder-pane-name> --stage ce-execute --param bead=<id> --param scope=<owned globs> --param exit=<the bead's VERIFY command from the artifact> --param channel=<steer-channel> --param pin=<coder-pin> --param report_file=/tmp/<id>-report-<epoch>.md` — and when the gate artifact carries learnings[], append `--param learnings=<paths csv>` so the brief's LEARNINGS field carries their constraints. fresh-eyes-explore is scout-only; coder execution is ce-execute. Every payload NAMES its bead id and carries the CLAIM/CLOSE CONTRACT — also post it once on the steer channel: claim before work (`br update <id> --status=in_progress --assignee <herdr-name>` — the herdr slug, e.g. flywheel-toron-coder, never a pin), reserve files before editing, commit BEFORE close with the bead id in the message, gate the completion (`br gate report <id> --gate <verdict> --provider <pin> --status pass --to closed`) then close with the commit sha (`br close <id> --commit-sha <sha>`) carrying the VERIFY tail + principles citations. Full reports go to report_file; the steer channel gets a <=700-char summary plus the file path.",
            ),
            (
                "wave-observe",
                "Wave observe (ADR-0006 D3): watch the steer channel for claims, receipts, and blocks. ESCALATE, never adjudicate: a bead blocked >10min, a reservation-ladder timeout, a fix-vs-hand-over fork, regression suspicion, or any judgment call -> mail the captain on the task thread with the evidence. You do NOT decide fixes, reassign work, or close beads. ESCALATION LESSON (ADR-0009 failure path): every escalation mail ends with a `lesson:` line — one sentence on what a future wave could do differently; if genuinely nothing, `lesson: none` (these feed the corpus at compound time). Escalations wake the captain; everything else runs unattended.",
            ),
            (
                "wave-verify",
                "Wave verify (ADR-0006 D3): for each close claim confirm ALL of: (1) the receipt exists at the committed HEAD — `flywheel ledger check <bead-id>`; (2) the close reason carries its VERIFY tail (what was actually run + the result tail); (3) PRINCIPLES citations name decisions (`<name> — <decision>`, cargo-cult bare names are rejects). RESIDUAL GATE (ADR-0009): every finding from the REVIEW step is either FIXED (evidenced in the diff) or FILED — a `br create` bead id cited in the close; a finding that lives only in the session fails this gate. TERMINAL STATES (ADR-0009): report files carry `terminal: done|clean_skip|blocked|failed` — clean_skip is a SUCCESS outcome (the gate judged the work N/A); a MISSING report stays unknown, never assumed failed. A close missing any of these REOPENS: post `[verify] reopen <id>: <missing>` on the steer channel. Verify against the real artifact, never a self-report. DIRT POLICY: only dirt under source/, tests/, benches/, Cargo.toml, Cargo.lock, and pinned toolchain files blocks a receipt. Tracker state (.beads/), orchestrator bookkeeping (.flywheel/), and editor artifacts are always-allowed dirt — note them and proceed; never stall a receipt on them. Anything else dirty: post-and-stop.",
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
                },
            );
        }
        // Seed role affinities (§3.8 stage-first compose).
        for (name, role) in SEED_AFFINITY {
            if let Some(stage) = stages.get_mut(*name) {
                stage.role = Some(role.to_string());
            }
        }
        // Seed phase metadata for the ce- loop (ADR-0009).
        for (name, phase) in SEED_PHASE {
            if let Some(stage) = stages.get_mut(*name) {
                stage.phase = Some(phase.to_string());
            }
        }
        let tiers: BTreeMap<String, Tier> =
            stages.keys().map(|k| (k.clone(), Tier::Seed)).collect();
        Self { stages, tiers }
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

    /// Canonical name (identity — ADR-0009 retired the alias indirection).
    fn canonical(&self, name: &str) -> String {
        name.to_string()
    }

    /// The full stage record for a name.
    pub fn get_stage(&self, name: &str) -> Option<&Stage> {
        self.stages.get(name)
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
        self.stages.get(name).map(|s| s.prompt.as_str())
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

/// The user override path (`~/.config/flywheel/stages.toml`), if present.
pub fn override_path() -> Option<PathBuf> {
    let path = config_dir().join("stages.toml");
    path.is_file().then_some(path)
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
            // verbatim: values are prompt text, never interpreted
            out = out.replace(&slot, v);
        } else if k != "task_id" {
            // A param with no slot would vanish silently (hollow-dispatch
            // class): warn loud, keep rendering. Never a hard error.
            // `task_id` is exempt: the send machinery consumes it for thread
            // routing, so the template need not carry its slot.
            eprintln!(
                "flywheel: warn: --param {k} matches no {{{{{k}}}}} slot in stage — value ignored"
            );
        }
    }
    Ok(out)
}

// ---- learnings tier/reinforced + bounded GATE projection (ADR-0010 D12) ----

/// Learning corpus decay model (bd-fmya). Frontmatter carries
/// `tier: pinned|aging|perishable` + `reinforced: YYYY-MM-DD` (perishable
/// docs additionally name a checkable `expires:`). The GATE grounding read
/// is a BUDGETED PROJECTION of `docs/learnings/`, never the corpus itself
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

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
    pub struct LearningEntry {
        pub path: String,
        pub tier: LearningTier,
        /// `reinforced: YYYY-MM-DD` (None = never re-verified).
        pub reinforced: Option<String>,
        /// Estimated token cost of grounding on this entry (frontmatter +
        /// rg context). The projection sums these under the budget.
        pub cost_tokens: usize,
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
    pub fn gate_projection<'a>(
        entries: &'a [LearningEntry],
        today: NaiveDate,
        budget_tokens: usize,
    ) -> Vec<&'a LearningEntry> {
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
        // newest reinforcement first; never-reinforced (pinned only) last.
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

mod adr0010_d12_tests {
    // governed-by: ADR-0010 D12 (bd-fmya) — learnings tier/reinforced +
    // bounded GATE projection.
    use super::learning::{
        LearningEntry, LearningTier, gate_projection, parse_learning_frontmatter,
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
        }
    }

    // AC: GATE output bounded under budget with 50 seeded learnings.
    #[test]
    fn adr0010_d12_projection_bounded_under_budget_with_50_seeded() {
        let today = d("2026-09-07");
        let entries: Vec<LearningEntry> = (0..50)
            .map(|i| {
                entry(
                    &format!("docs/learnings/k/l{i:02}.md"),
                    LearningTier::Aging,
                    Some("2026-09-07"),
                    100,
                )
            })
            .collect();
        // budget fits 5 entries only
        let out = gate_projection(&entries, today, 500);
        assert_eq!(out.len(), 5, "projection stops at the budget");
        // newest-reinforced-first ranking: all fresh here, so stable order
        assert!(
            out.iter()
                .all(|e| e.reinforced.as_deref() == Some("2026-09-07"))
        );
    }

    // AC: un-reinforced 40d aging entry absent from projection, present in
    // corpus.
    #[test]
    fn adr0010_d12_unreinforced_40d_aging_entry_absent_from_projection() {
        let today = d("2026-09-07");
        let entries = vec![
            entry(
                "docs/learnings/k/stale.md",
                LearningTier::Aging,
                Some("2026-07-29"),
                100,
            ), // 40d
            entry(
                "docs/learnings/k/fresh.md",
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
        // corpus unchanged (projection never deletes)
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn adr0010_d12_tier_matrix() {
        let today = d("2026-09-07");
        let entries = vec![
            // pinned survives no reinforced stamp at all
            entry("docs/learnings/k/pin.md", LearningTier::Pinned, None, 100),
            // aging exactly at the 30d TTL: still in (<= 30d)
            entry(
                "docs/learnings/k/edge30.md",
                LearningTier::Aging,
                Some("2026-08-08"),
                100,
            ),
            // perishable fresh (<= 7d): in
            entry(
                "docs/learnings/k/perish-fresh.md",
                LearningTier::Perishable,
                Some("2026-09-03"),
                100,
            ),
            // perishable stale (> 7d): out
            entry(
                "docs/learnings/k/perish-old.md",
                LearningTier::Perishable,
                Some("2026-08-20"),
                100,
            ),
        ];
        let out = gate_projection(&entries, today, 10_000);
        let paths: Vec<&str> = out.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"docs/learnings/k/pin.md"));
        assert!(paths.contains(&"docs/learnings/k/edge30.md"));
        assert!(paths.contains(&"docs/learnings/k/perish-fresh.md"));
        assert!(!paths.contains(&"docs/learnings/k/perish-old.md"));
        // ranking: newest reinforced first (pinned-with-no-stamp last)
        assert_eq!(paths[0], "docs/learnings/k/perish-fresh.md");
    }

    // frontmatter parse: tier + reinforced (+ expires for perishable docs)
    #[test]
    fn adr0010_d12_frontmatter_parse() {
        let fm = "---\ntrack: bug\ntier: aging\nreinforced: 2026-09-07\n---\n# body";
        let (tier, reinforced) = parse_learning_frontmatter(fm).expect("parses");
        assert_eq!(tier, LearningTier::Aging);
        assert_eq!(reinforced, Some("2026-09-07".to_string()));
        // legacy entries (no tier/reinforced): unknown tier, no stamp
        let fm2 = "---\ntrack: bug\n---\n# body";
        let (tier2, r2) = parse_learning_frontmatter(fm2).expect("parses");
        assert_eq!(tier2, LearningTier::Aging); // migration default
        assert_eq!(r2, None);
        // no frontmatter at all
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

    // ---- ADR-0006 D1: parameterized stage payloads (bd-stage-params-pqo9) ----

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
    /// `ce-execute` is the name (clean cutover, no shims).
    #[test]
    fn stage_p_codes_retired_ce_names_resolve() {
        let lib = StageLibrary::compiled_in();
        for code in ["p1", "p2", "p4", "p5", "p6", "p8"] {
            assert!(lib.resolve(code).is_none(), "{code} must not resolve");
        }
        assert!(lib.has("ce-execute"), "ce-execute resolves");
    }

    /// W3 (bd-w3-stages-o8lg), renamed by ADR-0009: ce-execute names the
    /// three verification principles + the `flywheel ledger` CLI; ce-review
    /// carries the cargo-cult BLOCKER language and the ADR-0009 review
    /// contract (detection condition, evidence quotes, independence).
    #[test]
    fn ce_stages_cite_verification_principles_and_ledger() {
        let lib = StageLibrary::compiled_in();
        let execute = lib.resolve("ce-execute").expect("ce-execute resolves");
        for name in [
            "prove-it-works",
            "fix-root-causes",
            "sequence-verifiable-units",
        ] {
            assert!(execute.contains(name), "ce-execute must name {name}");
        }
        assert!(
            execute.contains("flywheel ledger check"),
            "ce-execute cites the ledger CLI"
        );
        assert!(
            execute.contains("IDEMPOTENT RE-ENTRY"),
            "ce-execute carries the ADR-0009 idempotent re-entry rule"
        );
        assert!(
            execute.contains("LEARNINGS"),
            "ce-execute brief carries the LEARNINGS field"
        );
        let review = lib.resolve("ce-review").expect("ce-review resolves");
        for name in ["prove-it-works", "fix-root-causes", "minimize-reader-load"] {
            assert!(review.contains(name), "ce-review must name {name}");
        }
        assert!(
            review.contains("BLOCKER"),
            "ce-review carries BLOCKER language"
        );
        assert!(
            review.contains("cargo-cult"),
            "ce-review rejects cargo-cult citations"
        );
        assert!(
            review.contains("DETECTION CONDITION"),
            "ce-review carries the ADR-0009 detection-condition contract"
        );
        assert!(
            review.contains("not two witnesses"),
            "ce-review carries the independence rule"
        );
    }

    /// ADR-0009: the compiled-in library exposes the ce- loop — brainstorm,
    /// plan, plan-review, execute, simplify, review, compound — plus
    /// commit-and-push.
    #[test]
    fn compiled_in_exposes_ce_loop() {
        let lib = StageLibrary::compiled_in();
        for name in [
            "ce-brainstorm",
            "ce-plan",
            "ce-plan-review",
            "ce-execute",
            "ce-simplify",
            "ce-review",
            "ce-compound",
            "commit-and-push",
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
            lib.get_stage("ce-execute").unwrap().phase.as_deref(),
            Some("implement")
        );
    }

    /// bd-close-ceremony-gate-sha-68ms (GS-028 B13-B15+B3): the taught close
    /// contract is gate → commit → sha-carrying close, and the claim assignee
    /// is a herdr slug, never a pin. Both the wave-dispatch stage and the
    /// /beads-commit template carry it — ceremony drift fails the build.
    #[test]
    fn close_ceremony_contract_names_gate_sha_and_herdr_assignee() {
        let lib = StageLibrary::compiled_in();
        let dispatch = lib
            .resolve("wave-dispatch")
            .expect("wave-dispatch resolves");
        for needle in [
            "gate report",
            "--commit-sha",
            "--assignee <herdr-name>",
            "commit BEFORE close",
        ] {
            assert!(
                dispatch.contains(needle),
                "wave-dispatch must teach {needle:?}"
            );
        }
        assert!(
            !dispatch.contains("--assignee <pin>"),
            "wave-dispatch must not teach pin assignees (br takes a herdr slug)"
        );

        let tpl = include_str!("../integrations/prompts/beads-commit.md.tpl");
        for needle in [
            "gate report",
            "--commit-sha",
            "Commit BEFORE close",
            "VERIFY tail",
        ] {
            assert!(tpl.contains(needle), "/beads-commit must teach {needle:?}");
        }
        let commit_pos = tpl.find("Commit BEFORE close").expect("order anchor");
        let close_pos = tpl.find("br close").expect("close anchor");
        assert!(
            commit_pos < close_pos,
            "/beads-commit must order commit before close"
        );
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
    fn playbook_stages_resolve() {
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
            "[stages.judge-arms]\nname = \"judge-arms\"\nprompt = \"MY JUDGE OVERRIDE\"\n",
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

    /// ADR-0009: a user `stages.toml` override replaces a compiled-in prompt
    /// (the ce-execute rename survives overrides — the override keys the
    /// semantic name).
    #[test]
    fn user_override_replaces_prompt() {
        // XDG_CONFIG_HOME mutation is process-global — serialize (193.2).
        let _g = crate::config::lock_env();
        let lib = StageLibrary::compiled_in();
        let old = lib.resolve("ce-execute").unwrap().to_string();
        let dir = std::env::temp_dir().join(format!("fw-stages-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::create_dir_all(dir.join("flywheel")).unwrap();
        std::fs::write(
            dir.join("flywheel/stages.toml"),
            "[stages.ce-execute]\nname = \"ce-execute\"\nprompt = \"MY OVERRIDE PROMPT\"\n",
        )
        .unwrap();
        // Rebuild with the override applied.
        let lib2 = StageLibrary::load().unwrap();
        assert_eq!(lib2.resolve("ce-execute"), Some("MY OVERRIDE PROMPT"));
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
            lib.get_stage("ce-execute").unwrap().role.as_deref(),
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
        // Without the project file, the user tier still resolves.
        let lib_user_only = StageLibrary::load().unwrap();
        assert_eq!(lib_user_only.resolve("w6-layered"), Some("USER TIER"));
        assert_eq!(lib_user_only.tier_of("w6-layered"), Some(Tier::User));
        // Seed tag on an untouched compiled entry.
        assert_eq!(lib.tier_of("ce-execute"), Some(Tier::Seed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-cfgv: a prompt-only tier override MERGES field-level — seed
    /// role/phase/params survive; a tier that re-declares a field wins it.
    #[test]
    fn cfgv_stage_override_field_level_merge() {
        let mut lib = StageLibrary::compiled_in();
        // Seed ce-execute carries role + phase (SEED_AFFINITY/SEED_PHASE).
        assert_eq!(
            lib.get_stage("ce-execute").unwrap().role.as_deref(),
            Some("coder")
        );
        // User tier overrides ONLY the prompt: role/phase inherit.
        lib.apply_toml(
            "[stages.ce-execute]\nname = \"ce-execute\"\nprompt = \"USER PROMPT\"\n",
            Tier::User,
        )
        .unwrap();
        let s = lib.get_stage("ce-execute").unwrap();
        assert_eq!(s.prompt, "USER PROMPT", "override wins its own field");
        assert_eq!(
            s.role.as_deref(),
            Some("coder"),
            "seed role survives prompt-only override"
        );
        assert_eq!(s.phase.as_deref(), Some("implement"), "seed phase survives");
        assert_eq!(
            lib.tier_of("ce-execute"),
            Some(Tier::User),
            "tier tag reports the top tier"
        );
        // Re-declaring a field replaces it (later tier wins per-field).
        lib.apply_toml(
            "[stages.ce-execute]\nname = \"ce-execute\"\nprompt = \"P2\"\nrole = \"reviewer\"\nparams = [\"bead\"]\n",
            Tier::Project,
        )
        .unwrap();
        let s = lib.get_stage("ce-execute").unwrap();
        assert_eq!(s.role.as_deref(), Some("reviewer"), "declared field wins");
        assert_eq!(s.params, vec!["bead".to_string()], "declared params win");
        assert_eq!(
            s.phase.as_deref(),
            Some("implement"),
            "undeclared phase still inherits"
        );
        // An unknown name in a tier is a fresh record, not a merge.
        lib.apply_toml(
            "[stages.fresh]\nname = \"fresh\"\nprompt = \"ONLY\"\n",
            Tier::User,
        )
        .unwrap();
        assert_eq!(lib.get_stage("fresh").unwrap().prompt, "ONLY");
        assert_eq!(lib.get_stage("fresh").unwrap().role, None);
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
        // ADR-0009: `compound` is valid phase vocabulary.
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

    /// ADR-0006 D3 + ADR-0009: the wave-swarm compose token's compiled-in
    /// stages resolve; DISPATCH briefs ce-execute (never fresh-eyes-explore)
    /// and carries the learnings param when the gate matched.
    #[test]
    fn wave_dispatch_briefs_ce_execute_with_learnings() {
        let lib = StageLibrary::compiled_in();
        let prompt = &lib
            .get_stage("wave-dispatch")
            .expect("wave-dispatch seed")
            .prompt;
        assert!(
            prompt.contains("--stage ce-execute"),
            "DISPATCH must brief the coder stage, got: {prompt}"
        );
        assert!(
            !prompt.contains("--stage p1"),
            "p1 is retired; coder execution is ce-execute"
        );
        assert!(
            prompt.contains("--param learnings="),
            "DISPATCH carries the gate's learnings into the brief (ADR-0009)"
        );
    }

    #[test]
    fn wave_swarm_stages_resolve_compiled_in() {
        let lib = StageLibrary::compiled_in();
        for stage in ["wave-gate", "wave-dispatch", "wave-observe", "wave-verify"] {
            assert!(lib.has(stage), "compiled-in stage {stage} resolves");
        }
    }
}

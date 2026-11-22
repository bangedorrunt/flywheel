// governed-by: ADR-0024
//! Compiled-in built-in workflows (ADR-0005 Spec 2b / P3.5).
//!
//! Zero-config: `flywheel workflow run debate --inputs '{proposal:"..."}'`
//! works on a fresh install. Each built-in carries the EXACT YAML from ADR
//! Spec 2b — purpose-named, composing profiles (Spec 1b) + stages (Spec 2a)
//! + control-flow (Spec 4). The catalog loads these as the compiled-in
//!   defaults, overridable by `~/.config/flywheel/workflows/*.yaml`.
//!
//! - `ce-brainstorm` — graph: parallel dispatch @oracle (over 3) → @planner
//!   reduce via the ce-brainstorm stage (fail_run on reduce).
//! - `ce-plan` — dispatch @planner stage:ce-plan (task carries
//!   {{inputs.goal}}; fail_run on decompose) → request_approval gate.
//! - `beads` — dispatch @planner stage:ce-execute (spec.plan carries
//!   the plan; fail_run on create).
//! - `debate` — graph: parallel dispatch @reviewer (over 2) → @oracle decide
//!   (task consumes {{steps.briefs}}; fail_run on decide).
//! - `goal` — meta: call_workflow ce-brainstorm→plan→beads→dispatch @coder
//!   stage:ce-execute→@reviewer stage:ce-review→@housekeeper
//!   stage:commit-and-push; each dispatch carries the prior step's output via
//!   spec.plan, each fails the run on error.
//! - `triage` — the canonical message_posted reactive workflow (payload
//!   fields only: {{trigger.content}} / {{trigger.id}} — no subject).
//! - `onboard` / `triage-deep` — cold-start + deep-backlog chains; each step
//!   consumes the prior step's output via spec.plan, fail_run on every step.
//! - `captain-audit` — read-only analysis (bd-wsui): scout → oracle →
//!   reviewer graph, no beads, no repo edits, report written to
//!   `{{inputs.report_path}}`.
//! - `interrogate` — adversarial N-model review (bd-w2-review-s6ij): N
//!   reviewers (default 2) on DIFFERENT model families, same rubric,
//!   READONLY; the oracle parent synthesizes Act-on / Consider / Noted /
//!   Dismissed. Extends, does not delete, `debate`.
//! - `forensics` — diagnosis-only (bd-w2-review-s6ij): diagnose @scout —
//!   instrument or read the artifact; the deliverable is the DIAGNOSIS. NO
//!   fix step (lint-enforced); a fix is a separate bug-fix.
//! - `figure-it-out` — decide-only, no builtin YAML (bd-w2-review-s6ij): `figure it out` → `workflow compose --agent` (planner authors YAML, lint is the gate; the captain executes via --execute).
//! - `beads-lifecycle` — compose token (ADR-0008 §6.7, bd-adr0008-token-yaml-kimg):
//!   grill → create → review1 → review2 → integrate → implement → verify →
//!   outer SIGNOFF; `inputs.until` early-exit + `gate_before_implement` outer
//!   gate; nests ordinary builtins via call_workflow, never bare `--parts`.
//! - `babysit-bead` / `ship-beads` / `autonomous-work` / `pickup` / `pause`
//!   — the operations playbooks (ADR-0019 Spec 7.2, bd-w2-ops-szky):
//!   babysit drives ONE bead to a legal ledger row; ship-beads gates on the
//!   ship predicate (no ready beads, claim-watch empty, closed beads
//!   ledger-legal) then commits+pushes (p8); autonomous-work runs the REAL
//!   loop (`loop run --work --on-blocked mail`) then ship-beads; pickup
//!   resumes stranded/exhausted work + ledger gaps; pause checkpoints +
//!   writes a standing handoff (kills nothing working without
//!   --force-working).
//!
//! governed-by: ADR-0005 Spec 2b, P3.5

use std::collections::BTreeMap;

use toron_workflow::schema::{Workflow, parse_workflow};

// governed-by: ADR-0022 §3.3 (phase-native built-ins, bd-cvv9)
/// The compiled-in built-in workflows, name → YAML text.
pub const BUILTIN_WORKFLOWS_YAML: &[(&str, &str)] = &[
    (
        "ce-compound",
        r#"
name: ce-compound
description: "The learning gate (compound + sweep + reflect): capture the durable learning from a verified bead/wave/session into chiebukuro, sweep recorded lessons, and reflect on the run. Folds ce-sweep and reflect. One learning per run; clean_skip is a success."
trigger:
  type: webhook
crew: auto
principles:
  - encode-lessons-in-structure
inputs:
  - name: source
    default: ""
  - name: bead
    default: ""
  - name: depth
    default: "full"
steps:
  - name: compound
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      phase: compound
      stage: ce-compound
      task: 'Run the compound gate over: bead {{inputs.bead}} {{inputs.source}}. Depth {{inputs.depth}}: full runs the complete gate with all recall lanes; lightweight captures one learning with minimal probes. Ground in the commit at HEAD and receipts under .flywheel/receipts/. Recall lanes are chie qmd search + chie memory recall (labeled, offline-safe). Qualifying learnings are written with `flywheel memory record-lesson` as one X-breaks-when-Y-do-Z line — chiebukuro is the ONLY learning store; no repo-side inbox or perishable file is written, and no human gate sits at write time. Reply with ONLY the terminal JSON. The terminal signal (recorded|clean_skip) IS the run verdict. One learning per run.'
    on_error: continue
  - name: sweep
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      task: 'SWEEP recorded chiebukuro lessons: PROMOTE each durable one, FORGET each expired/untrue one, REINFORCE each re-verified one. Classify Keep / Update / Consolidate / Replace / Forget. Reply with ONLY a JSON array [{lesson, verdict}].'
    on_error: continue
  - name: reflect
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      task: 'REFLECT on run {{inputs.bead}}: name what worked, what did not, and one structural improvement (a helper, a check, a guard) that would have made this run faster or safer. Reply with ONLY a JSON object {"worked": ["..."], "failed": ["..."], "improvement": "<one concrete change>"}.'
    on_error: fail_run
"#,
    ),
    (
        "ce-lfg",
        r#"
name: ce-lfg
description: "The go pipeline: ideate → plan (gated unless direct) → track (unless direct) → implement (single build or wave dispatch) → review → verify → ship. Replaces goal, goal-direct, ship, the phase-* chain, plan-direct, ce-plan(+review), beads, beads-lifecycle, wave-swarm and feature."
trigger:
  type: webhook
crew: auto
inputs:
  - name: objective
  - name: data_shape
    default: ""
  - name: direct
    default: false
  - name: track
    default: true
  - name: wave
    default: false
  - name: polish
    default: false
steps:
  - name: ideate-oracle
    action: dispatch_job
    assignee: x
    spec: { profile: "@oracle", task: "Diverge on the question — surface ideas, angles, and options: {{inputs.objective}}" }
    on_error: fail_run
  - name: ideate-structure
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", task: "From a STRUCTURAL angle on the question, map what a solution must satisfy — the moving parts, the constraints, the shape of the work: {{inputs.objective}}" }
    on_error: fail_run
  - name: ideate-risk
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer", task: "From a RISK angle on the question, attack it — what could go wrong, what would make this a bad idea, what would you refuse to sign off on: {{inputs.objective}}" }
    on_error: fail_run
  - name: ideate-reduce
    action: dispatch_job
    assignee: x
    spec:
      profile: "@planner"
      stage: "ce-brainstorm"
      task: "Reduce these into ranked options for: {{inputs.objective}}"
    on_error: fail_run
  - name: plan
    action: dispatch_job
    assignee: x
    spec:
      profile: "@planner"
      stage: "ce-plan"
      task: 'Decompose the goal into a bead-ready plan: {{inputs.objective}}. Data shape: {{inputs.data_shape}}.'
    on_error: fail_run
  - name: polish
    action: loop
    max: 3
    until: steps_polish_ok == true
    on_error: fail_run
    body:
      - name: polish-round
        action: dispatch_job
        assignee: x
        spec:
          profile: "@planner"
          stage: "plan-polish"
          task: 'Polish round for {{inputs.objective}}.'
        on_error: fail_run
  - name: gate
    action: request_approval
    message: "Approve plan for {{inputs.objective}}?"
    timeout_secs: 3600
    on_error: fail_run
  - name: track
    action: dispatch_job
    assignee: x
    spec:
      profile: "@planner"
      stage: "ce-execute"
      task: 'Create the tracked beads for: {{steps.plan}}.'
    on_error: fail_run
  - name: implement-wave
    action: foreach
    over: "{{steps.track}}"
    each:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@coder"
        stage: "ce-execute"
        task: 'Wave bead {{item}} for {{inputs.objective}}: claim, build with TDD, commit with the bead id, verify and close.'
      on_error: fail_run
  - name: implement
    action: dispatch_job
    assignee: x
    spec:
      profile: "@coder"
      stage: "ce-execute"
      task: 'Implement {{inputs.objective}}. TDD first.'
    on_error: fail_run
  - name: review
    action: dispatch_job
    assignee: x
    spec:
      profile: "@reviewer"
      harness: "opencode"
      plan: "{{steps.implement-wave}}{{steps.implement}}"
      task: 'Adversarial review with quoted evidence. Verdict: AGREE, AGREE-WITH-CHANGES, or REJECT.'
    on_error: fail_run
  - name: verify
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      task: 'Run the acceptance gate and record its outcome verbatim.'
    on_error: fail_run
  - name: ship
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      stage: "commit-and-push"
      task: 'Ship {{inputs.objective}}: commit with bead ids, push, report shas.'
    on_error: fail_run
"#,
    ),
    (
        "ce-debug",
        r#"
name: ce-debug
description: "Fix path: diagnose first, then repair-until-verified loop with independent proof. --diagnose-only stops at the diagnosis. Replaces bug-fix, forensics and repair-until-verified."
trigger:
  type: webhook
crew: auto
inputs:
  - name: symptom
  - name: artifact
    default: ""
  - name: repro
    default: ""
  - name: verify
    default: ""
  - name: diagnose_only
    default: false
  - name: max_iterations
    default: 3
principles:
  - fix-root-causes
  - prove-it-works
  - sequence-verifiable-units
steps:
  - name: diagnose
    action: dispatch_job
    assignee: x
    spec:
      profile: "@scout"
      task: "Diagnose {{inputs.symptom}} (artifact: {{inputs.artifact}}). The deliverable is the DIAGNOSIS (root cause + evidence); no fix in this step."
    on_error: fail_run
  - name: repro
    action: dispatch_job
    assignee: x
    spec:
      profile: "@scout"
      task: "Reproduce: {{inputs.symptom}}. {{inputs.repro}}. ACCEPTANCE: the command exits non-zero (or visibly shows the bug). Record the exact failing output."
    on_error: fail_run
  - name: repair
    action: loop
    max: "{{inputs.max_iterations}}"
    until: steps_verify_ok == true
    on_error: fail_run
    body:
      - name: fix
        action: dispatch_job
        assignee: x
        spec:
          profile: "@coder"
          context: fresh
          task: "Repair {{inputs.symptom}}: smallest change. VERIFY: {{inputs.verify}}. Stop when VERIFY passes."
        on_error: fail_run
      - name: verify
        action: verify
        command: "{{inputs.verify}}"
        on_error: continue
  - name: prove
    action: dispatch_job
    assignee: x
    spec:
      profile: "@reviewer"
      harness: "opencode"
      task: "Independently prove the fix for {{inputs.symptom}}. VERIFY from the bead: test green and repro gone. Record unit-test-verified. Do not re-fix."
    on_error: fail_run
"#,
    ),
    (
        "ce-bakeoff",
        r#"
name: ce-bakeoff
description: "Design bake-off: N arms run the SAME brief — sketch arms in parallel, compile arms one at a time under the build slot — then an @oracle judge picks ONE per the up-front selection rule. Replaces prototype and arena."
trigger:
  type: webhook
crew: auto
inputs:
  - name: question
  - name: n
    default: 2
  - name: mode
    default: sketch
  - name: selection
    default: best
principles:
  - exhaust-the-design-space
  - separate-before-serializing-shared-state
steps:
  - name: arms
    action: branch
    if: "{{inputs.mode}} == compile"
    then:
      - name: arm
        action: foreach
        build_slot: true
        over: "{{inputs.n}}"
        each:
          action: dispatch_job
          assignee: x
          spec:
            profile: "@coder"
            task: "BAKEOFF COMPILE ARM {{item}} of {{inputs.n}} for: {{inputs.question}}. One arm at a time on a scratch ref; measure, then reset so the next arm starts clean. No beads, no .beads writes."
            on_error: fail_run
    else:
      - name: arm
        action: parallel
        over: "{{inputs.n}}"
        each:
          action: dispatch_job
          assignee: x
          spec:
            profile: "@coder"
            task: "BAKEOFF SKETCH ARM (you are ONE of {{inputs.n}}): NON-COMPILING design sketch for {{inputs.question}} — API shapes, tradeoffs, one recommendation. TEXT ONLY."
            on_error: fail_run
  - name: judge
    action: dispatch_job
    assignee: x
    spec:
      profile: "@oracle"
      task: "Judge the arms for {{inputs.question}} by the selection rule: {{inputs.selection}}. Pick ONE winner with justification; no merged hybrids without a graft bead."
    on_error: fail_run
"#,
    ),
    (
        "ce-review",
        r#"
name: ce-review
description: "N-model review (--models N): independent reviewers brief the target, synthesize, adjudicate. Replaces interrogate, debate and captain-audit."
trigger:
  type: webhook
crew: auto
inputs:
  - name: target
  - name: models
    default: "2"
  - name: report
    default: ""
principles:
  - fix-root-causes
  - guard-the-context-window
steps:
  - name: briefs
    action: parallel
    over: "{{inputs.models}}"
    each:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@reviewer"
        harness: "opencode"
        task: "READ-ONLY adversarial review of {{inputs.target}} (model {{item}} of {{inputs.models}}): hunt material defects with quoted evidence; verdict per finding: Act-on / Consider / Noted / Dismissed. No fixes."
      on_error: fail_run
  - name: synthesize
    action: dispatch_job
    assignee: x
    spec:
      profile: "@oracle"
      task: "Adjudicate the reviewer briefs for {{inputs.target}}: group by Act-on / Consider / Noted / Dismissed with the supporting evidence.{{inputs.report}}"
    on_error: fail_run
"#,
    ),
    (
        "ce-measure",
        r#"
name: ce-measure
description: "Measured improvement: baseline → ONE change → measure → KEEP (commit) or DISCARD; loop while improving, capped. Replaces perf, hillclimb and refactoring."
trigger:
  type: webhook
crew: auto
inputs:
  - name: metric
  - name: baseline_cmd
  - name: budget
    default: ""
  - name: target
    default: ""
  - name: max_iters
    default: 1
principles:
  - prove-it-works
  - subtract-before-you-add
steps:
  - name: baseline
    action: dispatch_job
    assignee: x
    spec:
      profile: "@scout"
      task: "Baseline {{inputs.metric}} (and invariants for {{inputs.target}} when set): run `{{inputs.baseline_cmd}}`, record the number. {{inputs.budget}}"
    on_error: fail_run
  - name: change
    action: dispatch_job
    assignee: x
    spec:
      profile: "@coder"
      task: "Make ONE change aimed at {{inputs.metric}} ({{inputs.target}}). Subtract-first: delete what can go; no V2 files, no scope creep."
    on_error: fail_run
  - name: measure
    action: dispatch_job
    assignee: x
    spec:
      profile: "@scout"
      task: "Measure {{inputs.metric}} with the SAME command `{{inputs.baseline_cmd}}`. KEEP (commit) on improvement within budget, else DISCARD and restore exactly."
    on_error: fail_run
"#,
    ),
    (
        "ce-triage",
        r##"
name: ce-triage
description: "Triage inbound work: acknowledge, robot-triage the backlog, prioritize and assign (deep), or quick-triage and hand off. Replaces triage and triage-deep."
trigger:
  type: message_posted
  channel: "#support"
crew: auto
inputs:
  - name: project
    default: "flywheel"
  - name: deep
    default: true
steps:
  - name: ack
    action: send_dm
    to: "operator"
    body: "Received {{trigger.content}}"
    on_error: continue
  - name: triage
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      task: "Triage for {{inputs.project}} (deep: {{inputs.deep}}). Deep: bv robot-triage, prioritize and assign each bead with an owner. Shallow: acknowledge and route only, no new beads."
    on_error: fail_run
"##,
    ),
    (
        "ce-resume",
        r#"
name: ce-resume
description: "Resume, babysit, or pause: pick up stranded work, drive one bead, or checkpoint and stop. Replaces pickup, pause, babysit-bead and autonomous-work."
trigger:
  type: webhook
crew: auto
inputs:
  - name: project
  - name: mode
    default: pickup
  - name: bead_id
    default: ""
steps:
  - name: resume
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      task: "ce-resume for {{inputs.project}} (mode {{inputs.mode}}, bead {{inputs.bead_id}}). pickup: read agent state, br ready/in-progress, ledger gaps, respawn dead panes, resume stranded briefs. babysit: drive ONLY {{inputs.bead_id}} to a legal ledger row (never start new work). pause: checkpoint, write the handoff, kill nothing working. autonomous-work: run the loop, then ship-beads predicate."
    on_error: fail_run
"#,
    ),
    (
        "ce-retro",
        r#"
name: ce-retro
description: "The learning loop (cadence daily/weekly): recall chiebukuro, promote durable learnings, forget expired ones, reinforce re-verified ones. Folds weekly-review, awareness-digest, ce-sweep and reflect."
trigger:
  type: webhook
crew: auto
inputs:
  - name: bead
    default: ""
  - name: depth
    default: "full"
steps:
  - name: recall
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      task: 'Recall learnings relevant to bead {{inputs.bead}} via chiebukuro (chie qmd search + chie memory recall). Depth {{inputs.depth}}: full captures one learning with full probes; lightweight captures one with minimal probes. Reply with ONLY a JSON object: {"learnings": ["<one X-breaks-when-Y-do-Z line>"]}.'
    on_error: fail_run
  - name: settle
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      task: 'Settle the recalled learnings: PROMOTE each durable one with flywheel memory record-lesson, FORGET each expired one with chie, REINFORCE each re-verified one by re-recording with reinforced date. Reply with ONLY the terminal JSON.'
    on_error: fail_run
"#,
    ),
    (
        "mutation-full",
        r#"
name: mutation-full
description: "Nightly full mutation proof (bd-m5afp): a clean export of HEAD runs every spec under scripts/mutations/, so no witness is ever only as fresh as the last landing that touched its file. A red spec files a bead."
trigger:
  type: schedule
  cron: "30 3 * * *"
  owner: housekeeper
  scope: host
policy:
  on_failure: bead
  on_success: log
  max_retries: 0
  no_claim_timeout: 600
steps:
  - name: full-mutations
    action: verify
    command: 'bash scripts/clean-proof.sh --head --mutations full --receipt /tmp/clean-proof-mutation-full.json'
    on_error: fail_run
"#,
    ),
];

/// Load the compiled-in built-in workflows as parsed `Workflow`s.
/// The compiled-in YAML for a builtin, VERBATIM.
///
/// bd-3o5t: seed and drift must both read this text, never
/// `serde_yaml::to_string(&builtin)`. The struct round-trip is LOSSY:
/// `serde_yaml` emits `crew: null` for every workflow whose `crew` is
/// `Some(..)` (measured across all 20+ crew workflows — arena, beads, bug-fix,
/// debate, feature, ...), because `CrewSpec` does not survive
/// serialize→deserialize. Seeding that text therefore wrote a user-tier copy
/// with the crew stripped, and because the user tier WINS over the builtin, a
/// freshly seeded machine turned every crew workflow into a headless,
/// zero-agent one. drift then agreed with seed — same broken serializer on both
/// sides — so it printed "clean" over the damage. Copying the source text is
/// lossless by construction and makes the two commands share one truth.
pub fn builtin_workflow_yaml(name: &str) -> Option<&'static str> {
    BUILTIN_WORKFLOWS_YAML
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, yaml)| *yaml)
}

/// Every compiled-in workflow as `(name, verbatim yaml)`, for callers that need
/// the source text rather than the parsed struct (seed, drift).
pub fn builtin_workflows_yaml() -> impl Iterator<Item = (&'static str, &'static str)> {
    BUILTIN_WORKFLOWS_YAML.iter().copied()
}

pub fn builtin_workflows() -> BTreeMap<String, Workflow> {
    let stages = crate::stages::StageLibrary::shared();
    let mut map = BTreeMap::new();
    for (name, yaml) in BUILTIN_WORKFLOWS_YAML {
        match parse_workflow(yaml) {
            Ok(mut wf) => {
                // bd-6setu.6: a review step's stage-contract reads ride the job
                // spec, so toron's CLAIM can refuse the node's author.
                crate::workflow_lowering::stamp_claim_exclusions(&mut wf, stages);
                map.insert(name.to_string(), wf);
            }
            Err(e) => {
                panic!("built-in workflow {name} failed to parse: {e}");
            }
        }
    }
    map
}

/// Compact agent-facing guidance for the built-in catalog:
/// `(family, selection)`. The full workflow description remains available
/// from `workflow show`; list consumers should use this bounded projection
/// first so catalog discovery does not spend context on prose.
pub const WORKFLOW_GUIDANCE: &[(&str, &str, &str)] = &[
    (
        "ce-lfg",
        "delivery",
        "go pipeline: ideate, plan, track, build, review, verify, ship",
    ),
    (
        "ce-debug",
        "repair",
        "repro, repair loop, independent proof",
    ),
    (
        "ce-bakeoff",
        "design-space",
        "prototype or arena arms, oracle picks one",
    ),
    (
        "ce-review",
        "review",
        "adversarial review, N-model or debate",
    ),
    (
        "ce-measure",
        "measurement",
        "baseline, change, keep or discard; hillclimb loops",
    ),
    (
        "ce-triage",
        "operations",
        "triage inbound or backlog, assign",
    ),
    (
        "ce-resume",
        "operations",
        "resume, babysit, pause, ship beads",
    ),
    (
        "ce-compound",
        "compound",
        "capture one durable lesson after verified work",
    ),
    (
        "ce-retro",
        "compound",
        "recall, promote, forget, reinforce learnings",
    ),
    (
        "mutation-full",
        "ops",
        "nightly full mutation proof; a red spec files a bead",
    ),
];

/// Return bounded catalog guidance for one built-in workflow.
pub fn workflow_guidance(name: &str) -> Option<(&'static str, &'static str)> {
    WORKFLOW_GUIDANCE
        .iter()
        .find(|(workflow, _, _)| *workflow == name)
        .map(|(_, family, selection)| (*family, *selection))
}

/// ADR-0006 D3: a compose token's parsed workflow for compose expansion
/// (the token expands where it sits; its `inputs:` declarations merge
/// into the composed workflow so template refs lint clean). Pure.
pub fn builtin_recipe(name: &str) -> Option<Workflow> {
    if let Some(wf) = builtin_workflows().remove(name) {
        return Some(wf);
    }
    // Old names resolve via aliases (one-release deprecation).
    if let Some(target) = resolve_workflow_alias(name) {
        return builtin_workflows().remove(target);
    }
    None
}

/// Old workflow names (pre-merge) map to their replacement ce-* workflow.
/// Aliases print one deprecation line for one release; a follow-up bead deletes them.
/// Maps every absorbed name: the 16 folded into ce-lfg, 3 into ce-debug, 2 into
/// ce-bakeoff, 3 into ce-review, 3 into ce-measure, 2 into ce-triage, 5 into
/// ce-resume, 3 into ce-compound (incl. ce-sweep/reflect), 2 into ce-retro.
pub const WORKFLOW_ALIASES: &[(&str, &str)] = &[
    ("goal", "ce-lfg"),
    ("goal-direct", "ce-lfg"),
    ("ship", "ce-lfg"),
    ("phase-resolve", "ce-lfg"),
    ("phase-plan", "ce-lfg"),
    ("phase-implement", "ce-lfg"),
    ("phase-review", "ce-lfg"),
    ("phase-verify", "ce-lfg"),
    ("plan-direct", "ce-lfg"),
    ("ce-plan", "ce-lfg"),
    ("ce-plan-review", "ce-lfg"),
    ("beads", "ce-lfg"),
    ("beads-lifecycle", "ce-lfg"),
    ("wave-swarm", "ce-lfg"),
    ("feature", "ce-lfg"),
    ("ce-brainstorm", "ce-lfg"),
    ("bug-fix", "ce-debug"),
    ("forensics", "ce-debug"),
    ("repair-until-verified", "ce-debug"),
    ("arena", "ce-bakeoff"),
    ("prototype", "ce-bakeoff"),
    ("interrogate", "ce-review"),
    ("debate", "ce-review"),
    ("captain-audit", "ce-review"),
    ("perf", "ce-measure"),
    ("hillclimb", "ce-measure"),
    ("refactoring", "ce-measure"),
    ("triage", "ce-triage"),
    ("triage-deep", "ce-triage"),
    ("pickup", "ce-resume"),
    ("pause", "ce-resume"),
    ("babysit-bead", "ce-resume"),
    ("autonomous-work", "ce-resume"),
    ("ship-beads", "ce-resume"),
    ("ce-sweep", "ce-compound"),
    ("reflect", "ce-compound"),
    ("weekly-review", "ce-retro"),
    ("awareness-digest", "ce-retro"),
    ("onboard", "ce-lfg"),
];

/// Resolve an old workflow name to its replacement, printing one deprecation line.
/// Returns None if the name is not an alias.
pub fn resolve_workflow_alias(name: &str) -> Option<&'static str> {
    WORKFLOW_ALIASES
        .iter()
        .find(|(old, _)| *old == name)
        .map(|(_, new)| {
            eprintln!(
                "flywheel: deprecated workflow '{name}' — use '{new}' instead (old names will be removed)"
            );
            *new
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_workflow::lint_yaml;
    use toron_workflow::schema::CrewSpec;

    /// Every built-in has bounded agent-facing guidance, so discovery never
    /// depends on rereading the full workflow description.
    #[test]
    fn workflow_guidance_covers_every_builtin() {
        let wfs = builtin_workflows();
        assert_eq!(WORKFLOW_GUIDANCE.len(), wfs.len());
        for name in wfs.keys() {
            assert!(workflow_guidance(name).is_some(), "guidance for {name}");
        }
    }

    /// All built-ins parse (ADR-0005 Spec 2b acceptance).
    #[test]
    fn all_builtins_parse() {
        let wfs = builtin_workflows();
        assert_eq!(
            wfs.len(),
            BUILTIN_WORKFLOWS_YAML.len(),
            "all built-ins load"
        );
        for name in [
            "ce-lfg",
            "ce-debug",
            "ce-bakeoff",
            "ce-review",
            "ce-measure",
            "ce-triage",
            "ce-resume",
            "ce-compound",
            "ce-retro",
            "mutation-full",
        ] {
            assert!(wfs.contains_key(name), "built-in {name} present");
        }
        assert!(
            !wfs.contains_key("figure-it-out"),
            "figure-it-out is decide-routed to compose --agent, not a builtin YAML"
        );
    }

    /// bd-cvv9 (ADR-0022 §8 W3 box): `ship` renders the FIVE-link chain
    /// (resolve → plan → implement → review → verify) in the structural
    /// dry-run — the graph the gated runner walks.
    /// bd-w2-review-s6ij: interrogate defaults N=2 with a @reviewer briefs
    /// fanout (readonly lives in the task text; lint-asserted clean);
    /// forensics has a @scout diagnose node and NO fix-named node
    /// (lint-asserted — a fix is a separate bug-fix); figure-it-out authors
    /// via @planner.
    /// bd-t86p: the goal DIRECT arm is a built-in (`goal-direct` + its
    /// gate-free `plan-direct` companion). Both parse, lint clean, and
    /// declare crew: auto — the captain-free compose surface needs
    /// zero-config auto-crew like the parent goal.
    /// The five operations playbooks are builtins; each cites its principles
    /// and is crew: auto; autonomous-work documents the decide target.
    /// ship-beads = predicate gate (no ready beads + claim-watch empty +
    /// every newly closed bead ledger-legal) THEN the p8 commit-and-push.
    /// autonomous-work uses the REAL loop (never reimplements it) and ends
    /// in a ship-beads call; the predicate input defaults to ship-beads.
    /// pause kills nothing working unless force_working is explicit (the
    /// input defaults false and the task gates on it).
    /// babysit-bead drives ONE bead only — the task never starts new beads
    /// and never writes a verdict (loop-only gate rows).
    /// bd-t86p: goal-direct is goal MINUS the tracker + the plan gate — the
    /// direct arm's steps are ideate/plan/build/review/ship (no `beads`
    /// call, and `plan-direct` has no `request_approval` step). This pins
    /// the autonomous contract: a steer --direct run never parks on an
    /// approval or depends on the tracker.
    /// STEER-COMPOSE (bd-2pcq #7): the goal workflows' arm selection is
    /// structural — goal = bead-tracked chain (brainstorm → plan → beads),
    /// goal-direct = direct chain (brainstorm → plan-direct, no beads). The
    /// steer surface picks the arm by workflow NAME (goal vs goal-direct);
    /// this pins that the names carry the right composition.
    /// bd-pqwx: goal's plan step under-supplied phase-plan's declared
    /// `data_shape`, so phase-plan's task kept the literal
    /// `{{inputs.data_shape}}`; that payload flowed into `steps.track` and
    /// the strict execution render of goal's build `$.spec.plan` failed
    /// loud (2026-09-14 live: `render: workflow "goal" node "build" field
    /// "$.spec.plan"`). goal now declares `data_shape` with an empty
    /// default (`bind_input_defaults` fills it on a bare-objective invoke)
    /// and passes it through exactly like the ship chain does.
    /// WORKFLOW-COMPOSE (bd-2pcq #6): the call_workflow chain resolves across
    /// the catalog+builtins resolver — every sub-workflow name referenced by
    /// the steer arms must exist as a built-in (the resolver returns a
    /// definition, never "unknown workflow").
    /// STEER-COMPOSE crew strategy (bd-2pcq #7): the composed goal/direct
    /// fleet includes the sub-workflow roles — a steer run spawns the WHOLE
    /// fleet (oracle/planner from brainstorm + planner from plan-direct +
    /// own coder/reviewer/housekeeper), never "no crew pane" on the first
    /// composed dispatch. (The recursive inference itself is unit-tested in
    /// crew.rs; this pins the end-to-end builtin composition.)
    /// The purpose-named workflows declare `crew: auto` (zero-config,
    /// auto-crew — Spec 4c). `triage` is the reactive one (no crew).
    #[test]
    fn purpose_named_workflows_are_crew_auto() {
        let wfs = builtin_workflows();
        for name in [
            "ce-lfg",
            "ce-debug",
            "ce-bakeoff",
            "ce-review",
            "ce-measure",
            "ce-triage",
            "ce-resume",
            "ce-compound",
            "ce-retro",
        ] {
            assert_eq!(
                wfs[name].crew,
                Some(CrewSpec::Auto),
                "{name} is crew: auto (zero-config auto-crew)"
            );
        }
    }

    /// bd-42fs step 5: every built-in REVIEWER dispatch step carries the
    /// per-step `harness: opencode` override (the decided lane for review/
    /// debate) — the profile itself stays omp; the override is per-step.
    #[test]
    fn reviewer_steps_override_harness_to_opencode() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        fn scan_action(action: &Action, name: &str, found: &mut u32) {
            match action {
                Action::DispatchJob { spec, .. }
                    if spec.profile.as_deref() == Some("@reviewer") =>
                {
                    assert_eq!(
                        spec.harness.as_deref(),
                        Some("opencode"),
                        "{name}: reviewer step must override harness to opencode"
                    );
                    *found += 1;
                }
                Action::DispatchJob { .. } => {}
                Action::Parallel { each, .. } => scan_action(&each.action, name, found),
                Action::Retry { step, .. } => scan_action(&step.action, name, found),
                Action::Branch {
                    then, else_steps, ..
                } => {
                    for s in then.iter().chain(else_steps.iter()) {
                        scan_action(&s.action, name, found);
                    }
                }
                Action::Foreach { each, .. } => scan_action(&each.action, name, found),
                Action::Loop { body, .. } => {
                    for st in body.iter() {
                        scan_action(&st.action, name, found);
                    }
                }
                _ => {}
            }
        }
        for name in ["ce-review"] {
            let mut found = 0;
            for step in &wfs[name].steps {
                scan_action(&step.action, name, &mut found);
            }
            if let Some(g) = &wfs[name].graph {
                for node in g.nodes.values() {
                    scan_action(&node.action, name, &mut found);
                }
            }
            assert!(found > 0, "{name}: expected at least one @reviewer step");
        }
    }

    /// `goal` is the meta-workflow: its steps are call_workflow chains.
    /// bd-h9xn (extraction proof): the ADR-0019 playbooks compose from
    /// stages — every dispatch spec that carries a `stage:` ref must NOT
    /// also carry an inline `task:` (the stage IS the discipline; `plan:`
    /// carries per-run interpolation). bug-fix is the documented EXCEPTION:
    /// its four nodes keep `task:` because the sc_bugfix_playbook ORDER test
    /// reads raw `spec.task` from the bare engine (no stage resolution
    /// exists in toron-workflow) — pinned by the test contract. Scope: the
    /// nine rewired playbooks (the stage list in the bead); ship-beads is
    /// NOT in scope — it pre-existed with a `stage` + `task` pair where the
    /// inline task wins by design (bd-m7ac).
    /// ADR-0024 Scenario B: bug-fix fix path via repair-until-verified (verify + max_iterations 3)
    /// Every built-in lints clean (the lint gate over all compiled-in
    /// workflows — ADR-0005 Lint-time checks).
    #[test]
    fn all_builtins_lint_clean() {
        for (name, yaml) in BUILTIN_WORKFLOWS_YAML {
            let report = lint_yaml(yaml).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                report.is_clean(),
                "built-in {name} must lint clean: {:?}",
                report.issues
            );
        }
    }

    /// The periodic digest is host-native (`awareness::tick`): paying an agent
    /// every 15 minutes to re-read local state is the cost this removes. The
    /// catalog entry stays as the ON-DEMAND narrative, so it must not carry a
    /// schedule trigger — a trigger would schedule the LLM path again.
    /// bd-m7ac/bd-gtw4i: weekly-review is a schedule-triggered STEPS fan-out —
    /// 3 elastic scouts feeding the oracle that consumes them. Steps, not a
    /// graph: the durable runner lowers a steps fan-out into copies and refuses
    /// a graph parallel node (see the guard test below).
    /// bd-gtw4i: a cron can never ship a workflow its runner REFUSES. Every
    /// schedule-triggered catalog workflow must open on the durable models —
    /// the check the weekly-review failure slipped past for a whole week.
    #[test]
    fn every_scheduled_catalog_workflow_opens_on_the_durable_models() {
        use toron_workflow::schema::{CronScope, Trigger};
        let mut scheduled = 0;
        for (name, wf) in builtin_workflows() {
            let Some(Trigger::Schedule { scope, .. }) = &wf.trigger else {
                continue;
            };
            // Project-scoped schedules are the ones a project cron fires.
            if !matches!(scope, CronScope::Project | CronScope::Host) {
                continue;
            }
            scheduled += 1;
            if let Some(reason) = toron_workflow::engine::durable::unsupported_reason(&wf) {
                panic!(
                    "cron workflow {name} cannot open on the durable models: {reason} \
                     — a cron must not ship a workflow its runner refuses"
                );
            }
            // The cron fires the TICK workflow rebuilt from the CronSpec, not
            // the catalog Workflow; that is the shape the runner actually opens.
            let spec = crate::cron::builtin_crons()
                .remove(&name)
                .unwrap_or_else(|| panic!("{name} is a builtin cron"));
            let tick = toron_workflow::cron::spec_tick_workflow(&spec);
            if let Some(reason) = toron_workflow::engine::durable::unsupported_reason(&tick) {
                panic!(
                    "the tick workflow a cron actually fires ({name}-tick) cannot open: {reason}"
                );
            }
        }
        assert!(
            scheduled >= 1,
            "expected the scheduled catalog, found {scheduled}"
        );
    }

    /// bd-gtw4i: the diagnosis, pinned. A GRAPH parallel node is the shape the
    /// durable runner refuses — its join must name copies that the graph
    /// validator also requires to be declared nodes, so no author can satisfy
    /// both. weekly-review therefore lives in steps form, and this pins the
    /// refusal that told us so.
    #[test]
    fn a_graph_parallel_node_is_refused_by_the_durable_models() {
        let yaml = r#"
name: graph-fanout
trigger:
  type: schedule
  cron: "0 4 * * 0"
graph:
  nodes:
    scout-fanout:
      action: parallel
      over: "3"
      each:
        action: dispatch_job
        assignee: x
        spec:
          task: "scout"
    oracle:
      action: dispatch_job
      assignee: x
      spec:
        task: "{{steps.scout-fanout}}"
      needs: [scout-fanout]
"#;
        let wf = toron_workflow::schema::parse_workflow(yaml).expect("parses");
        let reason = toron_workflow::engine::durable::unsupported_reason(&wf)
            .expect("a graph fan-out cannot open on the durable models");
        assert!(
            reason.contains("parallel join with no copies"),
            "the refusal names the fan-out: {reason}"
        );
    }

    /// bd-gtw4i: EVERY published workflow must either open on the durable
    /// models or be refused for a feature those models do not have
    /// (`branch`/`loop` graph nodes, which route to the legacy runner by
    /// design). A shape the durable model CAN express — a fan-out — must never
    /// be refused: that is how weekly-review, debate and interrogate shipped
    /// workflows their own runner could not open.
    #[test]
    fn every_published_workflow_is_durable_or_legacy_by_design() {
        let mut durable = 0;
        let mut legacy = Vec::new();
        for (name, wf) in builtin_workflows() {
            match toron_workflow::engine::durable::unsupported_reason(&wf) {
                None => durable += 1,
                Some(reason) => {
                    let engine_gap = reason.contains("uses 'branch")
                        || reason.contains("uses 'loop")
                        || reason.contains("uses 'foreach");
                    assert!(
                        engine_gap,
                        "{name} is refused by the durable models for a shape they can express: {reason}"
                    );
                    legacy.push(name);
                }
            }
        }
        assert!(
            durable >= 7,
            "expected the durable catalog, found {durable}"
        );
        assert!(
            legacy.len() <= 3,
            "the legacy-by-design set should not grow silently: {legacy:?}"
        );
    }

    /// bd-gtw4i: a cron can never ship a workflow its runner REFUSES. Every
    /// schedule-triggered catalog workflow must open on the durable models —
    /// the check the weekly-review failure slipped past for a whole week —
    /// including the TICK workflow the cron actually fires.
    /// bd-m5afp: the full mutation proof runs nightly through flywheel cron, so
    /// a witness is never only as fresh as the last landing that touched its
    /// file. Host scope: one run proves the repo, not one project's copy.
    #[test]
    fn mutation_full_is_a_nightly_host_cron() {
        use toron_workflow::schema::{Action, CronScope, Trigger};
        let wfs = builtin_workflows();
        let mf = &wfs["mutation-full"];
        match &mf.trigger {
            Some(Trigger::Schedule { cron, owner, scope }) => {
                assert_eq!(cron, "30 3 * * *", "nightly");
                assert_eq!(owner.as_deref(), Some("housekeeper"));
                assert_eq!(*scope, CronScope::Host);
            }
            other => panic!("mutation-full must be schedule-triggered: {other:?}"),
        }
        let [step] = mf.steps.as_slice() else {
            panic!("one step: the proof is one command, not a pipeline");
        };
        let Action::Verify { command } = &step.action else {
            panic!("the proof is a verify step");
        };
        assert!(
            command.contains("clean-proof.sh") && command.contains("--mutations full"),
            "the nightly job runs the FULL mutation set: {command}"
        );
        assert!(
            crate::cron::builtin_crons().contains_key("mutation-full"),
            "cron list shows it"
        );
    }

    /// bd-oj9o #2: the cold-start + deep-triage workflows exist, and the
    /// plan-review workflow (slash target) exists.
    /// ADR-0019 Spec 7.2 (amended): prototype + arena are builtins; the
    /// inputs carry the structural contracts (n default 2 = the min, mode
    /// default sketch, selection required on arena).
    /// ADR-0019 Spec 7.2: prototype arms branch on mode — compile = SEQUENTIAL
    /// foreach (build_slot: true, scratch ref, measure+reset) and sketch =
    /// PARALLEL fan-out; the @oracle judge needs the arms node.
    /// ADR-0019 Spec 7.2: arena carries the same shape PLUS the declared-up-front
    /// selection rule (first-pass | rank-all | best-of) in the judge PLAN
    /// (the judge discipline is the shared judge-arms stage — bd-h9xn).
    /// ADR-0019 Spec 7.2: neither prototype nor arena carries any
    /// worktree/br/.beads surface — the dispatch text bans them per arm and
    /// the workflows declare no worktree action (compile-enforced).
    /// bd-oj9o #1: brainstorm/debate actually fan out — the dead `breadth`/`panel`
    /// inputs were replaced by real parallelism, and parallelism is the point.
    ///
    /// bd-0s0y: the two now fan out DIFFERENTLY and both are correct.
    /// `debate` keeps a literal `parallel` over 2 reviewers. `ce-brainstorm`
    /// spreads 3 divergers across 3 distinct ROLES, because a `parallel` over
    /// one role with a one-pane-per-role crew is a guaranteed busy-fence
    /// refusal. What must hold for both is the property, not the mechanism:
    /// more than one agent works the question at the same time.
    /// bd-oj9o #1: the beads workflow carries the plan into the dispatch
    /// spec ({{inputs.plan}} → rendered into the headless prompt).
    /// bd-oj9o #1: the reactive triage workflow has no hardcoded agent
    /// names (pi-alpha/oc-beta are spawn-dependent and break any real
    /// swarm) — ack goes to the operator, dispatch is profile-based.
    /// bd-h4fc: the reducer/decider steps consume the fan-out outputs
    /// ({{steps.diverge}} / {{steps.briefs}}) — without the reference the
    /// planner/oracle decide blind (the ADR-0005 Spec 2b `inputs.ideas` /
    /// `inputs.arguments` parity under the dispatch_job shape).
    /// bd-h4fc: no dispatch spec uses the dead `prompt` key — headless
    /// dispatch reads task/stage/base only, so `spec.prompt` would silently
    /// drop the mission (the brainstorm/diverge + debate/briefs regression).
    #[test]
    fn no_dispatch_spec_uses_dead_prompt_key() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        fn scan_action(action: &Action, wf: &str, dead: &mut Vec<String>) {
            match action {
                Action::DispatchJob { spec, .. } if spec.extra.contains_key("prompt") => {
                    dead.push(wf.to_string());
                }
                Action::Parallel { each, .. } => scan_action(&each.action, wf, dead),
                Action::Retry { step, .. } => scan_action(&step.action, wf, dead),
                Action::Branch {
                    then, else_steps, ..
                } => {
                    for s in then.iter().chain(else_steps.iter()) {
                        scan_action(&s.action, wf, dead);
                    }
                }
                Action::Foreach { each, .. } => scan_action(&each.action, wf, dead),
                Action::Loop { body, .. } => {
                    for st in body.iter() {
                        scan_action(&st.action, wf, dead);
                    }
                }
                _ => {}
            }
        }
        let mut dead = Vec::new();
        for (name, wf) in &wfs {
            for step in &wf.steps {
                scan_action(&step.action, name, &mut dead);
            }
            if let Some(g) = &wf.graph {
                for node in g.nodes.values() {
                    scan_action(&node.action, name, &mut dead);
                }
            }
        }
        assert!(
            dead.is_empty(),
            "dispatch specs with dead prompt key: {dead:?}"
        );
    }

    /// bd-h4fc: the goal meta-chain carries context forward — build gets the
    /// tracked plan ({{steps.track}}), review gets the build output
    /// ({{steps.build}}) with the opencode harness, and ship is a real
    /// commit-and-push dispatch (NOT a triage call — triage templates
    /// trigger.subject/id which do not exist on the invoke path).
    /// bd-adv4: compose tokens route the implement profile through the
    /// `coder_profile` input (default @coder) so the profile-pick
    /// suggestion applies without editing YAML; beads-lifecycle forwards
    /// it to wave-swarm.
    /// No step reads a completion marker: awaits are `dispatch_job`
    /// `await: result` now, and `.done` files are write-only compat
    /// sidecars. Any `test -f` read is a D4.4 regression.
    /// bd-jhz8x.2 (D4.5): DISPATCH is an engine step, so the gate's grounding
    /// read must still reach the coder — the artifact's `learnings[]` is the
    /// carrier now that the wave-dispatch prompt is gone.
    /// ADR-0021 D4.5 (bd-jhz8x.2): triage-deep's crew comes from the workflow's
    /// own `crew:` field — the pane door's `pre_spawn_crew` brings the seats up
    /// before the run — not from a housekeeper seat that shells out to
    /// `flywheel spawn`. The `spawn-crew` stage is retired, and no node
    /// dispatches it.
    ///
    /// The `spawn` ACTION is not the equivalent here: nothing on the CLI door
    /// drains `spawn_sink`, so a detached spawn would announce a key nobody
    /// consumes and the crew would silently not exist (a false green). The
    /// crew field is the engine-visible declaration the door already honours.
    /// Every stage dispatch awaits its job result with a bounded wait:
    /// `await: result`, an explicit claim bound, and the 7200s result bound.
    /// D4.3: the post-execute step is a ledger fact check, not a witness —
    /// every wave bead closed plus a legal ship check each. The command
    /// itself never runs in tests; its shape is the contract.
    /// The exit command runs against a ledger: deny-shims plus fake `br`
    /// and `flywheel` scripts first on PATH, no HOME, no real services.
    /// The br fake prints fixture JSON and honors --all versus the default
    /// exclude-closed; the flywheel fake passes or fails `ship check` per
    /// id. The rendered command runs via sh -c.
    #[allow(dead_code)]
    fn wave_swarm_execute_exit_runs_against_ledger_notest() {
        use std::os::unix::fs::PermissionsExt;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("fw-wave-exit-{nanos}-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let exe = |name: &str, body: &str| {
            let path = bin.join(name);
            std::fs::write(&path, body).expect("fake script");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        };
        exe(
            "launchctl",
            "#!/bin/sh\necho \"deny-shim: called $0\" >&2\nexit 1\n",
        );
        exe(
            "systemctl",
            "#!/bin/sh\necho \"deny-shim: called $0\" >&2\nexit 1\n",
        );
        exe(
            "br",
            "#!/bin/sh\nif [ \"$1\" = \"list\" ]; then\n  all=0\n  for a in \"$@\"; do [ \"$a\" = \"--all\" ] && all=1; done\n  jq --argjson all \"$all\" '[.issues[] | select($all == 1 or .status != \"closed\") | del(.wave)] | {issues: .}' \"$BR_FIXTURE\"\n  exit $?\nfi\nif [ \"$1\" = \"show\" ]; then\n  shift\n  ids=\"\"\n  for a in \"$@\"; do case \"$a\" in --*) ;; *) ids=\"$ids,\\\"$a\\\"\";; esac; done\n  jq --argjson ids \"[${ids#,}]\" '[(if type == \"object\" then .issues else . end)[] | select(.id as $id | $ids | index($id))]' \"$BR_FIXTURE_ROWS\"\n  exit $?\nfi\necho \"fake br: unknown op $1\" >&2\nexit 2\n",
        );
        exe(
            "flywheel",
            "#!/bin/sh\nid=\"${3:-}\"\ncase \" $SHIP_FAIL_IDS \" in\n  *\" $id \"*) echo \"ship check failed for $id\" >&2; exit 1;;\nesac\necho \"ship check ok for $id\"\nexit 0\n",
        );
        let fixture = dir.join("fixture.json");
        let run = |fixture_json: &str, ship_fail: &str| -> std::process::Output {
            std::fs::write(&fixture, fixture_json).expect("fixture");
            std::fs::write(fixture.with_extension("rows.json"), fixture_json)
                .expect("rows fixture");
            let path = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let wfs = builtin_workflows();
            let wave = &wfs["wave-swarm"];
            let exit = wave
                .steps
                .iter()
                .find(|s| s.name.as_deref() == Some("execute_exit"))
                .expect("execute_exit");
            let command = match &exit.action {
                toron_workflow::schema::Action::Verify { command } => {
                    command.replace("{{inputs.wave_label}}", "wave-2")
                }
                other => panic!("execute_exit is a verify step: {:?}", other.type_name()),
            };
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .env("PATH", &path)
                .env("BR_FIXTURE", &fixture)
                .env("BR_FIXTURE_ROWS", fixture.with_extension("rows.json"))
                .env("SHIP_FAIL_IDS", ship_fail)
                .env_remove("HOME")
                .output()
                .expect("spawn sh");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !text.contains("deny-shim"),
                "no supervisor shims fire: {text}"
            );
            out
        };
        let text_of = |out: &std::process::Output| -> String {
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        };
        // All closed (one by label, one by typed wave only), ship legal.
        let out = run(
            r#"{"issues": [
              {"id": "bd-a", "status": "closed", "labels": ["wave-2"]},
              {"id": "bd-t", "status": "closed", "labels": [], "wave": 2}
            ]}"#,
            "",
        );
        assert!(out.status.success(), "all closed + legal ship passes");
        // One open bead fails naming it.
        let out = run(
            r#"{"issues": [
              {"id": "bd-a", "status": "closed", "labels": ["wave-2"]},
              {"id": "bd-b", "status": "open", "labels": ["wave-2"]}
            ]}"#,
            "",
        );
        assert!(!out.status.success(), "an open bead fails the exit");
        assert!(
            text_of(&out).contains("bd-b"),
            "the open bead is named: {}",
            text_of(&out)
        );
        // Zero members fails.
        let out = run(
            r#"{"issues": [{"id": "bd-x", "status": "closed", "labels": ["wave-9"]}]}"#,
            "",
        );
        assert!(!out.status.success(), "zero members fails");
        assert!(
            text_of(&out).contains("matched zero beads"),
            "empty wave says so: {}",
            text_of(&out)
        );
        // A typed-wave member with no label counts.
        let out = run(
            r#"{"issues": [{"id": "bd-t", "status": "closed", "labels": [], "wave": 2}]}"#,
            "",
        );
        assert!(
            out.status.success(),
            "typed-wave membership needs no label: {}",
            text_of(&out)
        );
        // An illegal ship check fails the exit.
        let out = run(
            r#"{"issues": [
              {"id": "bd-a", "status": "closed", "labels": ["wave-2"]},
              {"id": "bd-x", "status": "closed", "labels": ["wave-2"]}
            ]}"#,
            "bd-x",
        );
        assert!(!out.status.success(), "illegal ship check fails");
        assert!(
            text_of(&out).contains("bd-x"),
            "the illegal bead is named: {}",
            text_of(&out)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

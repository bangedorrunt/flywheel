//! Compiled-in built-in workflows (ADR-0005 Spec 2b / P3.5).
//!
//! Zero-config: `flywheel workflow run debate --inputs '{proposal:"..."}'`
//! works on a fresh install. Each built-in carries the EXACT YAML from ADR
//! Spec 2b — purpose-named, composing profiles (Spec 1b) + stages (Spec 2a)
//! + control-flow (Spec 4). The catalog loads these as the compiled-in
//!   defaults, overridable by `~/.config/flywheel/workflows/*.yaml`.
//!
//! - `brainstorm` — graph: parallel dispatch @oracle (over 3) → @planner
//!   reduce (task consumes {{steps.diverge}}; fail_run on reduce).
//! - `plan` — dispatch @planner (task carries {{inputs.goal}}; fail_run on
//!   decompose) → request_approval gate.
//! - `beads` — dispatch @planner stage:systematic-execute (spec.plan carries
//!   the plan; fail_run on create).
//! - `debate` — graph: parallel dispatch @reviewer (over 2) → @oracle decide
//!   (task consumes {{steps.briefs}}; fail_run on decide).
//! - `goal` — meta: call_workflow brainstorm→plan→beads→dispatch @coder
//!   stage:systematic-execute→@reviewer stage:code-review→@housekeeper
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
//! - `figure-it-out` — no bundled playbook (bd-w2-review-s6ij): the planner
//!   authors workflow YAML (lint is the gate); the captain executes via
//!   `workflow compose --agent` then `--execute`.
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

/// The compiled-in built-in workflows, name → YAML text.
pub const BUILTIN_WORKFLOWS_YAML: &[(&str, &str)] = &[
    (
        "brainstorm",
        r##"
name: brainstorm
description: Divergent ideation — fan out oracles, a planner reduces to ranked options.
trigger:
  type: webhook
crew: auto
inputs:
  - name: question
graph:
  nodes:
    diverge:
      action: parallel
      over: "3"
      each: { action: dispatch_job, assignee: x, spec: { profile: "@oracle", task: "Diverge on the question — surface ideas, angles, and options: {{inputs.question}}" } }
    reduce:
      action: dispatch_job
      assignee: x
      spec: { profile: "@planner", task: "Reduce these oracle outputs into ranked options for: {{inputs.question}}\n\nOracle outputs: {{steps.diverge}}" }
      needs: [diverge]
      on_error: fail_run
"##,
    ),
    (
        "plan",
        r##"
name: plan
description: Goal → bead-ready plan via the planner, with an optional approval gate.
trigger:
  type: webhook
crew: auto
inputs:
  - name: goal
steps:
  - name: decompose
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", task: "Decompose this goal into a bead-ready plan (tasks, subtasks, dependency structure): {{inputs.goal}}" }
    on_error: fail_run
  - name: gate
    action: request_approval
    message: "Approve plan for {{inputs.goal}}?"
"##,
    ),
    (
        "beads",
        r##"
name: beads
description: Plan → tracker. The planner runs the systematic-execute stage.
trigger:
  type: webhook
crew: auto
inputs:
  - name: plan
steps:
  - name: create
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "systematic-execute", plan: "{{inputs.plan}}" }
    on_error: fail_run
"##,
    ),
    (
        "debate",
        r##"
name: debate
description: Adversarial synthesis — reviewers brief, an oracle decides.
trigger:
  type: webhook
crew: auto
inputs:
  - name: proposal
graph:
  nodes:
    briefs:
      action: parallel
      over: "2"
      each: { action: dispatch_job, assignee: x, spec: { profile: "@reviewer", harness: "opencode", task: "Brief on the proposal from your adversarial angle: {{inputs.proposal}}" } }
    decide:
      action: dispatch_job
      assignee: x
      spec: { profile: "@oracle", task: "Adjudicate the briefs and decide on: {{inputs.proposal}}\n\nReviewer briefs: {{steps.briefs}}" }
      needs: [briefs]
      on_error: fail_run
"##,
    ),
    (
        "goal",
        r##"
name: goal
description: The meta-workflow — brainstorm → plan → beads → build → review → ship.
trigger:
  type: webhook
crew: auto
inputs:
  - name: objective
steps:
  - name: ideate
    action: call_workflow
    workflow: brainstorm
    inputs: { question: "{{inputs.objective}}" }
  - name: plan
    action: call_workflow
    workflow: plan
    inputs: { goal: "{{inputs.objective}}" }
  - name: track
    action: call_workflow
    workflow: beads
    inputs: { plan: "{{steps.plan}}" }
  - name: build
    action: dispatch_job
    assignee: x
    spec: { profile: "@coder", stage: "systematic-execute", plan: "{{steps.track}}" }
    on_error: fail_run
  - name: review
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer", harness: "opencode", stage: "code-review", plan: "{{steps.build}}" }
    on_error: fail_run
  - name: ship
    action: dispatch_job
    assignee: x
    spec: { profile: "@housekeeper", stage: "commit-and-push", plan: "{{steps.build}}" }
    on_error: fail_run
"##,
    ),
    (
        "goal-direct",
        r##"
name: goal-direct
description: "Captain-free DIRECT arm of goal (bd-t86p) — brainstorm → plan → build → review → ship with NO bead tracking and NO plan approval gate: non-bead targets flow straight to the coder. `flywheel steer \"<target>\" --direct` runs this."
trigger:
  type: webhook
crew: auto
inputs:
  - name: objective
steps:
  - name: ideate
    action: call_workflow
    workflow: brainstorm
    inputs: { question: "{{inputs.objective}}" }
  - name: plan
    action: call_workflow
    workflow: plan-direct
    inputs: { goal: "{{inputs.objective}}" }
  - name: build
    action: dispatch_job
    assignee: x
    spec: { profile: "@coder", stage: "systematic-execute", plan: "{{steps.plan}}" }
    on_error: fail_run
  - name: review
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer", stage: "code-review", plan: "{{steps.build}}" }
    on_error: fail_run
  - name: ship
    action: dispatch_job
    assignee: x
    spec: { profile: "@housekeeper", stage: "commit-and-push", plan: "{{steps.build}}" }
    on_error: fail_run
"##,
    ),
    (
        "plan-direct",
        r##"
name: plan-direct
description: "Goal → execution plan WITHOUT the approval gate (bd-t86p): the captain-free companion to `plan`. The plan gate is the one interactive checkpoint; a steer --direct run must not park on it — the captain already stated the target."
trigger:
  type: webhook
crew: auto
inputs:
  - name: goal
steps:
  - name: decompose
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", task: "Decompose this goal into an execution plan (tasks, subtasks, dependency structure) — direct mode, no tracker, no approval: {{inputs.goal}}" }
    on_error: fail_run
"##,
    ),
    (
        "triage",
        r##"
name: triage
description: Triage inbound messages — the canonical reactive workflow.
trigger:
  type: message_posted
  channel: "#support"
guard: "trigger.channel == '#support'"
steps:
  - name: ack
    action: send_dm
    to: "operator"
    body: "Received {{trigger.content}}"
  - name: dispatch
    action: dispatch_job
    assignee: "x"
    spec: { profile: "@housekeeper", task: "triage {{trigger.id}}" }
"##,
    ),
    (
        "plan-review",
        r##"
name: plan-review
description: Review a plan bead-by-bead before implementation — the p5 stage.
trigger:
  type: webhook
crew: auto
inputs:
  - name: plan
steps:
  - name: review
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer", harness: "opencode", stage: "plan-review", plan: "{{inputs.plan}}" }
    on_error: fail_run
"##,
    ),
    (
        "onboard",
        r##"
name: onboard
description: Cold-start a newcomer — map the repo, seed context, propose the first task.
trigger:
  type: webhook
crew: auto
inputs:
  - name: project
steps:
  - name: map
    action: dispatch_job
    assignee: x
    spec: { profile: "@scout", stage: "repo-map", project: "{{inputs.project}}" }
    on_error: fail_run
  - name: seed
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "seed-first-task", plan: "{{steps.map}}", project: "{{inputs.project}}" }
    on_error: fail_run
"##,
    ),
    (
        "triage-deep",
        r##"
name: triage-deep
description: Deep backlog triage — bv robot-triage, prioritize + assign, spawn the crew.
trigger:
  type: webhook
crew: auto
inputs:
  - name: project
steps:
  - name: triage
    action: dispatch_job
    assignee: x
    spec: { profile: "@scout", stage: "bv-triage", project: "{{inputs.project}}" }
    on_error: fail_run
  - name: prioritize
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "prioritize-assign", plan: "{{steps.triage}}", project: "{{inputs.project}}" }
    on_error: fail_run
  - name: spawn
    action: dispatch_job
    assignee: x
    spec: { profile: "@housekeeper", stage: "spawn-crew", plan: "{{steps.prioritize}}", project: "{{inputs.project}}" }
    on_error: fail_run
"##,
    ),
    (
        "captain-audit",
        r##"
name: captain-audit
description: "Captain audit (bd-wsui) — read-only analysis of a topic within a scope: scout gathers evidence, oracle synthesizes, reviewer challenges. ANALYSIS ONLY: no beads, no repo edits; the final report is written to report_path."
trigger:
  type: webhook
crew: auto
inputs:
  - name: topic
  - name: scope
  - name: report_path
graph:
  nodes:
    gather:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@scout"
        task: "ANALYSIS ONLY — no beads, no repo edits. Audit topic: {{inputs.topic}}. Scope: {{inputs.scope}}. Gather in-scope evidence and findings. Return COMPRESSED findings JSON: [{area, finding, evidence, ev}]."
    synthesize:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@oracle"
        task: "ANALYSIS ONLY — no beads, no repo edits. Audit topic: {{inputs.topic}}. Synthesize the scout findings into the audit report: {{steps.gather}}. Write the full report markdown to: {{inputs.report_path}}."
      needs: [gather]
      on_error: fail_run
    challenge:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@reviewer"
        harness: "opencode"
        task: "ANALYSIS ONLY — no beads, no repo edits. Challenge the synthesized report: {{steps.synthesize}}. Audit topic: {{inputs.topic}}. Check evidence, gaps, and overclaiming; append your adversarial verdict section to the report at: {{inputs.report_path}}."
      needs: [synthesize]
      on_error: fail_run
"##,
    ),
    (
        "weekly-review",
        r##"
name: weekly-review
description: "Weekly repo review — scout fan-out, oracle synthesis, exactly ONE follow-up bead (bd-m7ac)."
trigger:
  type: schedule
  cron: "0 4 * * 0"
  owner: oracle
  scope: project
policy:
  on_failure: bead
  on_repeat_failure: escalate
  repeat_threshold: 3
  on_success: log
  max_retries: 1
  no_claim_timeout: 600
inputs:
  - name: goal
graph:
  nodes:
    scout-fanout:
      action: parallel
      over: "3"
      each:
        action: dispatch_job
        assignee: x
        spec:
          profile: "@scout"
          task: "Scout the toron + flywheel repos: (1) internal gaps — code health, deferred ADR-0005/0010/0011 items still open, open seams, test gaps; (2) upstream parity — Dicklesworthstone/mcp_agent_mail_rust and block/buzz patterns not yet ported. Return COMPRESSED findings JSON: [{area, finding, evidence, ev}]. No academic literature."
    oracle:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@oracle"
        task: "Synthesize the scout findings: {{steps.scout-fanout}}. Rank by EV + project-fit. Pick the SINGLE best improvement idea (feature / design pattern / bug fix). Output JSON: {\"idea\": {\"title\": ..., \"what\": ..., \"why\": ..., \"ev\": ..., \"blast_radius\": ..., \"acceptance\": ...}} — or {\"idea\": null} if nothing worth proposing (honest, not forced)."
      needs: [scout-fanout]
"##,
    ),
    // ---- ADR-0019 Spec 7.2 fix-path playbooks (bd-w2-fix-path-7gd5) ----
    (
        "bug-fix",
        r##"
name: bug-fix
description: "Fix-path playbook (ADR-0019 Spec 7.2): repro → failing test → minimal fix → independent proof. No fix node without a repro predecessor (lint)."
trigger:
  type: webhook
crew: auto
inputs:
  - name: symptom
  - name: repro
principles:
  - fix-root-causes
  - prove-it-works
  - sequence-verifiable-units
graph:
  nodes:
    repro:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@scout"
        task: "Run or write the repro for: {{inputs.symptom}}\n\nRepro: {{inputs.repro}}\nACCEPTANCE: the command exits non-zero (or visibly shows the bug). Record the exact failing output as the baseline."
      on_error: fail_run
    tdd:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@coder"
        task: "Write the FAILING test first (RED) for: {{inputs.symptom}}\n\nRepro: {{inputs.repro}}\nVERIFY: the new test FAILS. Do not fix anything yet."
      needs: [repro]
      on_error: fail_run
    fix:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@coder"
        task: "Apply the MINIMAL fix for {{inputs.symptom}} against the failing test from step tdd.\nVERIFY: the test PASSES AND the original repro ({{inputs.repro}}) no longer reproduces."
      needs: [tdd]
      on_error: fail_run
    prove:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@reviewer"
        harness: "opencode"
        task: "Independently prove the fix for {{inputs.symptom}} (different model family, ADR-0019 Spec 4). VERIFY from the bead: test green + repro gone. Record unit-test-verified. Do not re-fix."
      needs: [fix]
      on_error: fail_run
"##,
    ),
    (
        "feature",
        r##"
name: feature
description: "Feature playbook (ADR-0019 Spec 7.2): shape the domain type/state machine → TDD build → independent proof. data_shape is REQUIRED."
trigger:
  type: webhook
crew: auto
inputs:
  - name: objective
  - name: data_shape
principles:
  - foundational-thinking
  - model-the-domain
  - prove-it-works
steps:
  - name: shape
    action: dispatch_job
    assignee: x
    spec:
      profile: "@planner"
      task: "Name the type / state machine for: {{inputs.objective}}\n\ndata_shape: {{inputs.data_shape}}\nREFUSE (return an error) when data_shape is empty — the feature cannot be shaped without naming its data."
    on_error: fail_run
  - name: build
    action: dispatch_job
    assignee: x
    spec:
      profile: "@coder"
      task: "Implement {{inputs.objective}} against the shape from step shape: {{steps.shape}}\nTDD: failing test first, then minimal code to pass."
    on_error: fail_run
  - name: prove
    action: dispatch_job
    assignee: x
    spec:
      profile: "@reviewer"
      harness: "opencode"
      task: "Prove {{inputs.objective}} independently (different model family, ADR-0019 Spec 4). VERIFY from the bead. Record unit-test-verified. Do not re-implement."
    on_error: fail_run
"##,
    ),
    (
        "refactoring",
        r##"
name: refactoring
description: "Refactoring playbook (ADR-0019 Spec 7.2): baseline invariant → delete first → behavior-preserving reshape → invariant still green, no *V2* files."
trigger:
  type: webhook
crew: auto
inputs:
  - name: target
  - name: invariant_cmd
principles:
  - laziness-protocol
  - subtract-before-you-add
  - prove-it-works
steps:
  - name: baseline
    action: dispatch_job
    assignee: x
    spec:
      profile: "@scout"
      task: "Run the behavior invariant for the refactoring of {{inputs.target}}: `{{inputs.invariant_cmd}}`. Store the stdout receipt — this is the baseline that must stay green."
    on_error: fail_run
  - name: subtract
    action: dispatch_job
    assignee: x
    spec:
      profile: "@coder"
      task: "Deletion inventory for {{inputs.target}}: list every line/module that can be removed, then APPLY the deletions. Do not add code yet (laziness protocol: subtract before you add)."
    on_error: fail_run
  - name: reshape
    action: dispatch_job
    assignee: x
    spec:
      profile: "@coder"
      task: "Behavior-preserving reshape of {{inputs.target}}: no behavior change, no *V2*/*_improved*/*_enhanced* files. Invariant must stay green: `{{inputs.invariant_cmd}}`."
    on_error: fail_run
  - name: prove
    action: dispatch_job
    assignee: x
    spec:
      profile: "@reviewer"
      harness: "opencode"
      task: "Prove the refactoring of {{inputs.target}}: invariant `{{inputs.invariant_cmd}}` still green AND no *V2*/*_improved*/*_enhanced* files added. Record unit-test-verified."
    on_error: fail_run
"##,
    ),
    // ---- ADR-0019 Spec 7.2 measured-improvement playbooks (bd-w2-perf-to27) ----
    (
        "perf",
        r##"
name: perf
description: "Measured-improvement playbook (ADR-0019 Spec 7.2): baseline → ONE change → same-command measure → KEEP (commit) or DISCARD (a revert, never a leftover) via an explicit control-flow branch."
trigger:
  type: webhook
crew: auto
inputs:
  - name: metric
  - name: baseline_cmd
  - name: budget
principles:
  - prove-it-works
  - sequence-verifiable-units
  - laziness-protocol
steps:
  - name: baseline
    action: dispatch_job
    assignee: x
    spec:
      profile: "@scout"
      task: "Run the baseline for {{inputs.metric}}: `{{inputs.baseline_cmd}}`. Record the NUMBER (the baseline). The budget is {{inputs.budget}}."
    on_error: fail_run
  - name: change
    action: dispatch_job
    assignee: x
    spec:
      profile: "@coder"
      task: "Make ONE change aimed at improving {{inputs.metric}} (baseline: {{steps.baseline}}). No scope creep — one change only."
    on_error: fail_run
  - name: measure
    action: dispatch_job
    assignee: x
    spec:
      profile: "@scout"
      task: "Measure {{inputs.metric}} with the SAME command `{{inputs.baseline_cmd}}` against the change. Return JSON: {\"improved\": <bool>, \"number\": <N>} — improved iff the metric is better AND the budget {{inputs.budget}} is held."
    on_error: fail_run
  - name: keep-or-discard
    action: branch
    if: "{{steps.measure.improved}} == true"
    then:
      - name: keep
        action: dispatch_job
        assignee: x
        spec:
          profile: "@scout"
          task: "KEEP the change: {{inputs.metric}} improved to {{steps.measure.number}} within budget. Commit the win — the improved state becomes the new baseline."
        on_error: fail_run
    else:
      - name: discard
        action: dispatch_job
        assignee: x
        spec:
          profile: "@scout"
          task: "DISCARD the change: {{inputs.metric}} did not improve (or the budget was missed). DISCARD IS A REVERT — restore the baseline exactly, no leftover."
        on_error: fail_run
"##,
    ),
    (
        "hillclimb",
        r##"
name: hillclimb
description: "Sustained measured-improvement loop (ADR-0019 Spec 7.2): capped retry — each attempt hypothesizes -> applies the smallest change -> measures -> KEEPS (ledger row + ONE commit per accepted win) or DISCARDS (a revert) and retries with a new hypothesis. The retry max_attempts is the iteration cap (max_iters)."
trigger:
  type: webhook
crew: auto
inputs:
  - name: metric
  - name: target
  - name: max_iters
    default: 10
principles:
  - prove-it-works
  - sequence-verifiable-units
  - build-the-lever
steps:
  - name: climb
    action: retry
    max_attempts: 10
    step:
      name: iteration
      action: dispatch_job
      assignee: x
      spec:
        profile: "@coder"
        task: "HILLCLIMB ITERATION (budget {{inputs.max_iters}} attempts; target {{inputs.target}} for {{inputs.metric}}): hypothesize -> apply the SMALLEST change -> measure with the same command. If improved: KEEP — record the ledger row (flywheel ledger / br gate, sha of this iteration's commit) + ONE commit per accepted win, return ok. If NOT improved: DISCARD — discard IS a revert, restore the baseline exactly, then return an ERROR so the loop retries with a new hypothesis. No unrelated work."
"##,
    ),
    (
        "prototype",
        r##"
name: prototype
description: "Exhaust-the-design-space (ADR-0019 Spec 7.2, as amended 2026-08-15 — no /tmp crate-copy): n arms (2-3) prototype the question, an @oracle judge picks ONE. Sketch mode (default): arms run PARALLEL and write NON-COMPILING design sketches — API shapes, state machines, tradeoff table — TEXT ONLY to /tmp/flywheel-proto-<run>-<arm>/; no crate copy, no br, no .beads/ writes. Compile mode: arms run SEQUENTIALLY (one at a time) in the primary tree on a scratch git ref under the build slot; arm N+1 starts only after arm N is measured and reset. Judge compares on named criteria, picks one, discards the rest; no merge of a bit-of-each unless the oracle writes a graft bead. Principles: exhaust-the-design-space, separate-before-serializing-shared-state."
trigger:
  type: webhook
crew: auto
principles:
  - exhaust-the-design-space
  - separate-before-serializing-shared-state
inputs:
  - name: question
  - name: n
    default: 2
  - name: mode
    default: sketch
  - name: run
  - name: arms
graph:
  nodes:
    arms:
      action: branch
      if: "str_contains(trigger_inputs, \"compile\")"
      then:
        - name: arm
          action: foreach
          build_slot: true
          over: "{{inputs.arms}}"
          each:
            action: dispatch_job
            assignee: x
            build_slot: true
            spec: { profile: "@coder", harness: "opencode", task: "PROTOTYPE COMPILE ARM {{item}} (of {{inputs.arms}}): implement your arm of the question in the PRIMARY TREE on a SCRATCH git ref (branch off current HEAD, never a herdr worktree). Question: {{inputs.question}}. One arm at a time: after implementing, MEASURE (build + test under the build slot — toron slot acquire before any cargo) and RESET the tree (git checkout the base + clean) so arm {{item}} leaves NO residue — the next arm starts only after your reset. No br, no .beads/ writes. Report: what you built, the measurement result, the reset proof." }
      else:
        - name: arm
          action: parallel
          over: "{{inputs.n}}"
          each:
            action: dispatch_job
            assignee: x
            spec: { profile: "@coder", harness: "opencode", task: "PROTOTYPE SKETCH ARM (you are ONE of {{inputs.n}} arms): NON-COMPILING design sketch for the question — API shapes, state machines, a tradeoff table, and the blast radius of each choice. Write TEXT ONLY to /tmp/flywheel-proto-{{inputs.run}}-<YOUR-AGENT-NAME>/ (no crate copy, no compile, no br, no .beads/ writes, no worktree). Question: {{inputs.question}}." }
    judge:
      action: dispatch_job
      assignee: x
      spec: { profile: "@oracle", harness: "opencode", task: "Judge the prototype arms for: {{inputs.question}}. Compare on the named criteria (fit to the question, design quality, feasibility, blast radius). Arms (branch envelope — the collected arm reports live under outputs.arm): {{steps.arms}}. Pick ONE; the others are DISCARDED. No merge of a bit-of-each unless you write a graft bead for the merged design. Report: chosen arm + the comparison table." }
      needs: [arms]
      on_error: fail_run
"##,
    ),
    (
        "arena",
        r##"
name: arena
description: "Model race (ADR-0019 Spec 7.2 arena): same shape as prototype but the arms may run the SAME brief on DIFFERENT models — the race is in the models, not the tree. Selection rule declared UP FRONT via the `selection` input: first-pass (pick the first arm that clears the bar) | rank-all (rank every arm) | best-of (pick the best). Sketch arms: parallel, text-only under /tmp/flywheel-proto-<run>-<arm>/; compile arms: SEQUENTIAL on the build slot, one arm at a time on a scratch ref, measure + reset between arms. The @oracle judge keeps the comparison TABLE, not raw dumps (guard-the-context-window). Principles: exhaust-the-design-space, guard-the-context-window."
trigger:
  type: webhook
crew: auto
principles:
  - exhaust-the-design-space
  - guard-the-context-window
inputs:
  - name: question
  - name: n
    default: 2
  - name: mode
    default: sketch
  - name: selection
  - name: run
  - name: arms
graph:
  nodes:
    arms:
      action: branch
      if: "str_contains(trigger_inputs, \"compile\")"
      then:
        - name: arm
          action: foreach
          build_slot: true
          over: "{{inputs.arms}}"
          each:
            action: dispatch_job
            assignee: x
            build_slot: true
            spec: { profile: "@coder", harness: "opencode", task: "ARENA COMPILE ARM {{item}} (of {{inputs.arms}}): run the SAME brief on YOUR assigned model on the SCRATCH git ref in the PRIMARY TREE (no herdr worktree). Brief: {{inputs.question}}. One arm at a time under the build slot (toron slot acquire before any cargo): after implementing, MEASURE (build + test) and RESET the tree (git checkout base + clean) — arm {{item}} must leave no residue; the next arm starts only after your reset. No br, no .beads/ writes. Report: model used, what you built, measurement, reset proof." }
      else:
        - name: arm
          action: parallel
          over: "{{inputs.n}}"
          each:
            action: dispatch_job
            assignee: x
            spec: { profile: "@coder", harness: "opencode", task: "ARENA SKETCH ARM (you are ONE of {{inputs.n}} arms): NON-COMPILING design sketch for the SAME brief on your assigned model — API shapes, state machines, tradeoff table. Write TEXT ONLY to /tmp/flywheel-proto-{{inputs.run}}-<YOUR-AGENT-NAME>/ (no crate copy, no br, no .beads/ writes, no worktree). Brief: {{inputs.question}}." }
    judge:
      action: dispatch_job
      assignee: x
      spec: { profile: "@oracle", harness: "opencode", task: "Judge the arena arms for: {{inputs.question}}. SELECTION RULE (declared up front): {{inputs.selection}} — first-pass: pick the first arm that clears the bar; rank-all: rank every arm; best-of: pick the best. Arms (branch envelope — collected arm reports under outputs.arm): {{steps.arms}}. Output the comparison TABLE (model per arm, verdict per criterion) — the table, not raw dumps. Pick ONE winner; others discarded. No merge of a bit-of-each unless you write a graft bead." }
      needs: [arms]
      on_error: fail_run
"##,
    ),
    (
        "interrogate",
        r##"
name: interrogate
description: "Adversarial N-model review (extends debate — 2-brief synthesis vs N-model rubric; both live): N reviewers on DIFFERENT model families, same rubric, READONLY; the parent synthesizes Act-on / Consider / Noted / Dismissed. Does not apply fixes — a follow-up bug-fix or refactoring consumes Act-on."
trigger:
  type: webhook
crew: auto
inputs:
  - name: target
  - name: reviewers
    default: "2"
principles:
  - prove-it-works
  - fix-root-causes
  - guard-the-context-window
graph:
  nodes:
    briefs:
      action: parallel
      over: "{{inputs.reviewers}}"
      each:
        action: dispatch_job
        assignee: x
        spec:
          profile: "@reviewer"
          harness: "opencode"
          task: "READONLY adversarial review — no edits, no repo writes, no beads. Same rubric for every reviewer (DIFFERENT model families per reviewer). Target: {{inputs.target}}. Review the target against the rubric; per finding give ONE classification: Act-on | Consider | Noted | Dismissed, with evidence. Rubric: prove-it-works (check the real artifact, not a proxy), fix-root-causes (name the root cause), guard-the-context-window (compressed evidence, no dumps)."
    synthesize:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@oracle"
        task: "Synthesize the N reviewer briefs for: {{inputs.target}}. Group every finding into Act-on / Consider / Noted / Dismissed with the supporting evidence. Do not apply changes — the parent routes Act-on into a follow-up playbook. Reviewer briefs: {{steps.briefs}}"
      needs: [briefs]
      on_error: fail_run
"##,
    ),
    (
        "forensics",
        r##"
name: forensics
description: "Diagnosis-only playbook (merged from runtime-forensics + trace-forensics): instrument the live surface or read the artifact; the deliverable is the DIAGNOSIS. Follow-up work is a separate playbook after the captain accepts the diagnosis."
trigger:
  type: webhook
crew: auto
inputs:
  - name: symptom
  - name: artifact
principles:
  - fix-root-causes
  - prove-it-works
graph:
  nodes:
    diagnose:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@scout"
        task: "Diagnose the symptom — instrument the live surface or read the artifact; the deliverable is the DIAGNOSIS with the reproducing evidence, root cause, and confidence. Read-only: no mutations to the repo or the target. Symptom: {{inputs.symptom}}. Artifact: {{inputs.artifact}}"
"##,
    ),
    (
        "figure-it-out",
        r##"
name: figure-it-out
description: "No bundled playbook matches — the planner authors workflow YAML (lint is the gate); the captain executes it via workflow compose --agent then --execute."
trigger:
  type: webhook
crew: auto
inputs:
  - name: objective
principles:
  - foundational-thinking
  - encode-lessons-in-structure
steps:
  - name: author
    action: dispatch_job
    assignee: x
    spec:
      profile: "@planner"
      task: "Author a workflow YAML for this objective — name the data shapes, sequence verifiable units, declare principles; lint is the gate. Return the YAML document. Objective: {{inputs.objective}}"
    on_error: fail_run
"##,
    ),
    (
        "babysit-bead",
        r##"
name: babysit-bead
description: "Drive ONE bead to a legal ledger row (ADR-0019 Spec 7.2, pstack babysit without PRs): claim-watch nudge, reservation renew, T0-T3 mail, verifier spawn when required. NEVER starts new beads. Principles: never-block-on-the-human, prove-it-works."
trigger:
  type: webhook
crew: auto
principles:
  - never-block-on-the-human
  - prove-it-works
inputs:
  - name: bead_id
steps:
  - name: drive
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      harness: "opencode"
      task: "BABYSIT ONE BEAD {{inputs.bead_id}} to a legal ledger row (ADR-0019): (1) `br show {{inputs.bead_id}} --json` — status in_progress / blocked / unverified-closed; (2) claim-watch: if the dispatched agent never claimed, nudge it (loop claim-watch); if it is unclaimed and free, claim it; (3) RENEW its file reservations (reason={{inputs.bead_id}}, toron renew_file_reservations) before the TTL lapses; (4) if blocked: drive the T0-T3 escalation mail through the loop machinery — never fabricate a verdict; (5) if closed but unverified: `flywheel ledger check {{inputs.bead_id}}` — missing/illegal verdict → the LOOP reopens it (never force-unlock a peer); if a verifier is required (P0/P1/judgment AC), spawn the reviewer dispatch on a different model family. NEVER start new beads; NEVER write a verdict yourself — only the loop records gate rows. Report: status before/after, what you drove, the legal ledger row."
    on_error: fail_run
"##,
    ),
    (
        "ship-beads",
        r##"
name: ship-beads
description: "Ship beads (ADR-0019 Spec 7.2, pstack shipping without merge-when-ready): predicate = no ready beads AND claim-watch empty AND every newly closed bead legal on the ledger -> @housekeeper commit-and-push (stage p8). Principles: prove-it-works, sequence-verifiable-units."
trigger:
  type: webhook
crew: auto
principles:
  - prove-it-works
  - sequence-verifiable-units
inputs:
  - name: project
steps:
  - name: verify-predicate
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      harness: "opencode"
      task: "SHIP-BEADS PREDICATE for {{inputs.project}} (ADR-0019) — verify ALL THREE before any commit: (1) `br list --status ready --json` in the project repo is EMPTY (no ready beads); (2) claim-watch is EMPTY — no dispatched-but-unclosed beads (`flywheel loop status --project {{inputs.project}}`); (3) every newly closed bead is LEGAL on the ledger — `flywheel ledger check <bead-id>` exits 0 for each close since the last ship (commit BEFORE close; bead id IN the commit message). If ANY predicate fails, STOP and report exactly what blocks — do NOT commit. If all three hold, report PREDICATE-PASS."
    on_error: fail_run
  - name: ship
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      harness: "opencode"
      stage: "commit-and-push"
      task: "Predicate verified ({{steps.verify-predicate}}). Commit all changed files in logically connected groups with detailed messages (bead ids IN the messages), then push. Do not edit code; do not commit obviously ephemeral files. This is pstack shipping without merge-when-ready — the ledger is the proof."
    on_error: fail_run
"##,
    ),
    (
        "autonomous-work",
        r##"
name: autonomous-work
description: "Autonomous overnight work (ADR-0019 Spec 7.2): runs `flywheel loop run --work --on-blocked mail` — the REAL loop, this workflow does NOT reimplement it — then a terminal ship-beads check (predicate default = ship-beads). Documented so decide can point at it. Principles: never-block-on-the-human, make-operations-idempotent."
trigger:
  type: webhook
crew: auto
principles:
  - never-block-on-the-human
  - make-operations-idempotent
inputs:
  - name: project
  - name: predicate
    default: ship-beads
steps:
  - name: run-loop
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      harness: "opencode"
      task: "AUTONOMOUS WORK for {{inputs.project}} (ADR-0019): run the REAL loop — `flywheel loop run --work --on-blocked mail --project {{inputs.project}}` — do NOT reimplement the loop (no hand-rolled polling, no bespoke dispatch, no new beads). Idempotent: the loop resumes its persisted invocation; taking over a live loop requires --force. When the loop settles (predicate reached or stall), run the terminal ship-beads check (predicate: {{inputs.predicate}}, default ship-beads): no ready beads, claim-watch empty, every newly closed bead legal on the ledger (`flywheel ledger check` per close since the last ship). Report: loop outcome, beads processed, ledger state."
    on_error: fail_run
  - name: ship
    action: call_workflow
    workflow: ship-beads
    inputs: { project: "{{inputs.project}}" }
"##,
    ),
    (
        "pickup",
        r##"
name: pickup
description: "Session pickup (ADR-0019 Spec 7.2): read .flywheel/agent-state/, br list --status in_progress, ledger gaps, and the last-sent registry; respawn context-exhausted panes; resume briefs for stranded in_progress beads. Principles: make-operations-idempotent, guard-the-context-window."
trigger:
  type: webhook
crew: auto
principles:
  - make-operations-idempotent
  - guard-the-context-window
inputs:
  - name: project
steps:
  - name: pickup
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      harness: "opencode"
      task: "PICKUP for {{inputs.project}} (ADR-0019): (1) read .flywheel/agent-state/*.json — context_pct >= 80 = EXHAUSTED → respawn those panes; (2) `br list --status in_progress --json` — stranded beads (assignee dead/gone) get a RESUME brief re-dispatched; (3) ledger gaps — beads closed without a legal verdict (`flywheel ledger check <bead-id>` exits non-zero) → flag for the loop reopen path, NEVER force-unlock; (4) last-sent registry — prompts recorded but unconfirmed get re-sent per resume-dedup. Idempotent: re-running is safe — a done pickup finds nothing to do. Keep SUMMARIES in your report, not raw dumps (guard-the-context-window)."
    on_error: fail_run
"##,
    ),
    (
        "pause",
        r##"
name: pause
description: "Pause safely (ADR-0019 Spec 7.2): `flywheel checkpoint save` + standing handoff (what is in flight, ledger gaps, the EXACT resume command). Kills nothing that is working unless --force-working is explicit. Principles: make-operations-idempotent, never-block-on-the-human."
trigger:
  type: webhook
crew: auto
principles:
  - make-operations-idempotent
  - never-block-on-the-human
inputs:
  - name: project
  - name: msg
  - name: force_working
    default: false
steps:
  - name: pause
    action: dispatch_job
    assignee: x
    spec:
      profile: "@housekeeper"
      harness: "opencode"
      task: "PAUSE {{inputs.project}} (ADR-0019): (1) `flywheel checkpoint save --project {{inputs.project}}` — durable state snapshot; (2) write the STANDING HANDOFF: what is in flight (in_progress beads + held reservations), ledger gaps (closed-but-unverified beads), and the EXACT resume command (e.g. `flywheel loop run --work --on-blocked mail --project {{inputs.project}}` or the pickup workflow); (3) kill NOTHING that is working unless force_working={{inputs.force_working}} was explicit — a working agent keeps running through the pause. Operator message: {{inputs.msg}}. Report: checkpoint id, handoff path, what stayed alive."
    on_error: fail_run
"##,
    ),
];

/// Load the compiled-in built-in workflows as parsed `Workflow`s.
pub fn builtin_workflows() -> BTreeMap<String, Workflow> {
    let mut map = BTreeMap::new();
    for (name, yaml) in BUILTIN_WORKFLOWS_YAML {
        match parse_workflow(yaml) {
            Ok(wf) => {
                map.insert(name.to_string(), wf);
            }
            Err(e) => {
                // A built-in that fails to parse is a crate bug — surface it
                // loudly rather than silently dropping the workflow.
                panic!("built-in workflow {name} failed to parse: {e}");
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew;
    use serde_json::Value;
    use toron_workflow::lint_yaml;
    use toron_workflow::schema::CrewSpec;

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
            "brainstorm",
            "plan",
            "beads",
            "debate",
            "goal",
            "triage",
            "plan-review",
            "onboard",
            "triage-deep",
            "captain-audit",
            "interrogate",
            "forensics",
            "figure-it-out",
        ] {
            assert!(wfs.contains_key(name), "built-in {name} present");
        }
    }

    /// bd-w2-review-s6ij: interrogate defaults N=2 with a @reviewer briefs
    /// fanout (readonly lives in the task text; lint-asserted clean);
    /// forensics has a @scout diagnose node and NO fix-named node
    /// (lint-asserted — a fix is a separate bug-fix); figure-it-out authors
    /// via @planner.
    #[test]
    fn review_trio_shapes() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        // interrogate: reviewers default "2".
        let inter = &wfs["interrogate"];
        let default = inter
            .inputs
            .iter()
            .find(|i| i.name == "reviewers")
            .and_then(|i| i.default.as_ref())
            .and_then(|d| d.as_str());
        assert_eq!(default, Some("2"), "interrogate defaults N=2");
        let briefs = inter
            .graph
            .as_ref()
            .expect("interrogate graph")
            .nodes
            .get("briefs")
            .expect("briefs node");
        assert!(
            matches!(briefs.action, Action::Parallel { .. }),
            "briefs is a parallel fanout"
        );
        // forensics: a @scout diagnose node.
        let fore = &wfs["forensics"];
        let diagnose = fore
            .graph
            .as_ref()
            .expect("forensics graph")
            .nodes
            .get("diagnose")
            .expect("diagnose node");
        assert!(
            matches!(diagnose.action, Action::DispatchJob { .. }),
            "diagnose is a dispatch"
        );
        // figure-it-out: @planner author step.
        let fio = &wfs["figure-it-out"];
        assert!(
            fio.steps
                .iter()
                .any(|s| s.name.as_deref() == Some("author")),
            "figure-it-out has an author step"
        );
    }

    /// bd-t86p: the goal DIRECT arm is a built-in (`goal-direct` + its
    /// gate-free `plan-direct` companion). Both parse, lint clean, and
    /// declare crew: auto — the captain-free compose surface needs
    /// zero-config auto-crew like the parent goal.
    #[test]
    fn goal_direct_arm_is_crew_auto_and_lint_clean() {
        let wfs = builtin_workflows();
        for name in ["goal-direct", "plan-direct"] {
            let wf = &wfs[name];
            assert_eq!(
                wf.crew,
                Some(CrewSpec::Auto),
                "{name} is crew: auto (captain-free zero-config)"
            );
            let report = toron_workflow::lint_workflow(wf);
            assert!(
                report.is_clean(),
                "{name} must lint clean (the steer gate): {:?}",
                report.issues
            );
        }
    }

    // ---- ADR-0019 Spec 7.2: operations playbooks (bd-w2-ops-szky) ----

    /// The five operations playbooks are builtins; each cites its principles
    /// and is crew: auto; autonomous-work documents the decide target.
    #[test]
    fn ops_playbooks_present_with_principles() {
        let wfs = builtin_workflows();
        for name in [
            "babysit-bead",
            "ship-beads",
            "autonomous-work",
            "pickup",
            "pause",
        ] {
            assert!(wfs.contains_key(name), "built-in {name} present");
            let wf = &wfs[name];
            assert!(!wf.principles.is_empty(), "{name} cites principles");
            assert_eq!(wf.crew, Some(CrewSpec::Auto), "{name} is crew: auto");
        }
        // autonomous-work documents the decide target in the description.
        assert!(
            wfs["autonomous-work"]
                .description
                .as_deref()
                .unwrap_or("")
                .contains("decide"),
            "decide can point at autonomous-work"
        );
    }

    /// ship-beads = predicate gate (no ready beads + claim-watch empty +
    /// every newly closed bead ledger-legal) THEN the p8 commit-and-push.
    #[test]
    fn ship_beads_predicates_then_ships() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let ship = &wfs["ship-beads"];
        assert_eq!(ship.steps.len(), 2, "predicate then ship");
        let Action::DispatchJob { spec, .. } = &ship.steps[0].action else {
            panic!("verify-predicate is a dispatch");
        };
        let task = spec
            .get("task")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(task.contains("br list --status ready"), "no ready beads");
        assert!(task.contains("claim-watch"), "claim-watch empty");
        assert!(task.contains("ledger check"), "ledger legal");
        let Action::DispatchJob { spec, .. } = &ship.steps[1].action else {
            panic!("ship is a dispatch");
        };
        assert_eq!(
            spec.get("stage").and_then(serde_json::Value::as_str),
            Some("commit-and-push"),
            "ship = p8 commit-and-push"
        );
    }

    /// autonomous-work uses the REAL loop (never reimplements it) and ends
    /// in a ship-beads call; the predicate input defaults to ship-beads.
    #[test]
    fn autonomous_work_uses_real_loop_then_ships() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let aw = &wfs["autonomous-work"];
        let Action::DispatchJob { spec, .. } = &aw.steps[0].action else {
            panic!("run-loop is a dispatch");
        };
        let task = spec
            .get("task")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(
            task.contains("flywheel loop run --work --on-blocked mail"),
            "the REAL loop"
        );
        assert!(
            task.contains("do NOT reimplement the loop"),
            "no reimplementation"
        );
        let Action::CallWorkflow { workflow, .. } = &aw.steps[1].action else {
            panic!("terminal check calls ship-beads");
        };
        assert_eq!(workflow, "ship-beads");
        let pred = aw.inputs.iter().find(|i| i.name == "predicate").unwrap();
        assert_eq!(
            pred.default.as_ref(),
            Some(&serde_json::json!("ship-beads"))
        );
    }

    /// pause kills nothing working unless force_working is explicit (the
    /// input defaults false and the task gates on it).
    #[test]
    fn pause_never_kills_working_without_force() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let pause = &wfs["pause"];
        let fw = pause
            .inputs
            .iter()
            .find(|i| i.name == "force_working")
            .unwrap();
        assert_eq!(fw.default.as_ref(), Some(&serde_json::json!(false)));
        let Action::DispatchJob { spec, .. } = &pause.steps[0].action else {
            panic!("pause is a dispatch");
        };
        let task = spec
            .get("task")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(task.contains("checkpoint save"), "durable state snapshot");
        assert!(task.contains("EXACT resume command"), "standing handoff");
        assert!(
            task.contains("kill NOTHING that is working"),
            "never kill working"
        );
        assert!(task.contains("force_working"), "unless explicit");
    }

    /// babysit-bead drives ONE bead only — the task never starts new beads
    /// and never writes a verdict (loop-only gate rows).
    #[test]
    fn babysit_bead_drives_one_bead_never_starts_new() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let bb = &wfs["babysit-bead"];
        let Action::DispatchJob { spec, .. } = &bb.steps[0].action else {
            panic!("babysit is a dispatch");
        };
        let task = spec
            .get("task")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(task.contains("ONE BEAD"), "one bead");
        assert!(
            task.contains("NEVER start new beads"),
            "never starts new beads"
        );
        assert!(
            task.contains("only the loop records gate rows"),
            "never writes a verdict"
        );
        assert!(
            task.contains("ledger check"),
            "drives to a legal ledger row"
        );
    }

    /// bd-t86p: goal-direct is goal MINUS the tracker + the plan gate — the
    /// direct arm's steps are ideate/plan/build/review/ship (no `beads`
    /// call, and `plan-direct` has no `request_approval` step). This pins
    /// the autonomous contract: a steer --direct run never parks on an
    /// approval or depends on the tracker.
    #[test]
    fn goal_direct_skips_tracker_and_approval_gate() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();

        // goal-direct: no call to the `beads` workflow (the tracker).
        let direct = &wfs["goal-direct"];
        let step_names: Vec<&str> = direct
            .steps
            .iter()
            .filter_map(|s| s.name.as_deref())
            .collect();
        assert_eq!(
            step_names,
            vec!["ideate", "plan", "build", "review", "ship"]
        );
        for s in &direct.steps {
            if let Action::CallWorkflow { workflow, .. } = &s.action {
                assert_ne!(
                    workflow.as_str(),
                    "beads",
                    "goal-direct must not call the beads workflow"
                );
            }
        }

        // plan-direct: the decompose dispatch, NO request_approval gate.
        let plan_direct = &wfs["plan-direct"];
        assert!(
            plan_direct
                .steps
                .iter()
                .all(|s| !matches!(&s.action, Action::RequestApproval { .. })),
            "plan-direct must not carry the plan approval gate"
        );
    }

    // ---- bd-2pcq Tier B: composition invariants (STEER-COMPOSE /
    //      WORKFLOW-COMPOSE) ----

    /// STEER-COMPOSE (bd-2pcq #7): the goal workflows' arm selection is
    /// structural — goal = bead-tracked chain (brainstorm → plan → beads),
    /// goal-direct = direct chain (brainstorm → plan-direct, no beads). The
    /// steer surface picks the arm by workflow NAME (goal vs goal-direct);
    /// this pins that the names carry the right composition.
    #[test]
    fn steer_compose_arm_selection_is_structural() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let goal = &wfs["goal"];
        let direct = &wfs["goal-direct"];

        // Tracked arm: the chain includes the beads (tracker) step.
        let goal_calls: Vec<&str> = goal
            .steps
            .iter()
            .filter_map(|s| match &s.action {
                Action::CallWorkflow { workflow, .. } => Some(workflow.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            goal_calls.contains(&"beads"),
            "tracked arm chains the tracker: {goal_calls:?}"
        );
        // Direct arm: NEVER calls beads.
        let direct_calls: Vec<&str> = direct
            .steps
            .iter()
            .filter_map(|s| match &s.action {
                Action::CallWorkflow { workflow, .. } => Some(workflow.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !direct_calls.contains(&"beads"),
            "direct arm must not chain the tracker: {direct_calls:?}"
        );
        // Both arms declare crew: auto (the steer surface composes the crew
        // strategy at invoke: auto | <squad>).
        assert_eq!(goal.crew, Some(CrewSpec::Auto));
        assert_eq!(direct.crew, Some(CrewSpec::Auto));
    }

    /// WORKFLOW-COMPOSE (bd-2pcq #6): the call_workflow chain resolves across
    /// the catalog+builtins resolver — every sub-workflow name referenced by
    /// the steer arms must exist as a built-in (the resolver returns a
    /// definition, never "unknown workflow").
    #[test]
    fn workflow_compose_chain_resolves_against_builtins() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        for arm in ["goal", "goal-direct"] {
            for s in &wfs[arm].steps {
                if let Action::CallWorkflow { workflow, .. } = &s.action {
                    assert!(
                        wfs.contains_key(workflow),
                        "{arm} composes {workflow} which must be a built-in"
                    );
                }
            }
        }
        // The direct arm's plan companion is gate-free AND resolves.
        assert!(
            wfs.contains_key("plan-direct"),
            "plan-direct built-in present"
        );
    }

    /// STEER-COMPOSE crew strategy (bd-2pcq #7): the composed goal/direct
    /// fleet includes the sub-workflow roles — a steer run spawns the WHOLE
    /// fleet (oracle/planner from brainstorm + planner from plan-direct +
    /// own coder/reviewer/housekeeper), never "no crew pane" on the first
    /// composed dispatch. (The recursive inference itself is unit-tested in
    /// crew.rs; this pins the end-to-end builtin composition.)
    #[test]
    fn steer_compose_crew_strategy_resolves_full_fleet() {
        let wfs = builtin_workflows();
        let resolve = |sub: &str| wfs.get(sub).cloned();
        for arm in ["goal", "goal-direct"] {
            let plan = crew::plan_crew_with(&wfs[arm], &resolve);
            for role in ["planner", "coder", "reviewer", "housekeeper"] {
                assert!(
                    plan.members.contains_key(role),
                    "{arm} fleet must include {role}: {:?}",
                    plan.members
                );
            }
            assert!(
                plan.members.contains_key("oracle"),
                "{arm} fleet must include oracle (brainstorm diverge): {:?}",
                plan.members
            );
        }
    }

    /// The purpose-named workflows declare `crew: auto` (zero-config,
    /// auto-crew — Spec 4c). `triage` is the reactive one (no crew).
    #[test]
    fn purpose_named_workflows_are_crew_auto() {
        let wfs = builtin_workflows();
        for name in [
            "brainstorm",
            "plan",
            "beads",
            "debate",
            "goal",
            "plan-review",
            "onboard",
            "triage-deep",
        ] {
            assert_eq!(
                wfs[name].crew,
                Some(CrewSpec::Auto),
                "{name} is crew: auto (zero-config auto-crew)"
            );
        }
        assert!(wfs["triage"].crew.is_none(), "triage has no crew");
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
                    if spec.get("profile").and_then(Value::as_str) == Some("@reviewer") =>
                {
                    assert_eq!(
                        spec.get("harness").and_then(Value::as_str),
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
                _ => {}
            }
        }
        for name in ["debate", "goal", "plan-review", "captain-audit"] {
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
    #[test]
    fn goal_is_call_workflow_chain() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let goal = &wfs["goal"];
        let chain: Vec<&str> = goal
            .steps
            .iter()
            .filter_map(|s| match &s.action {
                Action::CallWorkflow { workflow, .. } => Some(workflow.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            chain.contains(&"brainstorm") && chain.contains(&"plan") && chain.contains(&"beads"),
            "goal chains the built-ins: {chain:?}"
        );
    }

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

    /// bd-m7ac: weekly-review is a schedule-triggered graph — parallel
    /// scouts (3 elastic copies) feeding an oracle node that needs them.
    #[test]
    fn weekly_review_is_scout_fanout_to_oracle() {
        use toron_workflow::schema::{Action, Trigger};
        let wfs = builtin_workflows();
        let wr = &wfs["weekly-review"];
        // schedule trigger, project scope, owner oracle
        match &wr.trigger {
            Some(Trigger::Schedule { cron, owner, scope }) => {
                assert_eq!(cron, "0 4 * * 0");
                assert_eq!(owner.as_deref(), Some("oracle"));
                assert_eq!(*scope, toron_workflow::schema::CronScope::Project);
            }
            other => panic!("weekly-review must be schedule-triggered: {other:?}"),
        }
        // graph: scout-fanout (parallel @scout) -> oracle (@oracle, needs scouts)
        let g = wr.graph.as_ref().expect("graph present");
        let fanout = &g.nodes["scout-fanout"];
        assert!(matches!(fanout.action, Action::Parallel { .. }));
        let oracle = &g.nodes["oracle"];
        assert!(matches!(oracle.action, Action::DispatchJob { .. }));
        assert!(oracle.needs.contains(&"scout-fanout".to_string()));
        // the oracle's task templates the scout outputs (the synthesis input)
        let Action::DispatchJob { spec, .. } = &oracle.action else {
            unreachable!()
        };
        let task = spec
            .get("task")
            .and_then(serde_json::Value::as_str)
            .expect("oracle task present");
        assert!(
            task.contains("{{steps.scout-fanout}}"),
            "oracle consumes scout findings"
        );
        assert!(task.contains("idea"), "oracle outputs the idea contract");
    }

    /// bd-oj9o #2: the cold-start + deep-triage workflows exist, and the
    /// plan-review workflow (slash target) exists.
    #[test]
    fn onboard_triage_deep_plan_review_present() {
        let wfs = builtin_workflows();
        for name in ["onboard", "triage-deep", "plan-review"] {
            assert!(wfs.contains_key(name), "built-in {name} present (bd-oj9o)");
        }
    }

    /// ADR-0019 Spec 7.2 (amended): prototype + arena are builtins; the
    /// inputs carry the structural contracts (n default 2 = the min, mode
    /// default sketch, selection required on arena).
    #[test]
    fn prototype_and_arena_present_with_structural_defaults() {
        use toron_workflow::schema::{InputDecl, Workflow};
        let wfs = builtin_workflows();
        for name in ["prototype", "arena"] {
            assert!(wfs.contains_key(name), "built-in {name} present");
            let wf: &Workflow = &wfs[name];
            let input = |n: &str| {
                wf.inputs
                    .iter()
                    .find(|i: &&InputDecl| i.name == n)
                    .unwrap_or_else(|| panic!("{name} input {n}"))
            };
            // n: default 2 (min 2 — n=1 fails lint, lands with w2-lints).
            assert_eq!(input("n").default.as_ref(), Some(&serde_json::json!(2)));
            // mode: default sketch.
            assert_eq!(
                input("mode").default.as_ref(),
                Some(&serde_json::json!("sketch"))
            );
            // arena declares the selection rule UP FRONT (first-pass | rank-all | best-of).
            if name == "arena" {
                assert!(
                    input("selection").default.is_none(),
                    "selection is caller-declared"
                );
            }
        }
    }

    /// ADR-0019 Spec 7.2: prototype arms branch on mode — compile = SEQUENTIAL
    /// foreach (build_slot: true, scratch ref, measure+reset) and sketch =
    /// PARALLEL fan-out; the @oracle judge needs the arms node.
    #[test]
    fn prototype_branches_on_mode_and_oracle_judges() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let proto = &wfs["prototype"];
        let g = proto.graph.as_ref().expect("graph present");
        // arms node is a branch (mode switch)
        let arms = &g.nodes["arms"];
        let Action::Branch {
            if_guard,
            then,
            else_steps,
        } = &arms.action
        else {
            panic!("arms must branch on mode, got {:?}", arms.action);
        };
        assert!(
            if_guard.contains("compile"),
            "compile branch guard: {if_guard}"
        );
        // compile arm: foreach over {{inputs.arms}}, build_slot declared
        assert_eq!(then.len(), 1, "one compile arm step");
        assert!(
            matches!(then[0].action, Action::Foreach { .. }),
            "compile arms are sequential foreach"
        );
        assert!(
            then[0].build_slot,
            "compile arm serializes on the build slot"
        );
        // sketch arm: parallel over {{inputs.n}}, no build slot
        assert_eq!(else_steps.len(), 1, "one sketch arm step");
        assert!(
            matches!(&else_steps[0].action, Action::Parallel { over, .. } if over == "{{inputs.n}}"),
            "sketch arms fan out in parallel"
        );
        assert!(
            !else_steps[0].build_slot,
            "sketch arms write text only, no slot"
        );
        // judge: @oracle needs arms
        let judge = &g.nodes["judge"];
        assert!(judge.needs.contains(&"arms".to_string()));
        let Action::DispatchJob { spec, .. } = &judge.action else {
            panic!("judge is a dispatch");
        };
        let task = spec
            .get("task")
            .and_then(serde_json::Value::as_str)
            .expect("judge task");
        assert!(task.contains("@oracle") || spec.get("profile").is_some());
        assert!(task.contains("{{steps.arms}}"), "judge consumes the arms");
    }

    /// ADR-0019 Spec 7.2: arena carries the same shape PLUS the declared-up-front
    /// selection rule (first-pass | rank-all | best-of) in the judge task.
    #[test]
    fn arena_declares_selection_rule_up_front() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let arena = &wfs["arena"];
        let g = arena.graph.as_ref().expect("graph present");
        let judge = &g.nodes["judge"];
        let Action::DispatchJob { spec, .. } = &judge.action else {
            panic!("judge is a dispatch");
        };
        let task = spec
            .get("task")
            .and_then(serde_json::Value::as_str)
            .expect("judge task");
        assert!(
            task.contains("{{inputs.selection}}"),
            "selection rule declared up front"
        );
        assert!(task.contains("first-pass"), "first-pass documented");
        assert!(task.contains("rank-all"), "rank-all documented");
        assert!(task.contains("best-of"), "best-of documented");
        assert!(
            task.contains("comparison TABLE"),
            "parent keeps the table, not raw dumps"
        );
    }

    /// ADR-0019 Spec 7.2: neither prototype nor arena carries any
    /// worktree/br/.beads surface — the dispatch text bans them per arm and
    /// the workflows declare no worktree action (compile-enforced).
    #[test]
    fn prototype_arena_arms_ban_worktree_br_beads() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        for name in ["prototype", "arena"] {
            let wf = &wfs[name];
            let g = wf.graph.as_ref().expect("graph");
            let Action::Branch {
                then, else_steps, ..
            } = &g.nodes["arms"].action
            else {
                panic!("{name}: arms is a branch");
            };
            for step in then.iter().chain(else_steps.iter()) {
                let action = &step.action;
                let (Action::Foreach { each, .. } | Action::Parallel { each, .. }) = action else {
                    continue;
                };
                let Action::DispatchJob { spec, .. } = &each.action else {
                    panic!("{name}: arm each is a dispatch_job");
                };
                let task = spec
                    .get("task")
                    .and_then(serde_json::Value::as_str)
                    .expect("{name} arm dispatch task");
                // the ban is EXPLICIT (no worktree) and no creation
                // instruction exists (no `git worktree` command text)
                assert!(
                    task.contains("worktree"),
                    "{name} arm bans worktrees explicitly: {task}"
                );
                assert!(
                    !task.contains("git worktree") && !task.contains("worktree create"),
                    "{name} arm must never instruct a worktree create: {task}"
                );
                assert!(
                    task.to_lowercase().contains("no br"),
                    "{name} arm bans br: {task}"
                );
                assert!(
                    task.contains(".beads/"),
                    "{name} arm bans .beads/ writes: {task}"
                );
            }
        }
    }

    /// bd-oj9o #1: brainstorm/debate actually fan out (over > 1) — the dead
    /// `breadth`/`panel` inputs were replaced by a real parallel fan-out.
    #[test]
    fn brainstorm_and_debate_fan_out() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        for (name, node) in [("brainstorm", "diverge"), ("debate", "briefs")] {
            let wf = &wfs[name];
            let g = wf
                .graph
                .as_ref()
                .unwrap_or_else(|| panic!("{name} has a graph"));
            let n = g
                .nodes
                .get(node)
                .unwrap_or_else(|| panic!("{name}.{node} node exists"));
            let Action::Parallel { over, .. } = &n.action else {
                panic!("{name}.{node} is a parallel action");
            };
            let copies: usize = over.trim().parse().unwrap_or(1);
            assert!(
                copies > 1,
                "{name} must fan out over {copies} (parallelism is the point)"
            );
        }
    }

    /// bd-oj9o #1: the beads workflow carries the plan into the dispatch
    /// spec ({{inputs.plan}} → rendered into the headless prompt).
    #[test]
    fn beads_dispatch_carries_plan_input() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let beads = &wfs["beads"];
        let Action::DispatchJob { spec, .. } = &beads.steps[0].action else {
            panic!("beads step 0 is a dispatch_job");
        };
        assert_eq!(
            spec.get("plan").and_then(Value::as_str),
            Some("{{inputs.plan}}"),
            "beads dispatch spec carries the plan input"
        );
    }

    /// bd-oj9o #1: the reactive triage workflow has no hardcoded agent
    /// names (pi-alpha/oc-beta are spawn-dependent and break any real
    /// swarm) — ack goes to the operator, dispatch is profile-based.
    #[test]
    fn triage_has_no_hardcoded_agent_names() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let triage = &wfs["triage"];
        let yaml = BUILTIN_WORKFLOWS_YAML
            .iter()
            .find(|(n, _)| *n == "triage")
            .map(|(_, y)| *y)
            .unwrap();
        assert!(
            !yaml.contains("pi-alpha") && !yaml.contains("oc-beta"),
            "triage must not hardcode agent names"
        );
        let Action::DispatchJob { spec, .. } = &triage.steps[1].action else {
            panic!("triage step 1 is a dispatch_job");
        };
        assert_eq!(
            spec.get("profile").and_then(Value::as_str),
            Some("@housekeeper"),
            "triage dispatches via a profile, not a hardcoded name"
        );
    }

    /// bd-h4fc: the reducer/decider steps consume the fan-out outputs
    /// ({{steps.diverge}} / {{steps.briefs}}) — without the reference the
    /// planner/oracle decide blind (the ADR-0005 Spec 2b `inputs.ideas` /
    /// `inputs.arguments` parity under the dispatch_job shape).
    #[test]
    fn fanout_outputs_reach_reducers() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        for (name, node, token) in [
            ("brainstorm", "reduce", "{{steps.diverge}}"),
            ("debate", "decide", "{{steps.briefs}}"),
        ] {
            let g = wfs[name].graph.as_ref().expect("graph present");
            let Action::DispatchJob { spec, .. } = &g.nodes[node].action else {
                panic!("{name}.{node} is a dispatch_job");
            };
            let task = spec
                .get("task")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{name}.{node} has a task"));
            assert!(
                task.contains(token),
                "{name}.{node} consumes the fan-out ({token}): {task}"
            );
        }
    }

    /// bd-h4fc: no dispatch spec uses the dead `prompt` key — headless
    /// dispatch reads task/stage/base only, so `spec.prompt` would silently
    /// drop the mission (the brainstorm/diverge + debate/briefs regression).
    #[test]
    fn no_dispatch_spec_uses_dead_prompt_key() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        fn scan_action(action: &Action, wf: &str, dead: &mut Vec<String>) {
            match action {
                Action::DispatchJob { spec, .. } if spec.get("prompt").is_some() => {
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
    #[test]
    fn goal_carries_context_through_chain() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let goal = &wfs["goal"];
        for (step_name, plan_token, harness) in [
            ("build", "{{steps.track}}", None),
            ("review", "{{steps.build}}", Some("opencode")),
            ("ship", "{{steps.build}}", None),
        ] {
            let step = goal
                .steps
                .iter()
                .find(|s| s.name.as_deref() == Some(step_name))
                .unwrap_or_else(|| panic!("goal.{step_name} present"));
            let Action::DispatchJob { spec, .. } = &step.action else {
                panic!("goal.{step_name} is a dispatch_job");
            };
            assert_eq!(
                spec.get("plan").and_then(Value::as_str),
                Some(plan_token),
                "goal.{step_name} carries the prior output"
            );
            if let Some(h) = harness {
                assert_eq!(
                    spec.get("harness").and_then(Value::as_str),
                    Some(h),
                    "goal.{step_name} keeps the harness override"
                );
            }
            assert_eq!(
                spec.get("profile").and_then(Value::as_str).unwrap(),
                match step_name {
                    "build" => "@coder",
                    "review" => "@reviewer",
                    _ => "@housekeeper",
                }
            );
        }
        let ship = goal
            .steps
            .iter()
            .find(|s| s.name.as_deref() == Some("ship"))
            .unwrap();
        let Action::DispatchJob { spec, .. } = &ship.action else {
            panic!("ship is a dispatch_job, not call_workflow triage");
        };
        assert_eq!(
            spec.get("stage").and_then(Value::as_str),
            Some("commit-and-push"),
            "ship reuses the commit-and-push stage"
        );
        assert!(
            !BUILTIN_WORKFLOWS_YAML
                .iter()
                .find(|(n, _)| *n == "goal")
                .unwrap()
                .1
                .contains("workflow: triage"),
            "goal.ship must not call triage (invoke-path templates would render empty)"
        );
    }

    /// bd-h4fc: every decisive step fails the run on error (on_error:
    /// fail_run) — a failed synthesis/tracking/build must not pass silently
    /// as a success.
    #[test]
    fn decisive_steps_fail_run() {
        use toron_workflow::schema::StepOnError;
        let wfs = builtin_workflows();
        fn step_errors(wf: &toron_workflow::schema::Workflow) -> Vec<(String, bool)> {
            let mut out = Vec::new();
            for s in &wf.steps {
                out.push((
                    s.name
                        .clone()
                        .unwrap_or_else(|| s.action.type_name().into()),
                    s.on_error == StepOnError::FailRun,
                ));
            }
            if let Some(g) = &wf.graph {
                for (n, node) in &g.nodes {
                    out.push((n.clone(), node.on_error == StepOnError::FailRun));
                }
            }
            out
        }
        let checks: &[(&str, &[&str])] = &[
            ("brainstorm", &["reduce"]),
            ("plan", &["decompose"]),
            ("beads", &["create"]),
            ("debate", &["decide"]),
            ("goal", &["build", "review", "ship"]),
            ("plan-review", &["review"]),
            ("onboard", &["map", "seed"]),
            ("triage-deep", &["triage", "prioritize", "spawn"]),
        ];
        for (name, must_fail) in checks {
            let errors = step_errors(&wfs[*name]);
            for m in *must_fail {
                let (_, fail) = errors
                    .iter()
                    .find(|(n, _)| n == m)
                    .unwrap_or_else(|| panic!("{name}.{m} exists"));
                assert!(*fail, "{name}.{m} must be on_error: fail_run");
            }
        }
    }

    /// bd-h4fc: the reactive triage workflow templates ONLY payload fields
    /// that always exist on message_posted (content/id) — the former
    /// {{trigger.subject}} rendered an empty ack body even reactively.
    #[test]
    fn triage_uses_resolvable_payload_fields() {
        let yaml = BUILTIN_WORKFLOWS_YAML
            .iter()
            .find(|(n, _)| *n == "triage")
            .map(|(_, y)| *y)
            .unwrap();
        assert!(
            !yaml.contains("{{trigger.subject}}"),
            "triage must not template trigger.subject (absent from the payload)"
        );
        assert!(yaml.contains("{{trigger.content}}"), "ack uses content");
        assert!(yaml.contains("{{trigger.id}}"), "dispatch uses id");
    }

    /// bd-h4fc: onboard + triage-deep chains consume the prior step's output
    /// via spec.plan — a cold-start seed without the map, or a crew spawn
    /// without the prioritized list, was a disconnected dispatch.
    #[test]
    fn onboard_and_triage_deep_chain_outputs() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        for (name, step_name, token) in [
            ("onboard", "seed", "{{steps.map}}"),
            ("triage-deep", "prioritize", "{{steps.triage}}"),
            ("triage-deep", "spawn", "{{steps.prioritize}}"),
        ] {
            let wf = &wfs[name];
            let step = wf
                .steps
                .iter()
                .find(|s| s.name.as_deref() == Some(step_name))
                .unwrap_or_else(|| panic!("{name}.{step_name} present"));
            let Action::DispatchJob { spec, .. } = &step.action else {
                panic!("{name}.{step_name} is a dispatch_job");
            };
            assert_eq!(
                spec.get("plan").and_then(Value::as_str),
                Some(token),
                "{name}.{step_name} consumes the prior step output"
            );
        }
    }

    /// bd-wsui: captain-audit is the read-only analysis workflow — crew:
    /// auto graph (scout → oracle → reviewer), inputs topic/scope/
    /// report_path, every prompt ANALYSIS ONLY (no beads, no repo edits),
    /// and the report lands at {{inputs.report_path}}.
    #[test]
    fn captain_audit_is_analysis_only_chain_to_report_path() {
        use toron_workflow::schema::{Action, StepOnError};
        let wfs = builtin_workflows();
        let ca = &wfs["captain-audit"];

        // crew: auto + lint clean (the run gate).
        assert_eq!(ca.crew, Some(CrewSpec::Auto), "captain-audit is crew: auto");
        let report = toron_workflow::lint_workflow(ca);
        assert!(
            report.is_clean(),
            "captain-audit must lint clean: {:?}",
            report.issues
        );

        // inputs: topic, scope, report_path.
        let inputs: Vec<&str> = ca.inputs.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(inputs, vec!["topic", "scope", "report_path"]);

        // graph chain: gather (@scout) → synthesize (@oracle, needs gather)
        // → challenge (@reviewer, needs synthesize, opencode harness).
        let g = ca.graph.as_ref().expect("captain-audit has a graph");
        let gather = &g.nodes["gather"];
        let Action::DispatchJob { spec, .. } = &gather.action else {
            panic!("gather is a dispatch_job");
        };
        assert_eq!(spec.get("profile").and_then(Value::as_str), Some("@scout"));
        let synthesize = &g.nodes["synthesize"];
        let Action::DispatchJob { spec, .. } = &synthesize.action else {
            panic!("synthesize is a dispatch_job");
        };
        assert_eq!(spec.get("profile").and_then(Value::as_str), Some("@oracle"));
        assert!(synthesize.needs.contains(&"gather".to_string()));
        assert_eq!(synthesize.on_error, StepOnError::FailRun);
        let challenge = &g.nodes["challenge"];
        let Action::DispatchJob { spec, .. } = &challenge.action else {
            panic!("challenge is a dispatch_job");
        };
        assert_eq!(
            spec.get("profile").and_then(Value::as_str),
            Some("@reviewer")
        );
        assert_eq!(
            spec.get("harness").and_then(Value::as_str),
            Some("opencode"),
            "reviewer step pins the opencode harness (bd-42fs policy)"
        );
        assert!(challenge.needs.contains(&"synthesize".to_string()));
        assert_eq!(challenge.on_error, StepOnError::FailRun);

        // every prompt is ANALYSIS ONLY, no beads, no repo edits; the
        // report is written to {{inputs.report_path}}.
        let yaml = BUILTIN_WORKFLOWS_YAML
            .iter()
            .find(|(n, _)| *n == "captain-audit")
            .map(|(_, y)| *y)
            .unwrap();
        for needle in [
            "ANALYSIS ONLY",
            "no beads",
            "no repo edits",
            "{{inputs.topic}}",
            "{{inputs.scope}}",
            "{{inputs.report_path}}",
        ] {
            assert!(yaml.contains(needle), "captain-audit carries {needle:?}");
        }
    }

    /// bd-wsui: the captain-audit auto-crew resolves to EXACTLY the audit
    /// trio — scout + oracle + reviewer (no extras, no missing).
    #[test]
    fn captain_audit_crew_is_scout_oracle_reviewer() {
        let wfs = builtin_workflows();
        let plan = crew::plan_crew_with(&wfs["captain-audit"], &|_| None);
        assert_eq!(plan.members.len(), 3, "exactly the audit trio");
        for role in ["scout", "oracle", "reviewer"] {
            assert!(
                plan.members.contains_key(role),
                "captain-audit fleet includes {role}: {:?}",
                plan.members
            );
        }
    }

    // ---- ADR-0019 Spec 7.2 fix-path playbooks (bd-w2-fix-path-7gd5) ----

    /// The three fix-path playbooks are compiled-in, crew:auto, and declare
    /// their inputs.
    #[test]
    fn fix_path_playbooks_are_compiled_in() {
        let wfs = builtin_workflows();
        for name in ["bug-fix", "feature", "refactoring"] {
            let wf = &wfs[name];
            assert_eq!(wf.crew, Some(CrewSpec::Auto), "{name} is crew: auto");
            let input_names: Vec<&str> = wf.inputs.iter().map(|i| i.name.as_str()).collect();
            assert!(
                !input_names.is_empty(),
                "{name} declares inputs: {input_names:?}"
            );
        }
        // the exact input contracts
        let bug = &wfs["bug-fix"];
        assert_eq!(
            bug.inputs
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            vec!["symptom", "repro"],
            "bug-fix inputs"
        );
        let feat = &wfs["feature"];
        assert_eq!(
            feat.inputs
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            vec!["objective", "data_shape"],
            "feature inputs — data_shape REQUIRED (lint)"
        );
        let refac = &wfs["refactoring"];
        assert_eq!(
            refac
                .inputs
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            vec!["target", "invariant_cmd"],
            "refactoring inputs"
        );
    }

    /// bug-fix is a DAG with the TDD chain explicit: prove needs fix, fix
    /// needs tdd, tdd needs repro — a fix node without a repro predecessor
    /// cannot exist (the wave-2 structural lint reads exactly this shape).
    #[test]
    fn bug_fix_graph_orders_repro_before_fix() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let bug = &wfs["bug-fix"];
        let graph = bug.graph.as_ref().expect("bug-fix is a graph");
        assert_eq!(
            graph.nodes["tdd"].needs,
            vec!["repro".to_string()],
            "tdd runs after the repro"
        );
        assert_eq!(
            graph.nodes["fix"].needs,
            vec!["tdd".to_string()],
            "fix runs after the failing test"
        );
        assert_eq!(
            graph.nodes["prove"].needs,
            vec!["fix".to_string()],
            "prove runs after the fix"
        );
        // the reviewer prove node carries the opencode harness override
        let Action::DispatchJob { spec, .. } = &graph.nodes["prove"].action else {
            panic!("prove is a dispatch_job");
        };
        assert_eq!(
            spec.get("profile").and_then(Value::as_str),
            Some("@reviewer")
        );
        assert_eq!(
            spec.get("harness").and_then(Value::as_str),
            Some("opencode")
        );
    }

    /// The principle tags ride in the raw YAML with their CANONICAL names
    /// (ADR-0019 Spec 6 registry names — fixed by the ADR, validated by the
    /// wave-2 principles lint once the registry merge lands).
    #[test]
    fn fix_path_playbooks_carry_canonical_principle_tags() {
        let yaml_for = |name: &str| -> &str {
            BUILTIN_WORKFLOWS_YAML
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, y)| *y)
                .expect(name)
        };
        let cases = [
            (
                "bug-fix",
                [
                    "fix-root-causes",
                    "prove-it-works",
                    "sequence-verifiable-units",
                ],
            ),
            (
                "feature",
                [
                    "foundational-thinking",
                    "model-the-domain",
                    "prove-it-works",
                ],
            ),
            (
                "refactoring",
                [
                    "laziness-protocol",
                    "subtract-before-you-add",
                    "prove-it-works",
                ],
            ),
        ];
        for (name, tags) in cases {
            let yaml = yaml_for(name);
            for tag in tags {
                assert!(yaml.contains(tag), "{name} carries principle {tag}");
            }
        }
    }

    /// ADR-0019 Spec 0: NO pull-request / Graphite / gh-pr vocabulary in the
    /// playbooks (needles assembled so this test never trips itself).
    #[test]
    fn fix_path_playbooks_avoid_pr_vocabulary() {
        let forbidden = [
            format!("pull {}", "request"),
            "Graphite".to_string(),
            format!("gh {}", "pr"),
            format!("merge-{}", "when-ready"),
            format!("land the {}", "stack"),
        ];
        for name in ["bug-fix", "feature", "refactoring"] {
            let yaml = BUILTIN_WORKFLOWS_YAML
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, y)| *y)
                .unwrap();
            for needle in &forbidden {
                assert!(
                    !yaml.contains(needle.as_str()),
                    "{name} must not contain {needle:?} (Spec 0 vocabulary)"
                );
            }
        }
    }

    // ---- ADR-0019 Spec 7.2 measured-improvement playbooks (bd-w2-perf-to27) ----

    /// perf + hillclimb are compiled-in, crew:auto, with the exact input
    /// contracts (metric/baseline_cmd/budget and metric/target/max_iters).
    #[test]
    fn measured_improvement_playbooks_are_compiled_in() {
        let wfs = builtin_workflows();
        let perf = &wfs["perf"];
        assert_eq!(perf.crew, Some(CrewSpec::Auto), "perf is crew: auto");
        assert_eq!(
            perf.inputs
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            vec!["metric", "baseline_cmd", "budget"],
            "perf inputs"
        );
        let hc = &wfs["hillclimb"];
        assert_eq!(hc.crew, Some(CrewSpec::Auto), "hillclimb is crew: auto");
        let hc_inputs: Vec<&str> = hc.inputs.iter().map(|i| i.name.as_str()).collect();
        for required in ["metric", "target", "max_iters"] {
            assert!(
                hc_inputs.contains(&required),
                "hillclimb input {required}: {hc_inputs:?}"
            );
        }
        assert!(
            hc.inputs
                .iter()
                .any(|i| i.name == "max_iters" && i.default.is_some()),
            "max_iters carries a default cap"
        );
    }

    /// perf's keep/discard is an EXPLICIT control-flow branch on the measure
    /// result — the dry-run shows it (acceptance: keep/discard branch).
    #[test]
    fn perf_has_explicit_keep_discard_branch() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let perf = &wfs["perf"];
        let branch = perf
            .steps
            .iter()
            .find(|s| s.name.as_deref() == Some("keep-or-discard"))
            .expect("perf has the keep-or-discard branch step");
        let Action::Branch {
            if_guard,
            then,
            else_steps,
        } = &branch.action
        else {
            panic!("keep-or-discard is a branch");
        };
        assert!(
            if_guard.contains("steps.measure.improved"),
            "branch guards on the measure result: {if_guard}"
        );
        assert_eq!(then.len(), 1, "keep arm");
        assert_eq!(else_steps.len(), 1, "discard arm");
        assert_eq!(
            then[0].name.as_deref(),
            Some("keep"),
            "keep = commit the win"
        );
        assert_eq!(
            else_steps[0].name.as_deref(),
            Some("discard"),
            "discard arm"
        );
    }

    /// hillclimb's loop is a CAPPED retry — max_attempts is the iteration
    /// cap (dry-run shows it); each attempt keeps (ledger row + commit) or
    /// discards (revert) via the retry's success/failure split.
    #[test]
    fn hillclimb_loop_is_capped_retry() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let hc = &wfs["hillclimb"];
        let climb = hc
            .steps
            .iter()
            .find(|s| s.name.as_deref() == Some("climb"))
            .expect("hillclimb has the climb step");
        let Action::Retry {
            max_attempts, step, ..
        } = &climb.action
        else {
            panic!("climb is a capped retry");
        };
        assert!(
            *max_attempts >= 1,
            "retry carries the iteration cap: {max_attempts}"
        );
        let Action::DispatchJob { spec, .. } = &step.action else {
            panic!("each attempt is a dispatch_job");
        };
        let task = spec.get("task").and_then(Value::as_str).unwrap_or("");
        assert!(
            task.contains("DISCARD"),
            "attempt carries the discard rule: {task}"
        );
        assert!(task.contains("KEEP"), "attempt carries the keep rule");
        assert!(
            task.contains("max_iters"),
            "budget binds to the max_iters input"
        );
    }

    /// The principle tags ride in the raw YAML with canonical names.
    #[test]
    fn measured_playbooks_carry_canonical_principle_tags() {
        let yaml_for = |name: &str| -> &str {
            BUILTIN_WORKFLOWS_YAML
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, y)| *y)
                .expect(name)
        };
        for tag in [
            "prove-it-works",
            "sequence-verifiable-units",
            "laziness-protocol",
        ] {
            assert!(yaml_for("perf").contains(tag), "perf carries {tag}");
        }
        for tag in [
            "prove-it-works",
            "sequence-verifiable-units",
            "build-the-lever",
        ] {
            assert!(
                yaml_for("hillclimb").contains(tag),
                "hillclimb carries {tag}"
            );
        }
    }
}

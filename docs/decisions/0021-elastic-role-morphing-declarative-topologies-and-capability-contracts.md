<!-- governed-by: ADR-0005, ADR-0006, ADR-0007, ADR-0008, ADR-0009, ADR-0016, ADR-0018, ADR-0020, ADR-0022, toron ADR-0022, toron ADR-0024, toron ADR-0026, toron ADR-0032 -->
---
status: proposed  # 2026-11-17: implementation map in §6 (D1–D7, acceptance 1–7); not reviewed for acceptance
date: 2026-11-12
revised: 2026-11-13
decision-makers: Captain (bangedorrunt)
consulted: |
  AmberHarbor (first draft, 2026-11-12);
  code audit at flywheel 06b975bc, toron-workflow schema/engine, elm-effect-xstate export/dynamic (2026-11-13);
  live `flywheel stage list`, `workflow lint beads-lifecycle`, `br lint --help` on the operator host (2026-11-13).
informed: flywheel × toron × beads maintainers; harness agents (opencode/pi/fx/antigravity/gemini/claude/codex/grok)
amends: |
  ADR-0006 (wave sequencing by .done markers),
  ADR-0008 (is_compose_token gate; "topology" speech),
  ADR-0009 (stage catalog shape; plan-review runs once).
extends: |
  ADR-0018 (elm-effect is the core), ADR-0020 (swarm statechart), toron ADR-0022 (phase-native orchestration),
  toron ADR-0024 (repair loops), toron ADR-0032 (engine honesty).
companion: flywheel ADR-0022 (the swarm plan: who runs each step).
---

# ADR-0021: Shapes, Stage Contracts, and Witnessed Exits

| Field | Value |
|---|---|
| **Status** | **Proposed** (draft 2026-11-12; revised in place 2026-11-13) |
| **Governs** | Mode vs shape vocabulary; `compose:` flag on workflows; the stage contract (phase, mode, context, effort, reads, witness, repeat); `dispatch_job` await on job results; verification of the workflow that runs; stage catalog curation; the plan-polish loop |
| **Does not amend** | `flywheel → toron-workflow → toron-core`; Tick Law; journal-before-provider; bead close ceremony; SIGNOFF outer-only (ADR-0008) |
| **Companion** | [flywheel ADR-0022](0022-swarm-plan-one-contract-from-intent-to-crew.md) decides *who* runs a step (seat, harness, model). This ADR decides *what* a step needs and *what it must hand back*. |
| **Verification** | `flywheel workflow verify --json` over every catalog workflow; `flywheel stage lint`; `flywheel workflow lint <name>`; `bash scripts/verify-clippy.sh`; `bash scripts/clean-proof.sh --tests` |

---

## 0. Why this record was revised

The 2026-11-12 draft proposed elastic pane slots, in-pane morphing, a new
`flywheel-topology.v1` schema with its own statechart block, and sub-10 ms
transitions. A code audit found that most of its premises do not hold, and
that the parts that do hold already have a home in the stack. The draft
also had no answer for the question that matters most to an autonomous
crew: **how does a step prove it is done?**

| Draft premise | What the code shows (flywheel `06b975bc`) |
|---|---|
| 4 panes, 75 % idle | `wave-swarm` crew is `planner:1, coder:1, reviewer:1`. An idle harness spends no tokens; the cost is RAM and screen space (ADR-0022 §7.5). |
| `.done` polling adds 15–45 s per stage | The awaits already block on `fswatch`/`inotifywait`. Latency is one LLM turn, measured in minutes. The real defect is different: the marker is written by the agent on its honour, and a forgotten marker holds the run for up to ~11 h (240 attempts × ~140 s watch + 30 s backoff). |
| Compose tokens are hard-coded Rust | They are YAML builtins with tiers (built-in < user < project). `is_compose_token` (`src/builtin_workflows.rs:1586`) is only a whitelist of which YAML expands inline. |
| Need a topology catalog for scatter-gather, debate, repair | `parallel over: "@crew.<role>"` + `reduce` is scatter-gather; `src/cmd/debate.rs` and the `debate` workflow exist; `repair-until-verified` exists. |
| Statecharts are verified (TLC, zero divergence) | `SwarmLifecycle` (`src/elm/swarm_statechart.rs`) is `#![allow(dead_code)]` and only `workflow verify` drives it. It has an unreachable `Failed` state and an unbounded review→execute loop, and `workflow verify` reports `ok`. "Reachability" checks that the initial configuration has a leaf; "deadlock-free" feeds hand-picked events and counts a self-loop as an exit (`src/cmd/workflow.rs:1888-2015`). `tests/e2e_statechart_formal_verification.rs` checks its own copy of a transition table, which disagrees with production. `to_tla` exports topology only; guards and context are dropped ("every edge is always enabled"). |
| Need `Msg::StageDone` for reactive transitions | The reactive primitive exists: jobs 43001 → 43002 claim → 43004 result / 43006 error, `toron job result`, `toron wait --corr`, and `spawn` with `await: true` suspends the run on the job terminal. Only `dispatch_job` does not await, which is why markers exist. |
| Deliverable witnesses are new | `src/phase_contracts.rs` already checks evidence per phase — for headless JSON replies only. |
| Need a profile lifecycle for any domain | `flywheel profile {list, show, create, edit, set, remove, export, import}` exists. |

The draft's sound idea — coarse modes, skill-packaged tools, deliverable
witnesses (§5.3 of the draft) — is kept below as part of the stage contract.
Pane economy, morphing, and slot caps move to ADR-0022 as reconciler policy.

---

## 1. Context: three control languages and an untyped stage

Today a multi-agent run is described in three places that do not agree:

1. **Workflow YAML** (toron-workflow steps/graph) — what actually runs.
2. **Hand-written xstate charts** (`SwarmLifecycle`, `P2PNegotiation`) — what `workflow verify` checks. Not wired to the run.
3. **The compose grammar** (`--parts`, `is_compose_token`) — what an agent assembles.

The draft would have added a fourth (`flywheel-topology.v1` statecharts).

Stages are the other gap. A stage today is `{name, prompt, role?, phase?,
principles, params}` (`src/stages.rs:70`). It does not say what it reads,
what it must return, whether it may write, whether it needs a fresh context,
how hard the model should think, or whether it should repeat. All of that
lives in prose inside the prompt, so neither lint nor the engine can check
it. Consequences seen in the catalog (28 stages):

- **Misfit with no lint signal.** `beads-lifecycle.review2` dispatches `judge-arms`, whose prompt expects `{{steps.arms}}`; that workflow produces no arms. `workflow lint beads-lifecycle` passes.
- **Contradiction between stage and step.** `ce-plan-review` says "revise the beads"; the `review1` step that uses it says "return findings JSON + verdict".
- **Runs once where it should repeat.** `ce-plan` says "check your beads N times", but nothing repeats it.
- **Unused.** `ce-simplify`, `fresh-eyes-explore`, `minimal-fix`, `repro-baseline`, `tdd-red` are referenced by no built-in workflow.
- **LLM as a for-loop.** `wave-gate`, `wave-dispatch`, `wave-observe`, `spawn-crew` ask a model to run `br ready`, loop over beads, watch a channel, or call `flywheel spawn` — about 6 k characters of prompt for work the engine and the doorbell can do deterministically.

Agents do not pick stages; they pick a workflow (`decide`, `briefing`), and
workflows bind stages. So "agents cannot use all our stages" is a catalog
problem: the stages a workflow author needs are not typed, and the ones no
workflow needs are still listed.

---

## 2. Decision

### D1. Vocabulary: mode, shape, swarm

| Word | Meaning | Values | Where it lives |
|---|---|---|---|
| **mode** | How many agents, who supervises. The classifier's answer. | `open` \| `swarm` | `decide --json` (wire field stays `topology` for compatibility), `MODE` lines, `/swarm-mode` |
| **shape** | The coordination geometry of one workflow. | `pipeline` \| `fan-out` \| `debate` \| `repair` \| `polish` | `shape:` on a workflow; lint checks it (fan-out needs a `parallel`, repair and polish need a bounded `loop`) |
| **swarm** | The live object: one swarm plan (ADR-0022) + its seats + its run. | — | `flywheel swarm plan \| apply \| status` (ADR-0022) |
| **slot** | Model tier (ADR-0007). | `@cheap` \| `@standard` \| `@premium` | unchanged |

"Swarm" stays the name for multi-agent work and grows into the noun for the
running crew. It is not a shape. `wave-swarm` stays the default swarm
workflow: a pipeline whose execute phase fans out over the beads of one wave.
New speech does not use "topology" for shapes, and no longer uses "slot" for
panes.

### D2. One control language: workflow YAML is the source

- Workflow YAML stays the only authoring surface. No `flywheel-topology.v1`.
- `is_compose_token` is replaced by a field: `compose: true` on the workflow. Any tier may set it, so a project can ship its own compose token. `briefing.rs:218`'s duplicate gate reads the same field.
- `shape:` is declared on the workflow (D1).
- The draft's four "canonical topologies" are four workflows in the catalog (`wave-swarm`, `debate`, `repair-until-verified`, `plan-polish`), not a schema.

### D3. The stage contract

A stage says what one step needs from a seat and what it must hand back.
The profile says who runs it (ADR-0022). New fields, all optional except
`phase`:

```toml
[stages.plan-polish]
phase    = "plan"                 # required; closed set from toron Phase
role     = "planner"
mode     = "workspace-write"      # read-only | workspace-write | isolated (draft Tier 1)
context  = "warm"                 # warm | fresh  (toron ContextMode Inherit | Fresh)
effort   = "max"                  # mapped per harness by ADR-0022 D3; never a magic word in the prompt
reads    = ["inputs.objective", "inputs.epic", "steps.grill.target"]
witness  = "plan-polish.v1"       # named contract in phase_contracts
repeat   = { min = 2, max = 5, until = "converged" }
use_when = "a bead set exists and has not been polished"
prompt   = """..."""
```

Rules:

1. **`reads` is linted.** `workflow lint` fails when a step dispatches a stage whose `reads` the workflow cannot supply. This catches `judge-arms` in `beads-lifecycle`.
2. **`mode` reaches the harness** through the ADR-0022 adapter. Where no flag enforces it, the seat is `degraded`, never a silent pass (draft Tier 1, made honest).
3. **Skills stay on the profile** (`skills = [...]`), and they bring their own tools and MCP servers (draft Tier 2).
4. **`witness` is the exit** (D4). A stage with no witness may only feed another step; it cannot end a phase.
5. **`context: fresh` is the rule for review and verify stages.** A reviewer that inherits the author's context is not an independent review; `ModelRegistry::verifier_for` already prefers another model family.
6. **`effort` replaces in-prompt keywords** such as "ultrathink". The keyword works on one harness family; `effort` reaches pi `--thinking`, grok `--reasoning-effort`, agy `--effort`, or reports `degraded`.
7. A step's own fields override the stage's (`context:` on the step wins), as `DispatchSpec` already allows.

### D4. Witnessed exits: a step ends on evidence, not a marker

1. **`dispatch_job` gains `await: result`.** The run suspends until the job's 43004 result or 43006 error, exactly as `spawn` with `await: true` does today, bounded by `no_claim_timeout` and a new `result_timeout`. A pane agent finishes with `toron job result <job> --witness <file.json>`; headless leaves return the witness as stdout JSON (today's `phase_contracts::agent_report`).
2. **The witness is checked by the engine** against the stage's named contract (extend `phase_contracts`). A missing or failing witness is a typed failure that goes to the step's `on_error` or the enclosing repair loop. It is never success.
3. **Fact-derived exits win over reported ones.** Where a ledger already knows the answer, the exit is a `verify` step over that ledger, not a witness. Examples: execute ends when every bead of the wave is closed and `flywheel ship check` is legal; plan ends when `br lint` is clean and `br dep cycles` is 0.
4. **`.done` markers retire.** For one release the runner writes them as sidecars after a witnessed exit, so external watchers keep working; nothing reads them.
5. **The LLM-as-loop stages become engine steps.** `wave-gate` → a `verify` over `flywheel preflight` + `br ready --json` (already wave-gated by the typed field) with a lower-wave hold check. `wave-dispatch` → `foreach` over the ready set with `dispatch_job await: result`. `wave-observe` → the doorbell fold's escalation (it already sees claims, receipts, and stalls). `spawn-crew` → the `spawn` action. The planner seat in `wave-swarm` then only plans.

### D5. Verify the workflow that runs

1. **Lower every workflow to an elm-effect-xstate `DynamicMachine`** (`MachineConfig` JSON already supports compound, parallel, `invoke`, `after`, `onDone`). Steps become states; `branch`/`guard` become guarded transitions; `loop`/`retry` become bounded self-cycles with a counter in context; `parallel`/`foreach` become parallel regions joined by `onDone`; `request_approval` and `park` become waiting states with an `after` bound.
2. **`flywheel workflow verify [NAME]` checks the lowered machine of any catalog workflow**, not two hand-written charts:
   - *reachable*: breadth-first search from the initial state reaches every state;
   - *can finish*: every state can reach a final state (success or failure);
   - *bounded*: every cycle passes through a loop or retry with a finite `max`, and every waiting state has a timeout. The verifier names this check `bounded` and reads `park` (`max_wait_secs > 0`), `dispatch_job` `await: result` (`result_timeout > 0` and a bounded `no_claim_timeout`), `request_approval` (`timeout_secs > 0`), and loop/retry `max`. The engine enforces the approval bound: a wait past its deadline settles as a typed `timeout` (not approved, not rejected), scheduled as a timer message with `now_ms` as data; an absent `timeout_secs` stays unbounded;
   - *witnessed*: every phase-ending step has a witness or a fact-derived exit (D4). A phase end is a `dispatch_job` carrying a `spec.phase` named in `phase_contracts::WITNESSES`. It passes when its `spec.stage` resolves in the layered `StageLibrary` to a stage declaring a `witness` (the seam the engine checks at step exit), or when it names no stage, leaving the phase-chain contract check (`phase_chain::run_ship_chain`) as its exit. `compound` is outside the witness registry, so a compound-tagged step is not a witness-bearing phase end.
3. **The report names what is not checked.** Guards are over-approximated (every edge enabled); data is not modelled. `--export-tla` emits that topology-only module and says so in its header. No TLC proof is claimed for it.
4. **Real model checking is for protocols, not workflows.** Use `/create-formal-verification-skill` to write hand TLA+ models, run with TLC, for: (a) job claim/lease/reconcile — at most one claimer settles a job, an expired lease never settles over its successor; (b) the ADR-0022 reconciler — at most one live pane per pin, reservations survive a respawn. Each check must be shown failing on a broken variant first.
5. **Honesty fixes ship first, independent of the rest:** make `SwarmLifecycle.Failed` reachable and bound the review→execute loop (or delete the chart once D5.1 lowers `wave-swarm`); point `tests/e2e_statechart_formal_verification.rs` at the production machine; rename the current checks to what they test until D5.2 lands; remove "zero-divergence TLA+ proofs" and "mathematically verified" from `AGENTS.md` and `~/.agents/skills/flywheel/SKILL.md`.

### D6. Curate the stage catalog

A small spine of contract-typed stages, plus stages that belong to one
workflow. `stage list` groups by phase and prints `use_when`.

"Unused" is decided by the catalog, never by history. A stage is USED when a
catalog workflow references it in a `stage:` field, or when its own contract
declares how it is reached without one — `reached_by = "compose"` for a crew
compose token (`scout:fresh-eyes-explore`), `reached_by = "manual"` for
`flywheel send --stage`. A stage that is neither referenced nor declared is
DEAD, and `flywheel workflow verify` fails on it; `workflow verify` keeps
reporting the set, and `stage list --unused` names it, so retiring a stage is
a decision with an owner rather than a slow decay.

The earlier rule read the run log (a stage no run had used in 30 days was a
candidate). It is withdrawn: a workflow scan cannot see the manual surface, so
a hard failure on it depended on what the operator last ran, and a proof
machine has no run log at all. A gate that ships must answer the same question
on every machine, and "who can reach this" is a property of the catalog.

| Phase | Spine stage | Disposition of the others |
|---|---|---|
| plan | `ce-brainstorm` (grill), `ce-plan` (create), **`plan-polish`** (new, D7) | `ce-plan-review` becomes `plan-polish` in audit form (`mode = read-only`, `context = fresh`, `repeat` off); `judge-arms` leaves `beads-lifecycle` and stays with `arena`/`debate` |
| implement | `ce-execute`, `ce-simplify` | `ce-execute`'s "SIMPLIFY AS YOU GO" clause and `ce-simplify` say the same thing; keep one copy (the stage) and let `wave-swarm` run it after execute. `tdd-red`, `repro-baseline`, `minimal-fix` are wired into `bug-fix` or retired |
| review | `ce-review` | — |
| verify | `ce-evaluate` (one repair pass, fresh), `wave-verify` (receipts) | `prove-verify` becomes `wave-verify`'s per-bead form with `context = fresh` and a different family |
| compound | `ce-compound` | — |
| orchestration | — | `wave-gate`, `wave-dispatch`, `wave-observe`, `spawn-crew` become engine steps (D4.5) |
| workflow-private | `baseline-measure`, `measure-keep-or-discard`, `bv-triage`, `prioritize-assign`, `repo-map`, `seed-first-task`, `resume-pickup`, `diagnose-only`, `fresh-eyes-explore`, `commit-and-push`, `judge-arms` | keep; add `phase`, `reads`, `use_when`; retire what stays unused |

### D7. The plan-polish loop

Beads are cheaper to fix than code. One polish pass is not enough in
practice, and the current lifecycle runs one review that is told only to
report. The polish stage revises the bead set in place, at least twice, until
a round changes nothing, under a fence that is not the model's opinion.

**Stage `plan-polish`** (phase `plan`, role `planner`, `mode =
workspace-write`, `context = warm`, `effort = max`, `repeat = {min 2, max 5,
until converged}`):

> Polish round for the bead set of {{inputs.objective}} (epic {{inputs.epic}}).
>
> Check over each bead super carefully. Are you sure it makes sense? Is it
> optimal? Could we change anything to make the system work better for its
> users? If so, revise the beads now, in place (`br update`, `br dep add`,
> `br create`). It is far cheaper to fix the plan here than after
> implementation.
>
> Do not oversimplify. Do not lose any feature or functionality. The
> coverage map from the grill step lists every requirement; after your
> revisions each requirement must still map to at least one bead. A
> requirement may leave the map only by moving to non-goals with a stated
> reason.
>
> Every bead that changes behaviour names its tests: unit tests by file and
> case, each with what it proves and how it can fail; and an end-to-end check
> script with detailed, structured logging, so that each step records what it
> ran, its inputs, and what it observed, and a failure can be diagnosed from
> the log alone. End-to-end scripts follow the repo's test sandbox (temp
> HOME/XDG, deny-shims, spare ports; never the live store). A large
> end-to-end suite is its own bead that depends on the feature beads.
>
> Every bead stays self-contained: background, reasoning, acceptance, a
> `## VERIFY` fence with the exact command, and `## PRINCIPLES`.
> Dependencies are explicit and acyclic.
>
> Look in particular for: missing steps; wrong order; a bead too large to
> verify in one pass (split it); a bead too small to stand alone (merge it);
> hidden coupling between beads that will touch the same files; acceptance
> that cannot fail; missing failure, rollback, or migration paths; missing
> docs and skill sync.
>
> Reply with only JSON: `{"revised": ["bd-…"], "added": ["bd-…"],
> "removed": [{"id": "bd-…", "reason": "…"}], "coverage": {"<requirement>":
> ["bd-…"]}, "open_risks": ["…"]}`.

**Fence after every round** (a `verify` step, fact-derived, D4.3):

1. `br lint --json` over the epic is clean (template sections present).
2. `br dep cycles --json` is 0.
3. Coverage does not shrink: every requirement from `ce-brainstorm`'s `target` is in `coverage` or in `non_goals` with a reason.
4. Every bead of type feature/task/bug has a `## VERIFY` fence and names unit and end-to-end tests.

**Convergence is measured, not reported.** The runner computes a digest of
the epic's bead set (ids, descriptions, dependencies) after each round.
`converged` means the fence passes and the digest equals the previous
round's. The loop runs at least `min` rounds and stops at `max`; exhausting
`max` without convergence is a typed failure that parks for the captain
(or, under ADR-0022 `signoff = witness`, files the open risks as beads and
continues).

**Then one independent audit.** After the loop, one `plan-polish` pass in
audit form (`mode = read-only`, `context = fresh`, a different model family)
returns findings only. Findings feed one more warm polish round (the old
`integrate` step). This replaces `review1` + `review2`.

New `beads-lifecycle` order:

```
grill (ce-brainstorm) → create (phase-plan) → polish ×[2..5] → audit (fresh, other family)
  → integrate (one polish round on the findings) → [gate-before-implement] → implement (wave-swarm)
  → verify → signoff
```

`repeat` needs one engine change: `loop` gains `min:` (toron-workflow
`Action::Loop`). Until it lands, a workflow writes two fixed polish steps
followed by a `loop` with `until: steps_polish_converged == true`.

The same `repeat` field fits other stages that improve with passes:
`ce-simplify` (until no diff), `ce-review` fix-and-recheck.

---

## 3. Consequences

**Better**

- One language to read, run, and verify; the chart that is checked is the workflow that runs.
- A step ends on evidence the engine checks. A forgotten marker no longer holds a run for hours.
- Stage misuse (`judge-arms` in `beads-lifecycle`) fails lint.
- Plans are polished until stable, with a coverage map that stops features from being lost.
- The wave planner seat stops spending tokens on loops the engine can run.

**Costs**

- Crew harness prompts must call `toron job result --witness`. Pane agents that ignore it fail loudly instead of passing quietly; expect a migration period of loud failures.
- `Stage` gains seven fields and lint rules; user-tier stage files need `phase` added.
- `plan-polish` at `effort = max` for 2–5 rounds is expensive on large epics. `repeat.max` and the ADR-0022 budget are the controls.

**Rejected**

- `flywheel-topology.v1` (a fourth control language).
- A hard pane cap and in-pane morphing as architecture (ADR-0022 D7 treats them as reconciler policy).
- Claiming TLC proofs from a topology-only export.

---

## 4. Rollout

Each step ships alone and changes no default until its last bullet.

1. **Honesty** (D5.5). Small, independent, first.
2. **Stage contract** (D3) with `reads` lint; add `phase` to every stage; `stage list` by phase; `--unused` (catalog-referenced ∪ `reached_by` declared; the rest is fatal to `workflow verify`).
3. **`plan-polish`** (D7) with two fixed steps + loop; new `beads-lifecycle` order; fence verify step; digest convergence.
4. **Witnessed exits** (D4): `dispatch_job await: result`, `toron job result --witness`, witness contracts; `wave-swarm` moves off markers; wave stages become engine steps.
5. **Lowering and verify** (D5.1–D5.3) over every catalog workflow; delete the hand-written `SwarmLifecycle` once `wave-swarm` lowers.
6. **`compose:` field** (D2) replaces `is_compose_token` in both places.
7. **Protocol TLA+** (D5.4) via `/create-formal-verification-skill`.
8. **`loop min:`** in toron-workflow; collapse the two fixed polish steps.

## 5. Acceptance

1. `flywheel workflow lint beads-lifecycle` fails on today's file (`judge-arms` reads `steps.arms`) and passes on the new one.
2. `flywheel workflow verify --json` lists every catalog workflow. On today's `SwarmLifecycle` it reports `Failed` unreachable and the review→execute cycle unbounded.
3. A `wave-swarm` run whose coder never writes a witness fails at `result_timeout` with a typed error naming the step; no `.done` file is read.
4. A `beads-lifecycle` run on a seeded epic performs at least two polish rounds, stops when the digest is unchanged, and fails when a round drops a requirement from coverage.
5. A project-tier workflow with `compose: true` expands under `--parts` without a rebuild.
6. Each new check is shown failing on a broken variant before it is trusted.
7. `bash scripts/verify-clippy.sh` and `bash scripts/clean-proof.sh --tests` pass.

---

## 6. Implementation map (2026-11-17)

Every name below was grepped, not recalled (the decisions and acceptance
rows at `8ce0e80e`, the bd-jhz8x.6 mutants at the commit that added them). A mutation id is
**spec-qualified** (`spec-id`) because ids collide across specs: `adr0021-gates
S1` is the shape gate, not `plan-spawn-consent S1`. `AD²` means the row is
witnessed by ADR-0022's map (§8), which owns the adapter/profile half of D3.

### Decisions

| # | Witness test | Mutation | Status |
|---|---|---|---|
| D1 vocabulary (`mode`, `shape`, no `swarm` shape) | `adr0021_shape_lint_refuses_a_fanout_without_parallel`, `decide_router_topology_declared_no_silent_swarm`; `Shape` has five values and no `Swarm` (`workflow_catalog.rs`) | `adr0021-gates S1` | closed |
| D2 one control language (`compose:` is data) | `adr0021_compose_override_wins_from_the_upper_tier`, `beads_lifecycle_is_compose_token_and_beads_is_not` | `adr0021-gates C1` | closed |
| D3.1 `reads` is linted | `adr0021_reads_lint_refuses_a_missing_read`, `stage_contract_judge_arms_without_arms_fails` | `adr0021-gates R1` | closed |
| D3.2 `mode` reaches the harness, else `degraded` | `harness_default_plan_says_honored_and_the_launch_injects_nothing`, `dry_run_ambiguous_from_profile_is_degraded` | `v4-adapter-property V4` | closed (AD² D3/V4) |
| D3.3 skills stay on the profile | `digest_is_bounded_deterministic_and_order_sensitive`, `empty_skills_have_no_digest`, `builtins_have_verbatim_preamble_and_skills` | none | closed (structural: `Stage` has no skills field; the profile does) |
| D3.4 `witness` is the exit | `adr0021_witness_exit_refuses_an_empty_report`, `durable_body_witness_fail_goes_to_on_error`, `durable_body_witness_missing_report_fails` | `adr0021-gates W1`, `witness-sidecar W1` | closed |
| D3.5 review + verify run `context: fresh` | `stage_contract_seed_phases_complete` (all four seeds fresh + read-only), `beads_lifecycle_plan_polish_loop` (audit fresh) | `adr0021-witnessed E1` | closed (bd-jhz8x.6) |
| D3.6 `effort` replaces in-prompt keywords | `dispatch_resolves_effort_stage_over_profile`, `resolver_v4_effort_bare_matrix`, `grok_effort_is_honored_from_profile_and_user` | `adr0021-witnessed E2` | closed (bd-jhz8x.6) |
| D3.7 a step's fields override the stage | structural: a stage carries no dispatch fields (`DispatchSpec` lives on the step, the stage supplies only the prompt) | none | closed (structural) |
| D4.1 `dispatch_job await: result` suspends to the terminal | `wave_swarm_stages_await_result`; toron `graph_dispatch_await_waits_for_the_job_terminal` | `graph-await P1/T1/O1` (toron), `wave_swarm_*` | closed (bd-jhz8x.2/.5) |
| D4.2 the engine checks the witness; a miss is typed | `durable_body_witness_fail_goes_to_on_error`, `durable_body_witness_missing_report_fails` | `adr0021-gates W1`, `witness-sidecar W1` | closed |
| D4.3 fact-derived exits win over reported ones | `wave_swarm_execute_exit_is_ledger_fact`, `wave_swarm_execute_exit_runs_against_ledger` | `adr0021-witnessed E3` | closed (bd-jhz8x.6) |
| D4.4 `.done` markers retire (sidecars, never read) | `wave_swarm_no_marker_reads` | `witness-sidecar W1` | **partial: nothing reads a marker, but the sidecars are still steps — the RUNNER writing them is bd-jhz8x.3** |
| D4.5 the LLM-as-loop stages become engine steps | `wave_swarm_stages_await_result` (dispatch), `a_lower_open_wave_holds_and_names_the_blockers` (gate), `a_blocked_pane_escalates_to_the_captain_once_per_episode` (observe) | `wave-gate-engine G1/G2`, `observe-fold W1/R1/K1`, `observe-escalation L1/E1/B1/F1` | **partial: gate/dispatch/observe are engine steps; `spawn-crew` is still a stage — bd-jhz8x.2** |
| D5.1 lower every workflow to a machine | `workflow_lowering_unreachable_node_named`, `workflow_lowering_can_finish_names_stranded_state`, `workflow_lowering_unbounded_back_edge_named`, `workflow_lowering_approval_without_timeout_fails` | `adr0021-gates L1` (bounded), `adr0021-witnessed E4/E5` (reachable, can-finish) | closed (bd-jhz8x.6) |
| D5.2 `workflow verify` checks the machine; the report says what it does not prove | `statechart_verify_passes_the_shipped_charts`, `statechart_report_fails_on_an_orphan_and_a_trap`, `test_cli_workflow_verify_json_contract` | `adr0021-gates S1/L1/W1`, `adr0021-witnessed E4/E5`, `workflow-verify-unused U1` | closed (bd-jhz8x.6) |
| D5.3 the report names what is not checked | `test_cli_workflow_verify_plain_text` (scope line), `test_cli_workflow_verify_json_contract` (scope field) | none | closed (the text is the assertion) |
| D5.4 hand TLA+ protocols, each shown failing on a broken variant | `.agents/skills/verify-protocol-tla/models/{JobLease,ReservationBoard,Respawn}.tla` with `.broken-*.cfg` variants, run by `scripts/verify-formal.sh` (the proof's `verify-formal` step) | none (the models are the artifact) | closed |
| D5.5 honesty fixes ship first | `tests/e2e_statechart_formal_verification.rs` — `test_cli_workflow_verify_plain_text`, `test_cli_workflow_verify_json_contract`, `test_cli_workflow_verify_export_tla` (SwarmLifecycle retired); `AGENTS.md`/skill grep for "zero-divergence"/"mathematically verified" is empty | none | closed |
| D6 curate the catalog (unused is a property of the catalog) | `an_undeclared_unreferenced_stage_fails_and_a_declaration_clears_it`, `declared_reach_names_only_real_stages`, `stage_contract_list_filters` (`stage list --phase`) | `workflow-verify-unused U1`, `unused-stage U1`, `adr0021-witnessed E6` | closed (bd-jhz8x.6) |
| D7 the plan-polish loop (min 2, max 5, digest convergence, fence) | `beads_lifecycle_plan_polish_loop`, `adr0021_plan_fence_never_converges_on_a_changed_digest`, `converged_only_on_equal_previous`, `digest_sees_content_and_dependency_changes`, `fence_passes_only_on_double_zero` | `adr0021-gates F1` | **partial: the fence never checks coverage non-shrink — bd-jhz8x.7** |

### Acceptance

| # | Claim | Witness | Status |
|---|---|---|---|
| 1 | lint fails on `judge-arms` without an arms step, passes with one | `stage_contract_judge_arms_without_arms_fails` | closed, `adr0021-gates R1` |
| 2 | `workflow verify --json` lists every catalog workflow; the retired `SwarmLifecycle` is unreachable/unbounded | `test_cli_workflow_verify_json_contract` (chart list), `test_cli_workflow_verify_single_named_chart` | superseded: D5.1 deleted the hand chart; the test now pins its ABSENCE and `wave-swarm` verifying as a workflow |
| 3 | a coder that never writes a witness fails typed at `result_timeout`; no `.done` is read | `wave_swarm_missing_witness_fails_at_result_timeout`, `wave_swarm_no_marker_reads` | closed (bd-jhz8x.5 un-ignored it), `graph-await P1/T1/O1` |
| 4 | two polish rounds, converge on an unchanged digest, fail on a dropped requirement | `beads_lifecycle_plan_polish_loop`, `adr0021_plan_fence_never_converges_on_a_changed_digest` | **partial: the dropped-requirement half is not checked — bd-jhz8x.7** |
| 5 | a project-tier `compose: true` expands without a rebuild | `adr0021_compose_override_wins_from_the_upper_tier` | closed, `adr0021-gates C1` |
| 6 | every new check is seen failing on a broken variant first | `scripts/mutations/adr0021-gates.spec.sh` 6/6 RED at `bd-jhz8x.1` | closed |
| 7 | `verify-clippy.sh` and `clean-proof.sh --tests` pass | those scripts, plus `verify-clippy-gates` and `proof-suite-and-isolation` | closed, `verify-clippy-gates W1`, `proof-suite-and-isolation S1` |

### What the map does not claim

Every row above was grepped; none was recalled. A row marked **open** or
**partial** is a real gap with an owner bead — nothing here is closed without
both a witness test and a mutation seen RED. The five checks that were
witnessed but unmutated are closed by bd-jhz8x.6 (`adr0021-witnessed E1–E6`,
6/6 RED); the missing coverage fence is bd-jhz8x.7; the sidecar-writer and
`spawn-crew` legs stay with bd-jhz8x.3 and bd-jhz8x.2. The topology export
still proves nothing (D5.3): no model checker runs over it, and D5.4's TLC
models cover protocols, not workflows.

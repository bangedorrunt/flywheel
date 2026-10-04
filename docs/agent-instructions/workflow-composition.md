# Workflow Composition — the Agent's Guide to Optimizing Flywheel Work

<!-- governed-by: ADR-0022, ADR-0005, ADR-0010, ADR-0019 -->
<!--
Agent-facing tutorial. SHIPPED = works today at this commit. Every command
block below marked SHIPPED was verified against the CLI surface.
-->

You are a captain or crew agent driving flywheel. This document teaches you
how to get the most out of the workflow system: when to run what exists,
when to compose something new, how to do it safely, and how to promote what
works into the catalog so nobody improvises the same thing twice.

Compose-arm reference: `flywheel` skill §1 + §5 (the `runtime-adaptation` skill was folded into it; the compose-arm content now lives there).
Design doc: toron `docs/decisions/0022-phase-native-orchestration.md`.

---

## 1. The mental model (60 seconds)

Five nouns, not interchangeable:

| Noun | What it is | What it is not |
| :--- | :--- | :--- |
| **stage** | one named prompt (`send --stage ce-execute`). Catalog: `stage list`. | a runner. |
| **workflow** | engine procedure (YAML steps or graph). A step *may* dispatch a stage to a `@role`. SIGNOFF is `request_approval`, not a stage. | a topology. |
| **profile** | role → harness + model + skills. `crew: auto` counts `@roles`. | what makes a compose token. |
| **topology** | how work is supervised: loop / swarm / open. | Workflow YAML. "Loop workflow" is forbidden speech. |
| **compose** (verb) | assembling a Workflow from parts (`--parts`, `--from-yaml`, `--agent`). | starting anything. |
| **compose token** | bare name legal as `--parts`: `wave-swarm`, `repair-until-verified`, or `beads-lifecycle`. Preferred door: `workflow compose … --parts <token> --run`. Legacy alias: `workflow run <token> --execute`. | a recipe / squad. |
| **ordinary builtin** | the same Workflow type, not a `--parts` token (e.g. captain-audit). | a compose token. |

Door map (one door per intent):

| Intent | Door | Not |
| :--- | :--- | :--- |
| Assemble a NEW Workflow from parts | `workflow compose … --parts …` | spawn kind/count |
| Preview assembled graph + crew | compose **without** `--run` | claiming "started" |
| Start a compose-token durable run | **`workflow compose … --parts <token> --run`** (**preferred**) | treating `workflow run <token> --execute` as primary |
| Legacy alias for compose token | `workflow run <token> --execute` (**legacy alias**, not preferred) | teaching it as the door |
| Run a named ordinary builtin | `workflow run <name> --execute` | bare `--parts captain-audit` |
| Inline a compose token inside parts | `--parts wave-swarm` / `repair-until-verified` / `beads-lifecycle` | calling it a "recipe" / squad |
| Supervise a solo durable lane | topology `loop-mode` | "run the loop workflow" |
| Human gate on outer wave | SIGNOFF = `request_approval` on **outer** run only | nested crew inventing signoff |

Invariants (each stated once): **preview ≠ started** — compose without `--run` only lints + previews the crew. **SIGNOFF outer-only** — nested captain+crew via `call_workflow` / `dispatch_job` never open a second `request_approval`, and a parked SIGNOFF is announced by the run's approval drainer (`flywheel workflow approve` resolves it; a resumed run re-issues it). **wave-swarm sequences on completion artifacts** — each gated pair (gate→dispatch→execute→review→verify) is separated by an await step that retries a `verify` of `.flywheel/wave/<label>-<stage>.done`, the marker the predecessor writes; per-step budget 240 x 30s, and exhaustion fails at that step with `wave in flight … extend with --timeout` (`--timeout` / `FLYWHEEL_COMPOSE_MAX_SECONDS` stays the run's outer bound). **join-before-post** — `compose --run` ensures the token's declared `steer_channel` (refusing up front when the channel cannot be resolved) and joins every spawned pin before any stage posts; briefing binds `captain_pin` to the arriving pin in the emitted `--inputs`, and names it in `questions` when it cannot derive it. Ordinary builtins enter via `workflow run <name> --execute` — never bare `--parts`. Open mode may run a bounded ordinary workflow or stage in the current pane; that procedure does not change topology or grant delegation permission. On every follow-up/material context change, re-check `workflow list --brief --json` and `stage list`, choose the smallest vehicle, or announce `none: raw` rather than replaying a stale first-prompt choice.

Recursive `call_workflow` is durable Inception-style composition, but host-bounded: serialized lineage rejects cycles, depth beyond 8 (root included), and exhausted 32-child budget per lineage branch before `ctx.run` creates the child. The guard is an engine contract, not a prompt suggestion.

Evaluation contract: `ce-evaluate` is a fresh-context, read-only reviewer stage used inside `repair-until-verified`. That token is one `loop` (`max` from `inputs.max_iterations`, cap 32, `until` `steps_verify_ok == true`): repair, then evaluate, then host `verify`. A red verify continues. Exhaustion fails the run. The evaluator returns structured findings only; its `done` field is advisory. The host `VerifySink` and phase contract own pass/fail, and ship-chain evaluations are persisted as bounded, redacted `EvaluationEvidence` under the parent run. Inspect them with `flywheel artifacts stitch <run_id> --project <p>`; do not treat worker status fields as proof.

**beads-lifecycle** (third token): preferred `workflow compose "<objective>" --parts beads-lifecycle --run --project <p>`; legacy alias `workflow run beads-lifecycle --execute`. Nests ordinary builtins (phase-plan, beads, ce-plan-review, phase-implement, phase-verify) via call_workflow; `inputs.until` early-exits SUCCESS after any phase; SIGNOFF outer-only. Sibling of wave-swarm: after integrate, hand off with `until=integrate`, never merge.

```
catalog + built-ins          your first stop — something already fits
        │  doesn't fit
        ▼
arrive + briefing            the session door — picks the vehicle
        │  decide is the leaf
        │  compose arm
        ▼
compose (parts / from-yaml)  assemble or author a Workflow
        │  lint gate (fail-stop)
        ▼
dry-run → run                preview, then execute (durable engine run)
        │  observe
        ▼
flywheel runs                what happened, per step
        │  same shape twice?
        ▼
promote (workflow new)       it becomes catalog; the next agent finds it
```

The whole point: **improvise deliberately, never twice.** Improvisation is
for shapes the catalog lacks; graduation is how the catalog learns.

---

## 2. The decision procedure (follow in order)

Run the session door first — `flywheel arrive <project> --as <pin>`, then
`flywheel briefing "<goal>" --project <p> --json` — and read the arm it
proposes. `decide "<target>" --json` is the classifier **leaf** inside
briefing (the table lives in `src/decide.rs`), not a second door to skip to.
The arms and what they obligate you to do:

| Arm | Meaning | Your obligation |
| :--- | :--- | :--- |
| find-existing | a catalog/built-in workflow fits | run it; do NOT compose |
| compose-parts | deterministic assembly suffices | compose with `--parts` |
| compose-agent | the shape needs authoring | planner authors YAML; lint fail-stop |
| steer | objective-driven, captain-free driving | `flywheel steer "<objective>"` |
| loop-work | bead queue driving | `flywheel loop-mode run <project> --work` |
| captain-audit | read-only audit of the captain session | run the audit workflow |
| none | no catalog route matched | declare a trigger, or `workflow compose --agent`; never steer anyway |

Hard rules:

- decide said find-existing / steer / loop-work / captain-audit → **composing
  is wrong**, even if you think you could. Force-fitting `goal` onto a
  read-only audit corrupts the audit's contract.
- decide said `none` → running `steer` anyway is wrong. Restate with a
  declared trigger, or author the shape (`compose --agent`).
- Lint is fail-stop. A workflow that does not lint never runs. Never dispatch
  dirty YAML, never "just this once" past a lint error.
- Archetype check before composing: scout-fanout, verify-fence, debate,
  hillclimb, review cover most task shapes (§6). If one fits even loosely,
  prefer it — it carries battle-tested gates.

---

## 3. The shipped surface (verified)

### 3.1 Inspect the catalog (SHIPPED)

```bash
flywheel workflow list                 # name, source (builtin|catalog), trigger, crew
flywheel workflow show <name>          # full definition
flywheel workflow lint                 # ALL workflows; exit 0 clean, 1 blocking, 2 parse error
flywheel workflow lint --only <name>   # one workflow
```

### 3.2 Run a workflow (SHIPPED)

```bash
# dry-run (DEFAULT): control flow executes, side-effects are RECORDED ONLY
flywheel workflow run <name> --inputs '{"goal":"..."}'

# real execution, headless (steps via __run-headless subprocesses)
flywheel workflow run <name> --execute --mode headless

# real execution, pane (pre-spawns a real herdr crew, dispatches via __send)
flywheel workflow run <name> --execute --mode pane --project <project>
```

**The #1 foot-gun:** without `--execute` nothing real happens. Dry-run first
is not optional discipline; it is how you read a workflow's shape before
committing crews to it.

`--mode pane --workspace <id>` binds to an already-running crew instead of
spawning fresh. Use it for repeat work on a live swarm.

### 3.3 Compose (SHIPPED)

```bash
# parts only: lint + preview the inferred crew (no run)
flywheel workflow compose "<target>" --parts "scout:fresh-eyes-explore,coder:ce-execute"

# seed from an existing workflow, then extend
flywheel workflow compose "<target>" --base triage --parts "coder:ce-execute"

# chain a catalog workflow as one part of something bigger
flywheel workflow compose "<target>" --parts "workflow:weekly-review"

# run it: lint gate first, pane mode
flywheel workflow compose "<target>" --parts "..." --run --project <project>

# planner arm: an agent authors the whole workflow from target + catalog
flywheel workflow compose "<target>" --agent
```

Part grammar (today): use an explicit tag when a role or phase name could be
confused with another vocabulary word. `stage:<stage>` selects a stage through
its declared role, `role:<role>:<stage>` pins a profile, and
`phase:<phase>:<stage>` carries a lifecycle phase. The compact forms
`<role>:<stage>` and `<phase>:<stage>`, `workflow:<name>`, and a bare stage
remain valid. `kind:<harness>:<stage>` and `kind:<harness>:<role>:<stage>`
pin that harness on the dispatch. Parts comma-separate. Nested control forms are
`each:<param>:<part>`, `parallel:<param>:<part>`, `when:<guard>:<part>`, and
`retry(n):<part>`. Wrappers fail the run. A false `when` guard skips.

SHIPPED (2026-08-21/22): phase metadata is carried on dispatch
specs (omp `--plan-yolo`, opencode `--agent plan`; unsupported mechanics fail
loudly), stage-first bare-name parts resolve through stage affinity, `--keep
<name>` graduates only after a GREEN run, and `cron run --live` performs the
real dispatch. A compose token is a named workflow fragment that expands at the
part position; it is orthogonal to a stage and a squad.

To make operator-owned copies of the built-in catalog:

```bash
flywheel workflow seed --json
flywheel workflow drift --json
```

`seed` never overwrites a user file. `drift` is the freshness check: `missing`
means no user copy, `current` means byte-equivalent semantics, `stale` means a
user copy shadows a changed builtin, and `custom` means a user-only entry.

SHIPPED: `each:`/`when:`/
`retry(n)`/`parallel:` part forms, `--from-yaml -`, `--dry-run`
first-class, compose budget caps.

### 3.4 Observe (SHIPPED)

```bash
flywheel runs list
flywheel runs show <run-id>
flywheel runs digest
```

Read the run BEFORE claiming anything about it. A step that "should have"
dispatched shows in the run log; your claim about a run you did not read is
noise.

### 3.5 Promote (SHIPPED)

The same composed shape needed twice = it is not improvisation anymore, it
is missing catalog:

```bash
flywheel workflow new <name>    # scaffold a catalog entry
# write the YAML (start from the composed shape), then:
flywheel workflow lint --only <name>
```

SHIPPED (W6): `--keep <name>` automates graduation from a green composed
run with provenance. Promote by hand via `workflow new` when not using `--keep`, and
say so in the bead/commit.

---

## 4. Worked tutorials

### Tutorial A — the catalog already fits (most common)

Task: "weekly review of the swarm's beads and runs."

1. `flywheel workflow list` → `weekly-review` exists (builtin).
2. `flywheel workflow show weekly-review` → check trigger, crew, steps match
   the intent. They do.
3. Dry-run: `flywheel workflow run weekly-review --inputs '{"project":"toron"}'`
   → read the step graph.
4. Execute: add `--execute --mode pane --project toron`.
5. `flywheel runs digest`.

You composed nothing. That is the win.

### Tutorial B — compose from parts

Task: "fresh-eyes exploration of module X, then systematic execution of the
findings." No single catalog workflow does both in sequence.

1. `flywheel briefing "explore then fix module X" --project <p> --json` →
   compose-parts (the `decide` leaf reports the arm).
2. Compose + preview:
   `flywheel workflow compose "explore then fix module X" --parts "scout:fresh-eyes-explore,coder:ce-execute"`
   → read the lint output and the crew preview.
3. Lint issues? Fix the parts (wrong stage name, wrong role) — never the gate.
4. Run: append `--run --project <p>`.
5. `flywheel runs show <id>` — confirm both steps dispatched and landed.
6. Needed again next week? Tutorial C.

### Tutorial C — promote a recurring shape

1. You have now composed the same two-part shape twice.
2. `flywheel workflow new explore-then-fix`.
3. Write the YAML into the scaffolded catalog entry, translating the parts:
   two `dispatch_job` steps (assignee `@scout` / `@coder`, the same stage
   bodies), outputs chained via `{{steps.<name>.output}}`.
4. `flywheel workflow lint --only explore-then-fix` → exit 0.
5. Dry-run once, then it is catalog: future agents hit Tutorial A.

---

## 5. Steering from inside a pane (slash commands)

SHIPPED (2026-08-21 fix — the commands were previously dead in opencode; if
`/swarm-mode` does not expand, run `flywheel integration opencode` to regenerate
into `~/.config/opencode/commands/` — plural):

- `/swarm-mode` — captain-led multi-crew (`workflow compose --parts wave-swarm --run`)
- `/loop-mode` — start/inspect the durable solo loop
- `/open-mode` — current-pane work, no lifecycle
- `/peer-aware-mode` — attach a running pane to the doorbell
- Catalog workflows (`workflow list --brief`) and `br` own bead/workflow verbs;
  `/workflow-*` and `/beads-*` slash files are gone (`flywheel integration`
  deletes leftovers)

Mail steering (any harness): crews ack, read, and reply through the toron
mailbox; replies must land in the thread before you claim a response
(lesson: verify delivery, do not assume).

### 5a. Coordination and steering, by boundary

Use one control plane for each handoff:

- **Captain → crew:** `flywheel send --stage <stage>` carries a bounded brief;
  use `flywheel collab` when the captain needs a blocking answer.
- **Crew → crew:** join the project channel, post a one-line kind-9 update, and
  claim the resulting job. Use `toron channel` for the room and `toron job` for
  the work item. A private fact belongs in `toron mail`, not the room.
- **Captain objective, one lane:** `flywheel steer "<objective>" --project <p>`.
  It owns the objective-to-completion path; do not wrap it in a second compose.
- **Explicit multi-lane shape:** `workflow compose "<objective>" --parts ... --run`
  after briefing proposes it. The compose spec owns ordering, fan-out, and the
  single artifact trail; the crew does not invent a second topology.
- **Current pane, no delegation:** announce `vehicle: none (raw)` and work in
  open mode. A stage or ordinary workflow may run in the pane, but it does not
  silently authorize spawning.

A steering request is complete when the recipient has acknowledged it, the
work item or corr is durable, and the response is read back. “Sent” alone is
not coordination evidence.

## 5b. Principles — the awareness contract (SHIPPED)

The 21 engineering principles are a closed registry.
They are not decoration; two mechanisms consume them:

1. **Workflow YAML**: declare `principles: [name, ...]` on any workflow
   (catalog, promoted, composed). The lint validates every name against
   the registry — a typo'd principle is a lint error, not a silent comment.
2. **The durable loop**: priority beads must cite `name — decision` via
   `check_principles` (durable.rs) — the loop refuses unprincipled
   priority claims with the exact reason string.

Authoring discipline:

```bash
flywheel principles list           # the 21 names — read before authoring
flywheel principles show <name>    # full text of one principle
```

- BEFORE composing or authoring: name the 2-3 principles that govern the
  task (e.g. a migration task → `migrate-callers-then-delete-legacy-apis`,
  `boundary-discipline`; a perf task → `prove-it-works`,
  `redesign-from-first-principles`). Declare them in the workflow.
- A workflow whose behavior contradicts its declared principles is a lint
  smell AND a review finding — reviewers check the declared set first.
- Closing a priority bead on work that violated its cited principle is the
  2026-08-17 mislabel class — the loop's refusal is correct; fix the work
  or the citation, never the check.

Phase affinities (guidance; not yet machine-enforced): resolve →
`foundational-thinking`, `model-the-domain`, `boundary-discipline`; plan →
`sequence-verifiable-units`, `subtract-before-you-add`; implement →
`prove-it-works`, `fix-root-causes`, `make-operations-idempotent`; review →
`exhaust-the-design-space`, `minimize-reader-load`; verify →
`prove-it-works`, `encode-lessons-in-structure`.


## 6. The archetype library (what to reach for)

| Shape | Workflow | Why it wins |
| :--- | :--- | :--- |
| unknown territory, need coverage before committing | scout fan-out (`scout:fresh-eyes-explore`, parallel) | N cheap readers beat 1 expensive reader |
| risky change, want a tripwire | verify-fence pattern | explicit probe steps with `on_error: fail_run` |
| contested decision | debate | N rounds + judge verdict; requires reply verification |
| measured improvement loop | hillclimb | `loop` capped by `inputs.max_iters` (default 10, engine cap 32). Stops only when the reply JSON has `improved: true`. No such field spends the cap, then the run fails. Keep is a ledger row plus one commit. Discard is a revert. |
| quality gate before merge/ship | review workflows | adversarial findings + verdict, n≥2 reviewers |

When composing, START from an archetype and modify, rather than authoring
control flow from zero. Your diff against a battle-tested shape is reviewable;
a from-zero graph is not.

---

## 7. Phases

Every dispatch may carry a typed phase
(`resolve|plan|implement|review|verify|compound`); the harness applies the
available phase mechanic and reports an unsupported mechanic loudly. The
host `VerifySink` and phase evidence own completion, never a worker’s `done`
claim.

---

## 8. Discipline (violations are how swarms die)

1. **Lint is fail-stop.** No exceptions, no "temporary".
2. **Dry-run before execute.** Always. Even the third time.
3. **Read the run before claiming outcome.** Evidence-based done.
4. **No live-run mutation.** A running workflow is never edited mid-run
   (breaks durable replay). Abort → compose v2 seeded from v1 → run v2.
5. **Budget your fan-outs.** The A7 semaphore bounds in-flight per agent; a
   composed `parallel` that floods one role queues, it does not parallelize.
6. **Blast radius or bust.** A composed workflow you cannot say which paths
   it touches is not ready to run.
7. **Promote, don't repeat.** Second improvisation of the same shape is a
   catalog gap — file it (bead) and promote.
8. **`--agent` authoring bails on garbage YAML.** So should you: never
   hand-edit past a parse error; regenerate.

---

## 9. Troubleshooting

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| run "succeeded" but nothing happened | dry-run default (no `--execute`) | re-run with `--execute` |
| compose reports unknown stage/role | typo'd part | `flywheel workflow list` / stage library names are exact |
| `agent_not_ready` during pane dispatch | crew still booting | transient — the runner retries with backoff; persistent = check the pane (herdr agent get) |
| step recorded, no dispatch | recording runner (dry-run) or headless mode without execute | use `--execute`; check `--mode` |
| slash command not expanding (opencode) | dead `command/` dir (pre-fix) or stale files | `flywheel integration opencode`, check `commands/` (plural) |
| lint exit 2 | YAML parse failure | fix syntax; never bypass |
| lint exit 1 | blocking playbook rule | read the rule id; comply (e.g. feature needs `data_shape`) |
| prompt to a blocked agent refused (`agent_blocked`) | herdr 0.8.2 refuses prompts into approval dialogs | resolve the dialog (escalation tiers will mail you) — do not retry-loop the prompt |

---

## 10. Quick reference

```bash
flywheel workflow list | show <n> | lint [--only <n>]
flywheel workflow run <n> [--execute --mode headless|pane --project <p>] [--inputs '<json>']
flywheel workflow compose "<target>" [--base <wf>] [--parts a,b] [--agent] [--run --project <p>]
flywheel workflow seed [--json]       # copy built-ins into the user tier, never overwrite
flywheel workflow drift [--json]      # report missing/current/stale/custom copies
flywheel workflow new <n>
flywheel runs list | show <id> | digest
flywheel arrive <project> --as <pin>
flywheel briefing "<goal>" --project <p> --json   # decide is the leaf inside
flywheel steer "<objective>" [--direct]
flywheel loop run <project> --work
```

SHIPPED: `phase:` in specs; `stage {list,show,new,lint,promote}` + stage-first parts;
`--keep` graduation; `cron run --live`; delegated compose via mail (W7
allowlist-gated); `each:`/`when:`/`retry(n)`/`parallel:` parts; `--from-yaml -`;
`--dry-run`; budget caps. Known deviations: an open String phase and no R-P lint; the engine is
in-memory, so there is no cross-process crash resume yet.

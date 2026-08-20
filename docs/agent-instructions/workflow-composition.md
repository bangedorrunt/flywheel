# Workflow Composition — the Agent's Guide to Optimizing Flywheel Work

<!-- governed-by: ADR-0022 (proposed), ADR-0005, ADR-0010, ADR-0019 -->
<!--
Agent-facing tutorial. SHIPPED = works today at this commit. PROPOSED =
ADR-0022 wave not yet implemented — do NOT attempt these flags; the command
will reject them. Every command block below marked SHIPPED was verified
against the CLI surface.
-->

You are a captain or crew agent driving flywheel. This document teaches you
how to get the most out of the workflow system: when to run what exists,
when to compose something new, how to do it safely, and how to promote what
works into the catalog so nobody improvises the same thing twice.

Companion skill: `runtime-adaptation` (the compose arm, lean version of §3).
Design doc: toron `docs/decisions/0022-phase-native-orchestration.md`.

---

## 1. The mental model (60 seconds)

```
catalog + built-ins          your first stop — something already fits
        │  doesn't fit
        ▼
flywheel decide              classifies the task shape into an arm
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

Run `flywheel decide` (or reason it through if you cannot shell out). The
arms and what they obligate you to do:

| Arm | Meaning | Your obligation |
| :--- | :--- | :--- |
| find-existing | a catalog/built-in workflow fits | run it; do NOT compose |
| compose-parts | deterministic assembly suffices | compose with `--parts` |
| compose-agent | the shape needs authoring | planner authors YAML; lint fail-stop |
| steer | objective-driven, captain-free driving | `flywheel steer "<objective>"` |
| loop-work | bead queue driving | `flywheel loop run <project> --work` |
| captain-audit | read-only audit of the captain session | run the audit workflow |

Hard rules:

- decide said find-existing / steer / loop-work / captain-audit → **composing
  is wrong**, even if you think you could. Force-fitting `goal` onto a
  read-only audit corrupts the audit's contract.
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
flywheel workflow compose "<target>" --parts "scout:fresh-eyes-explore,coder:systematic-execute"

# seed from an existing workflow, then extend
flywheel workflow compose "<target>" --base triage --parts "coder:systematic-execute"

# chain a catalog workflow as one part of something bigger
flywheel workflow compose "<target>" --parts "workflow:weekly-review"

# run it: lint gate first, pane mode
flywheel workflow compose "<target>" --parts "..." --run --project <project>

# planner arm: an agent authors the whole workflow from target + catalog
flywheel workflow compose "<target>" --agent
```

Part grammar (today): `role:stage` → dispatch_job to `@role` running the
named stage; `workflow:name` → `call_workflow`. Parts comma-separate.

PROPOSED (ADR-0022 W3, do not use yet): `phase:stage` parts,
`each:`/`when:`/`retry(n)`/`parallel:` part forms, `--from-yaml -`,
`--dry-run` first-class, `--keep <name>` graduation.

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

PROPOSED (W6): `--keep <name>` automates graduation from a green composed
run with provenance. Until then, promote by hand via `workflow new`, and
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

1. `flywheel decide` → compose-parts.
2. Compose + preview:
   `flywheel workflow compose "explore then fix module X" --parts "scout:fresh-eyes-explore,coder:systematic-execute"`
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
`/swarm` does not expand, run `flywheel integration opencode` to regenerate
into `~/.config/opencode/commands/` — plural):

- `/swarm` — spawn/steer a crew
- `/loop` — start/inspect the durable loop
- `/workflow-*` — brainstorm, plan, plan-review, debate, triage, onboard,
  captain-audit
- `/beads-*` — triage, execute, review, commit, create

Mail steering (any harness): crews ack, read, and reply through the toron
mailbox; replies must land in the thread before you claim a response
(bd-debate-reply-routing lesson: verify delivery, do not assume).

---

## 6. The archetype library (what to reach for)

| Shape | Workflow | Why it wins |
| :--- | :--- | :--- |
| unknown territory, need coverage before committing | scout fan-out (`scout:fresh-eyes-explore`, parallel) | N cheap readers beat 1 expensive reader |
| risky change, want a tripwire | verify-fence pattern | explicit probe steps with `on_error: fail_run` |
| contested decision | debate | N rounds + judge verdict; requires reply verification |
| measured improvement loop | hillclimb | capped retry: hypothesize → smallest change → measure → keep (ledger + commit) or discard (revert) |
| quality gate before merge/ship | review workflows | adversarial findings + verdict, n≥2 reviewers |

When composing, START from an archetype and modify, rather than authoring
control flow from zero. Your diff against a battle-tested shape is reviewable;
a from-zero graph is not.

---

## 7. Phases (ADR-0022 — how this changes, PROPOSED)

Once W1–W5 land: every dispatch carries a typed `phase`
(`resolve|plan|implement|review|verify`); the harness enforces phase
mechanics (plan-mode flags); completion is evidence, not claims — plan ends
when beads with acceptance exist, implement ends when a commit cites the bead
and its gate exits 0. Until then: emulate the discipline manually. Post your
goal statement before planning; create beads with acceptance before coding;
cite bead ids in commits; post review verdicts. The ADR will mechanize what
you practiced.

---

## 8. Discipline (violations are how swarms die)

1. **Lint is fail-stop.** No exceptions, no "temporary".
2. **Dry-run before execute.** Always. Even the third time.
3. **Read the run before claiming outcome.** Evidence-based done (ADR-0019).
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
flywheel workflow new <n>
flywheel runs list | show <id> | digest
flywheel decide
flywheel steer "<objective>" [--direct]
flywheel loop run <project> --work
```

PROPOSED (ADR-0022, waves W1–W7): `phase:` in specs, `phase:stage` parts,
`each:/when:/retry(n)/parallel:` parts, `--from-yaml -`, `--dry-run`
first-class, `--keep` graduation, delegated compose via mail. Watch the
ADR's Verification section for the ship markers before using them.

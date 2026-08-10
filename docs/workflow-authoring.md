# Workflow, Cron, Recipe & Profile Authoring Guide

> **Audience:** agents and captains driving flywheel. This is the practical "how to" for the
> orchestration vocabulary: **create, list, select, execute, remove**. Governed by
> [ADR-0005](../../toron/docs/decisions/0005-orchestration-layer.md) (the tier) and
> [ADR-0010](../../toron/docs/decisions/0010-execution-wiring-model-registry-headless.md)
> (execution wiring + model registry + headless).
>
> **One-line mental model:** a **profile** is a role template (behavior + model + harness); a
> **recipe/squad** is a group of profiles; a **workflow** is a multi-step procedure that uses
> profiles; a **cron** is a workflow on a schedule. You compose them; flywheel executes.

---

## TL;DR — the five verbs

| Verb | Workflow | Cron | Recipe/Squad | Profile |
|---|---|---|---|---|
| **create** | `flywheel workflow new <name>` + edit YAML | `flywheel cron add <name> --template <wf> --cron "30 3 * * 0" --owner housekeeper` | add `[recipes.<name>]` to `~/.config/flywheel/recipes.toml` | `flywheel profile create <name> --harness ... --model ... --behavior ...` |
| **list** | `flywheel workflow list` | `flywheel cron list` | `flywheel squad list` | `flywheel profile list` |
| **show** | `flywheel workflow show <name>` | `flywheel cron show <name>` | `flywheel squad show <name>` | `flywheel profile show <name>` |
| **execute** | `flywheel workflow run <name> --execute [--inputs '{}'] [--crew auto]` | `flywheel cron start` (runs the registry) | `flywheel squad spawn <name>` | `flywheel profile run <name> [--workflow <wf>]` |
| **remove** | `flywheel workflow remove <name>` | `flywheel cron remove <name>` | `flywheel squad remove <name>` | `flywheel profile remove <name>` |

Every command takes `--json` for agent drivers. `flywheel workflow lint` is the fail-stop gate —
a workflow with blocking lint issues REFUSES to run.

---

## 1. Profiles — the role template

A profile layers **spawn intent** (harness + model) and **role discipline** (behavior preamble)
on a kind-30013 identity. The six built-ins are compiled-in; you add domain roles locally.

```bash
# List the built-ins + any locals
flywheel profile list

# Show one (full behavior preamble + skills)
flywheel profile show planner

# Create a domain role
flywheel profile create hachiko-researcher \
  --harness pi \
  --model claude-sonnet-4 \
  --role researcher \
  --behavior "You are a HACHIKO RESEARCHER. Run experiments, log Sharpe/PBO honestly, never claim a result you didn't measure. ..." \
  --task-hint "strategy validation experiments"

# Edit (same fields; alias of create)
flywheel profile edit hachiko-researcher --model claude-opus-4

# Spawn one agent from a profile + optionally invoke a workflow
flywheel profile run hachiko-researcher --workflow validate-strategy --inputs '{"asset":"BTC"}'

# Remove
flywheel profile remove hachiko-researcher
```

**The six built-in profiles** (ADR-0005 Spec 1b): `oracle`, `planner`, `reviewer`, `coder`,
`scout`, `housekeeper`. Each has a tuned `behavior` preamble (200+ words) + `model` + `skills`.
`housekeeper` is the cron owner (idempotent, health-gated, honest reporting).

**Default model map (2026-08-10):** all six profiles run **`omp`** by default. Per-role:
`oracle` = `opencode-go/glm-5.2` (high/max effort; fallback to the grok harness's premium model on rate-limit),
`planner`/`coder`/`reviewer`/`housekeeper` = `deepseek/deepseek-v4-flash`,
`scout` = `opencode/big-pickle`. Registered in `~/.config/flywheel/models.toml`. `reviewer` runs on
**opencode** in review/debate workflows (a per-step harness override — cross-harness review independence);
grok/opencode are otherwise used only when a profile/workflow explicitly asks.

**Key fields:**
- `harness` — `pi | oc | grok | omp | ...` (omp is a first-class herdr kind; `herdr agent start --kind omp` works).
- `model` — resolved against `~/.config/flywheel/models.toml` (ADR-0010 Spec D); injected into the spawn via the harness's model flag.
- `behavior` — the system-prompt preamble. **Prepended to every dispatched prompt** (ADR-0010 Spec C) — this is how the role "takes effect."
- `role` — free-form label, NOT access control. Prompt-shaping only.
- `exec` *(ADR-0010)* — `pane` (default; you observe) or `headless` (autonomous; logged; no pane).

> **Harnesses:** `oc`/`pi`/`gk` are the legacy shorts (opencode/pi/grok). `omp` spawns directly
> (`herdr agent start --kind omp`). `pa` (prime-agent) is NOT yet a herdr kind — defer until
> herdr lists it. Custom harnesses via `[harness.<name>] spawn_cmd` in `profiles.toml`.

---

## 2. Recipes / Squads — a group of profiles

A **squad** is a named `{profile: count}` map. The legacy `Recipe{oc,pi,gk}` is a squad alias
(`flywheel recipes list` still works).

```bash
# List compiled-in + recipes.toml aliases
flywheel squad list

# Show a squad's members
flywheel squad show feature-team      # {planner: 1, coder: 2, reviewer: 1, scout: 1}

# Spawn the whole squad
flywheel squad spawn feature-team --project myproj --json

# Define one in ~/.config/flywheel/recipes.toml:
#   [recipes.feature-team]
#   planner = 1
#   coder = 2
#   reviewer = 1
#   scout = 1
# (legacy form also works: oc = 2, pi = 1, gk = 1)

# Remove
flywheel squad remove feature-team
```

A squad is the **crew** a workflow runs across. `crew: {planner: 1, coder: 2}` in a workflow
IS an inline squad; `crew: auto` infers the squad from the workflow's own `profile:` refs.

---

## 3. Workflows — multi-step procedures

A workflow = name + optional trigger + guard + steps (linear) or graph (DAG) + inputs + crew.
One YAML document; multi-doc streams supported. `flywheel workflow new <name>` scaffolds one.

### 3a. BASIC — a linear workflow (the 80% case)

```bash
flywheel workflow new greet
# writes ~/.config/flywheel/workflows/greet.yaml — edit it:
```
```yaml
name: greet
description: "send a hello to the operator"
trigger:
  type: webhook          # invoke-only if you omit the trigger
inputs:
  - name: who
steps:
  - name: say
    action: send_dm
    to: operator
    body: "hello {{inputs.who}}"
```
```bash
flywheel workflow lint greet        # fail-stop gate (must pass)
flywheel workflow run greet --inputs '{"who":"world"}' --execute
flywheel workflow history greet     # past runs
flywheel workflow remove greet
```

### 3b. IN-DEPTH — crew + roles + model + mode

A real feature workflow: a scout scopes, a planner decomposes, coders build in parallel, a
reviewer checks. Profiles + crew + `await` + the role preambles (Spec C) + model injection
(Spec D).

```yaml
name: ship-feature
description: "scope → plan → build (parallel) → review"
inputs:
  - name: feature
crew: { scout: 1, planner: 1, coder: 2, reviewer: 1 }   # the squad
steps:
  - name: scope
    action: spawn
    profile: "@scout"
    stage: fresh-eyes-explore        # named stage (Spec 2a) — reuses the p1 prompt
    await: true                      # block until the scout finishes
  - name: plan
    action: spawn
    profile: "@planner"
    inputs: { goal: "{{inputs.feature}}, context: {{steps.scope}}" }
    await: true
  - name: build
    action: parallel
    over: "@crew.coder"              # fan out across the 2 coders
    each: { action: spawn, stage: systematic-execute, await: true }
  - name: review
    action: spawn
    profile: "@reviewer"
    inputs: { diff: "{{steps.build}}" }
    await: true
```
```bash
flywheel workflow lint ship-feature
flywheel workflow crew ship-feature     # preview the auto/inferred crew
flywheel workflow run ship-feature --execute --inputs '{"feature":"add X"}' --crew auto
```

**What happens on `--execute`** (ADR-0010 Spec A–C): flywheel pre-spawns the crew
(`crew → cmd_spawn → herdr agent_start`), injecting each profile's `model` via the harness flag.
Each `spawn` step's prompt is **prepended with the profile's `behavior` preamble** and dispatched
to the bound pane. `parallel over: "@crew.coder"` fans the build across both coders; results
collect into `{{steps.build}}`. Without `--execute`: the no-op runner (control-flow only).

### 3c. DAG (graph:) — when steps aren't strictly linear

```yaml
name: recon-and-fix
crew: auto
graph:
  nodes:
    scout:  { action: spawn, profile: "@scout",   await: true }
    plan:   { action: spawn, profile: "@planner", await: true, needs: [scout] }
    fix_a:  { action: spawn, profile: "@coder",   await: true, needs: [plan] }
    fix_b:  { action: spawn, profile: "@coder",   await: true, needs: [plan] }
    ship:   { action: call_workflow, workflow: merge-best, needs: [fix_a, fix_b] }
```
`needs:` defines edges (single encoding — no separate `edges:`). Independent nodes run
concurrently. Lint rejects cyclic graphs.

### 3d. All step actions + triggers (reference)

**Triggers:** `schedule`, `webhook`, `message_posted`, `reaction_added`,
`reservation_acquired`, `reservation_released`, `agent_state_changed`.

**Actions:** `send_message`, `send_dm`, `react`, `dispatch_job`, `spawn`, `call_webhook`,
`request_approval`, `delay`, `call_workflow`, `parallel`, `branch`, `retry`, `foreach`.

**Step policies:** `on_error: continue` (default; backward compat) | `fail_run` (the step's
error FAILS the run → kind-46012 `status=failed`, the signal smart-cron consumes). `await: true`
(block for the agent's result); `await: false` (fire-and-forget; headless logs later).

**Templating:** `{{inputs.x}}`, `{{trigger.x}}`, `{{steps.<name>}}` (a step's returned Value).
Guards are `evalexpr` expressions (100ms timeout).

### 3e. Built-in library + slash commands

**Built-in workflows (2026-08-10):** `brainstorm` · `plan` · `beads` · `debate` · `goal` ·
`triage` + **`onboard`** (cold-start: read AGENTS.md + map structure + seed context + first task) +
**`triage-deep`** (bv robot-triage → prioritize → assign → spawn) + **`weekly-review`** (the weekly
self-improvement cron: scout-fanout → oracle → ONE follow-up bead; see §4). Zero-config:
`flywheel workflow run debate --inputs '{"proposal":"..."}'` works on a fresh install.

**Slash commands (2026-08-10):** the bead-workflow lifecycle is delivered as slash commands —
`/beads-triage`, `/beads-execute`, `/beads-review`, `/beads-commit`, `/beads-create`
(`prompts/beads-*.md` per tool: opencode/pi/grok/omp), generated by `flywheel integration <kind>`
from the shared body. The former `beads` SUBAGENT is retired; the loop's dispatch prompt (swarm
path) + these slash commands (interactive path) are the two deliveries.

---

## 4. Cron — a workflow on a schedule

Smart cron (ADR-0005 Spec 3 + ADR-0010 Spec E): a schedule owned by a profile that reports
failure → bead → escalation. Cron defaults to **headless** (no workspace churn; structured logs).

```bash
# Register a cron bound to a workflow + owner profile
flywheel cron add weekly-docker-upgrade \
  --template docker-upgrade \
  --cron "30 3 * * 0" \
  --owner housekeeper \
  --scope host \
  --mode headless          # ADR-0010: headless (default) | pane

# Inspect + control
flywheel cron list --json
flywheel cron show weekly-docker-upgrade
flywheel cron status
flywheel cron pause weekly-docker-upgrade
flywheel cron resume weekly-docker-upgrade
flywheel cron history weekly-docker-upgrade

# Start the durable cron manager (Resonate durable-sleep loop; blocks)
flywheel cron start

# Remove
flywheel cron remove weekly-docker-upgrade
```

**The self-reporting loop:** schedule due → ensure the owner profile's agent →
`WorkflowEngine::invoke` → on SUCCESS `log`; on FAILURE → `br create` (bead with the failure
bundle) → defined-fixer workflow (or human-in-loop) → repeat-failure ≥ threshold → escalate
(kind-14 DM to captain). The scheduler is a Resonate durable-sleep loop — survives a flywheel
restart with `latest` catch-up.

**Headless cron** (ADR-0010 Spec E): each tick runs the owner harness one-shot
(`<harness> -p --mode=json --model=X "<behavior + step prompt>"`), captures JSON →
`~/.local/state/flywheel/runs/<run_id>.jsonl`. No panes, no workspace churn. Non-durable
(idempotent re-run on crash). Local-only (not on the federated bus). Track via
`flywheel runs`/`digest`.

---

## 5. Models — the registry (ADR-0010 Spec D)

```bash
flywheel model list                       # the registry
flywheel model show claude-sonnet-4
flywheel model add <id> --provider anthropic --cost-in 3 --cost-out 15 --caps "code,reason"
flywheel model remove <id>
```
`Profile.model` resolves against this registry; the resolved model is injected at spawn via the
harness's flag (omp `--model=`, opencode `-m`, pi `--model`, grok `-m`). Unknown model → warning
+ pass-through (never blocks spawn). Per-step override: `workflow run --model <id>`.

---

## 6. Observability — what's running / failed

```bash
flywheel runs                  # all runs: state + owner + per-run model/token cost
flywheel runs show <run-id>    # full step trace / outputs / headless response
flywheel runs pending          # parked on approval / spawn-await
flywheel runs digest --since 7d --broken-after 3   # failures + parked + broken>N
```
Headless run logs (`runs/*.jsonl`) feed this view; pane runs feed from the project event log.

---

## Decision cheatsheet — which vocabulary do I need?

| You want to... | Use |
|---|---|
| run one agent on one task, watch it | `flywheel profile run <role>` or `flywheel send` |
| run a fixed daily stage sequence on a live swarm | `flywheel loop run <project> --work` (the legacy imperative path; still primary for live swarms) |
| run a reusable multi-step procedure once | `flywheel workflow run <name> --execute` |
| run that procedure on a schedule, unattended | `flywheel cron add ...` + `flywheel cron start` |
| spawn a fixed team shape repeatedly | define a recipe/squad → `flywheel squad spawn <name>` |
| fan out N agents on one step | `parallel over: "@crew.<role>"` in a workflow |
| add a new role/persona | `flywheel profile create <name> ...` |
| pick a model per role | set `model` on the profile (+ `flywheel model add` to the registry) |

> **Status note (ADR-0010):** the `--execute` real runner, `--mode headless`, model injection,
> and the `remove`/`model` commands land in the P10.1–P10.5 beads. Until P10.3 lands,
> `workflow run` (no `--execute`) is the no-op control-flow-only path; the **live swarm path
> remains `flywheel loop run <project> --work`**. This guide documents the target surface so
> agents author against it; the ADR's verification gates mark each command live as its bead
> closes.

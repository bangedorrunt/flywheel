# Orchestration

ADR-0005 + 0010 + 0011 + 0012. Authoring: [docs/workflow-authoring.md](../workflow-authoring.md).

| Noun | Command |
|------|---------|
| profile | `flywheel profile {list,show,create,edit,run,remove}` — 6 built-ins (oracle/planner/reviewer/coder/scout/housekeeper) |
| squad | `flywheel squad {list,show,spawn,remove}` (`recipes list` alias) |
| workflow | `flywheel workflow {list,show,lint,run,new,history,crew,compose,remove}` |
| cron | `flywheel cron {lint,run,list,show,pause,resume,status,history,start,remove}` — distinct YAML type |
| model | `flywheel model {list,show,add,remove}` |
| runs | `flywheel runs {list,show,pending,digest}` |
| coalition | `flywheel coalition {form,status,disband}` — pane-crew-only |
| steer | `flywheel steer "<target>" [--direct] [--project p]` |
| open mode | `/open-mode`: direct work in the current pane; no crew, durable loop, or swarm lifecycle |
| loop mode | `/loop-mode` / `flywheel loop-mode run`: durable solo execution in one current harness; `--work` opts into tracked work |
| swarm mode | `/swarm-mode`: captain-led multi-crew execution with parallel lanes; loop supervision is optional |
| doorbell | workspace `__doorbell` (ADR-0005 Layer 0). Spawn starts it. `flywheel kill` stops it. `/peer-aware-mode` attaches. Shared by loop-mode and swarm-mode when enabled. |

## Captain selector

Run `flywheel decide "<target>" --json` (pure classifier in `src/decide.rs`). Run the returned `argv`. Do not re-derive the table in prose.

| primitive | argv |
|-----------|------|
| captain-audit | `workflow run captain-audit --execute --mode headless` |
| loop-work | `loop-mode run <project> --work` |
| steer / steer-direct | `steer` / `steer --direct` |
| compose-parts / compose-agent | `workflow compose --parts` / `--agent` |
| coalition-form | `coalition form` |
| spawn-send | `spawn --profile` (never `add --oc`) |
| cron-start | `cron start` |

`workflow run` without `--execute` records control-flow only (no pane dispatch). Real work needs `--execute`. `--mode` default is **headless**. `steer` always `--execute --mode pane`. `/swarm-mode` pins `compose --parts wave-swarm --run` (do not re-run decide). `/loop-mode` pins `loop-mode run <project> --work`.

## Operating topologies

These are not interchangeable with workflow definitions:

- **open**: immediate work in the current harness; no orchestration lifecycle.
- **loop**: one current harness drives one objective durably. A queue or named
  workflow may be the work source. `--work` selects the bead-backed variant;
  beads are not required for loop mode generally.
- **swarm**: a captain coordinates multiple crews/panes and parallel lanes.
  It may use `loop-mode --work` for bead supervision, but peer wake does not
  depend on the loop.

A **workflow** is a reusable engine procedure. It can run inside a loop,
inside a swarm, headlessly, or directly; it is not itself an execution
topology.

A **stage** is one prompt (`send --stage`). A **compose token**
(`wave-swarm`, `repair-until-verified`) expands to a compiled-in
workflow when used as a bare `--parts` name. A **profile** is role →
harness/model; `crew: auto` counts `@roles`. `flywheel squad` /
`recipes list` is a group of profiles (spawn shape) — say "squad," not
"compose token."

**What a compose token is:** a bare name legal in
`workflow compose … --parts <token> --run` that expands to a compiled-in
workflow's steps, inline, at that position — one engine run, one artifact
trail. Preferred door: `workflow compose … --parts <token> --run`.
Legacy alias for compose tokens only: `workflow run <token> --execute`.
Ordinary builtins (e.g. captain-audit): `workflow run <name> --execute`,
NOT bare `--parts`. Other `--parts` forms: bare stage name, `role:stage`,
`phase:stage`, `workflow:name`, and the grammar forms (`each:`, `when:`,
`retry(n):`, `parallel:`). Stages referenced by spliced steps resolve
from the stage catalog (`stage list`).

## Execution modes (ADR-0010)

- **pane**: pre-spawn crew, `__send --confirm`, bus-auditable. `dispatch_job` live. `action: spawn` drains `SpawnSink` into `agent_start`.
- **headless**: `__run-headless` one-shot, logs `runs/<id>.jsonl`. Spawn must not fake 43001/43004.

## Profiles

All six built-ins default harness **omp**. Per-role models live in `profiles.toml` / `models.toml` — registry keys are composite `provider/model` (e.g. `[models."opencode-go/deepseek-v4-flash"]`); `Profile.provider` pins the composite key, bare ids fall back to the default provider (user file entries first, else alphabetically-first). Bare harness+model `spawn --model <id> [--provider <p>]` (bd-j3so) is the lenient fallback for kind/count spawns without a named profile (registered ids route via composite, unknown ids pass through verbatim; `--provider` requires `--model`). `spawn --profile` writes the spawn-registry (behavior + skills). `profile run` / `squad spawn` use `profile_launch_args` (model/effort) and a **pane id**, never a workspace id.

## Captain briefing (bd-xe9r/bd-nmzk)

Interactive TTY `steer`, `workflow run --execute`, and managed mode commands prompt before dispatch — inferred crew counts (editable), per-role model menus w/ cost/caps from the registry, squad/bare-harness shape, stage-chain preview, go/no-go. Skipped by `--json`, `--yes`, or an explicit `--crew`; non-TTY dispatch is byte-identical. Shared prompt module `src/steer_brief.rs` (`source` fork only, `workflow_briefing_context` extracts `objective`/`goal`/`target` from `--inputs`); prompt logic is pure over injected IO.

## Hidden leaves

Never invoke `__*` except documented `__wait-output`.

## Which noun when (strategy surface, ADR-0028 E7)

| noun | is | choose it when | trail |
| --- | --- | --- | --- |
| **stage** | a payload unit (prompt + role/phase affinity) dispatched by `send --stage` | the INSTRUCTIONS are the value; one step, one crew member | `flywheel timeline` shows `stage=` per send |
| **workflow** | an execution procedure (steps/branches/verify) run by `workflow run/compose` | the CONTROL FLOW is the value: gates, retries, parallel arms, SIGNOFF | compose runs write artifacts |
| **compose token** | bare `--parts` name (`wave-swarm`, `repair-until-verified`); preferred `workflow compose … --parts <token> --run`; legacy `workflow run <token> --execute` | the CONTROL FLOW is the value and repeats across objectives | the composed run's artifacts |
| **profile** | role conditioning (harness/model/effort/behavior) referenced by stage/workflow/compose-token | always — model routing is strict; compose tokens/stages name profiles | model registry |

Announce the vehicle on every dispatch (stage / workflow / compose token / `none: raw`) — crews restate it in reports (2026-09-04 contract).

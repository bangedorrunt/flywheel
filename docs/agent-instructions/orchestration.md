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
| doorbell | workspace `__doorbell` (ADR-0005 Layer 0). Spawn starts it. `flywheel kill` stops it. `/peer-aware` attaches. Shared by loop and swarm when enabled. |

## Captain selector

Run `flywheel decide "<target>" --json` (pure classifier in `src/decide.rs`). Run the returned `argv`. Do not re-derive the table in prose.

| primitive | argv |
|-----------|------|
| captain-audit | `workflow run captain-audit --execute --mode headless` |
| loop-solo | `loop-mode run <project>` |
| loop-work | `loop-mode run <project> --work` |
| open | current pane / `/open` |
| steer / steer-direct | `steer` / `steer --direct` |
| compose-parts / compose-agent | `workflow compose --parts` / `--agent` |
| coalition-form | `coalition form` |
| spawn-send | `spawn --profile` (never `add --oc`) |
| cron-start | `cron start` |

`workflow run` without `--execute` is a no-op. `--mode` default is **headless**. `steer` always `--execute --mode pane`.

## Operating topologies

These are not interchangeable with workflow definitions:

- **open**: immediate work in the current harness; no orchestration lifecycle.
- **loop**: one current harness drives one objective durably. A queue or named
  workflow may be the work source. `--work` selects the bead-backed variant;
  beads are not required for loop mode generally.
- **swarm**: a captain coordinates multiple crews/panes and parallel lanes.
  It may use `loop-mode --work` for bead supervision, but peer wake does not
  depend on the loop.

A **workflow** is a reusable multi-step procedure. It can run inside a loop,
inside a swarm, headlessly, or directly; it is not itself an execution
 topology.

## Execution modes (ADR-0010)

- **pane**: pre-spawn crew, `__send --confirm`, bus-auditable. `dispatch_job` live. `action: spawn` drains `SpawnSink` into `agent_start`.
- **headless**: `__run-headless` one-shot, logs `runs/<id>.jsonl`. Spawn must not fake 43001/43004.

## Profiles

All six built-ins default harness **omp**. Per-role models live in `profiles.toml` / `models.toml` — registry keys are composite `provider/model` (e.g. `[models."opencode-go/deepseek-v4-flash"]`); `Profile.provider` pins the composite key, bare ids fall back to the default provider (user file entries first, else alphabetically-first). Bare harness+model `spawn --model <id> [--provider <p>]` (bd-j3so) is the lenient fallback for kind/count spawns without a named profile (registered ids route via composite, unknown ids pass through verbatim; `--provider` requires `--model`). `spawn --profile` writes the spawn-registry (behavior + skills). `profile run` / `squad spawn` use `profile_launch_args` (model/effort) and a **pane id**, never a workspace id.

## Captain briefing (bd-xe9r/bd-nmzk)

Interactive TTY `steer`, `workflow run --execute`, and managed mode commands prompt before dispatch — inferred crew counts (editable), per-role model menus w/ cost/caps from the registry, squad/bare-harness shape, stage-chain preview, go/no-go. Skipped by `--json`, `--yes`, or an explicit `--crew`; non-TTY dispatch is byte-identical. Shared prompt module `src/steer_brief.rs` (`source` fork only, `workflow_briefing_context` extracts `objective`/`goal`/`target` from `--inputs`); prompt logic is pure over injected IO.

## Hidden leaves

Never invoke `__*` except documented `__wait-output`.

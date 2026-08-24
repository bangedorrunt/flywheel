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
| doorbell | workspace `__doorbell` (ADR-0005 Layer 0). Spawn starts it. `flywheel kill` stops it. `/peer-aware` attaches. |

## Captain selector

Run `flywheel decide "<target>" --json` (pure classifier in `src/decide.rs`). Run the returned `argv`. Do not re-derive the table in prose.

| primitive | argv |
|-----------|------|
| captain-audit | `workflow run captain-audit --execute --mode headless` |
| loop-work | `loop run --work` |
| steer / steer-direct | `steer` / `steer --direct` |
| compose-parts / compose-agent | `workflow compose --parts` / `--agent` |
| coalition-form | `coalition form` |
| spawn-send | `spawn --profile` (never `add --oc`) |
| cron-start | `cron start` |

`workflow run` without `--execute` is a no-op. `--mode` default is **headless**. `steer` always `--execute --mode pane`.

## Execution modes (ADR-0010)

- **pane**: pre-spawn crew, `__send --confirm`, bus-auditable. `dispatch_job` live. `action: spawn` drains `SpawnSink` into `agent_start`.
- **headless**: `__run-headless` one-shot, logs `runs/<id>.jsonl`. Spawn must not fake 43001/43004.

## Profiles

All six built-ins default harness **omp**. Per-role models live in `profiles.toml` / `models.toml` — registry keys are composite `provider/model` (e.g. `[models."opencode-go/deepseek-v4-flash"]`); `Profile.provider` pins the composite key, bare ids fall back to the default provider (user file entries first, else alphabetically-first). Bare harness+model `spawn --model <id> [--provider <p>]` (bd-j3so) is the lenient fallback for kind/count spawns without a named profile (registered ids route via composite, unknown ids pass through verbatim; `--provider` requires `--model`). `spawn --profile` writes the spawn-registry (behavior + skills). `profile run` / `squad spawn` use `profile_launch_args` (model/effort) and a **pane id**, never a workspace id.

## Captain briefing (bd-xe9r/bd-nmzk)

Interactive TTY `steer` and `workflow run --execute` prompt before dispatch — inferred crew counts (editable), per-role model menus w/ cost/caps from the registry, squad/bare-harness shape, stage-chain preview, go/no-go. Skipped by `--json`, `--yes`, or an explicit `--crew`; non-TTY dispatch is byte-identical. Shared prompt module `src/steer_brief.rs` (`source` fork only, `workflow_briefing_context` extracts `objective`/`goal`/`target` from `--inputs`); prompt logic is pure over injected IO.

## Hidden leaves

Never invoke `__*` except documented `__wait-output`.

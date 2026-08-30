# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] — 2026-08-29/30 orchestration campaign (ADR-0024/0025/0026)

### ✨ Added

- **Engine kind-aware `fail_run` (ADR-0024 Spec 1)** — `rate_limit`/`timeout` retry the same
  node with backoff (bounded by the enclosing `retry`, default 3) before escalating;
  `auth`/`blocked`/`cancelled` abort. R-E1/E2/E3 lint rules fire at compose time.
- **RECIPE compose expansion keeps `graph`** — `--parts repair-until-verified` yields the full
  3-cycle graph (was: steps-only copy expanded empty). Collision-safe node merging; builtin
  `on_error` preserved verbatim through expansion.
- **Loop graph vehicle (bd-27d0/bd-bpd9)** — a bead opting in via `## WORKFLOW <name>` (or a
  `workflow:<name>` label) runs its composed builtin through a detached pane-mode engine
  invocation; in-graph verify writes a workflow receipt; close still requires loop VERIFY +
  `CommandVerified`; bare path byte-identical; marker-file dedupe survives loop recovery.
  Title text never opts in (the c57dfd4-era stub removed).
- **Failure taxonomy + funded preflight + fallback chains (ADR-0026 W1)** — `FailureKind`
  classifier at spawn/send/headless (signature table, no-match = Unknown with evidence),
  one-reroute-per-class fallback chains with audit rows, provider-funded preflight, pane-path
  fallback hop, unknown fallback names fail at catalog load.
- **Bounded mail-health escalation (ADR-0026 W2)** — `loops.json` v3 `mail_health`; 3 consecutive
  renew/lease failures stop new dispatch + escalate (kind-14) until a success; loop-start
  fails loud without a usable identity key.
- **Lease reclaim (ADR-0025 W1)** — `pane_exited` reclaim with respawn-race suppression,
  7200s/86400s staleness tiers, reservation release by reason, audit comment; never `br close`.
- **Typed artifacts + stitcher (ADR-0025 W2)** — `ArtifactRecord` (four status vocabularies +
  grounding), `flywheel artifacts <run_id> [--json]` / `artifacts stitch`; admission requires
  embedding support, never worker self-rating.
- **Loop reducer + journal (ADR-0025 W3)** — `tick(&LoopModel, &LoopMsg)` total reducer,
  `LoopPhase` illegal transitions unrepresentable, Msg journal + `loop replay` / `loop why`,
  `LoopSnapshot` (delivery-verdict carrier, deferred).
- **Parent-death policy (ADR-0025 W4a)** — `on_captain_death: stop|continue|adopt` (default
  stop; stop implies `--force-working` for attributed pins); `toron contact captain --clear`.
- **Plane tripwires (ADR-0025 W4b)** — secret→public and job-assign→private fail closed at
  send with named remedies; `--unsafe-plane` audited; `--raw` does not bypass.
- **Ingress + launch idempotency (ADR-0025 W5)** — `IngressEvent` seam + `handle_ingress`;
  spawn/compose keyed by `(project, run_id|bead, profile)`, dead-pane = respawn; compose
  `--run` replays recorded executions.
- **Evidence-bearing approvals (ADR-0024 Spec 3)** — promise payload carries
  `nodes_completed/failed/pending`, artifact paths, `ledger_path`, `evidence_hash`; a different
  hash is a new question; reject is a decision.
- **PaneStalled classification** — stall detection with evidence-tail sampling classifies
  `rate_limit`/`network`/`model_truncated`/`silent_idle`; watching tier before Diagnose.
- **Mailbox/channel wake completeness** — `MailDelivered` metadata ring (content stays sealed),
  `AckSlaExpired` events, watch kinds include channel join/leave (39002).
- **GitFact producer** — the strict pre-commit guard appends attributed commit facts to the
  flywheel fact journal (best-effort, never blocks).

### 🔧 Fixed

- **Non-vacuous VERIFY evidence** — receipt/loop paths parse test counts; a zero-match filter
  warns (and refuses in strict mode) instead of closing green on nothing.
- **channel react after post** — react target hydrates from the bus (was: unknown-message
  failure right after post; also unblocked e2e sc30).
- **reopen_flow fixture** — carries the loop-ready fence `br ready` now requires.

### 🧹 Removed

- Unused `figure-it-out` builtin YAML (decide routes it to `compose --agent`).
- `assert!(true)` padding tests and the title-contains-`graph` vehicle stub.




### ✨ Added

- **Standalone cron CLI (ADR-0012, `bd-25g7.3`, 2026-08-11)** — `flywheel cron` operates on the
  new cron YAMLs (`CronSpec`): `lint` (fail-stop; engine + flywheel tiers), `run` (on-demand, via
  the shared engine with `call_workflow` resolution), `list` (source + scope + steps), `show`,
  `pause`/`resume`, `status`, `history`, `start`, `remove`. User dirs: global
  `~/.config/flywheel/crons/` + per-project `.flywheel/crons/`, precedence project > global >
  builtin. The weekly-review cron migrated to a standalone YAML (bd-25g7.4/6); crons.toml retired.
- **Coalition CLI (ADR-0011, `bd-foh8.3a`, 2026-08-10)** — `flywheel coalition
  {register,form,status,list,intent,reserve,detect,auto-coalesce,negotiate-*,disband}` over the
  intent/soft-conflict substrate.
- **prime-agent (pa) as a spawnable harness (H1, `bd-harness-prime-agent-818f`, 2026-08-15)** —
  compiled-in `pa` profile + `[harness.pa]` spawn recipe (`prime-agent --offline`); when herdr
  rejects an unknown kind (`unsupported_agent_kind`), `start_agent_with_retry[(_dyn)]` falls back
  to `pane.run` the recipe with shell-quoted launch args and waits/renames. prime-agent's built-in
  herdr socket extension self-reports lifecycle (hook-authority Accept route; verified smoke: spawn
  → agent listed as `prime-agent`, boot prime answered READY). `agent.prompt` remains blocked
  upstream (herdr closed Agent enum; kind request: herdrdev/herdr#2840).
- **Crew/agent model rework (`bd-42fs`, 2026-08-10)** — all profiles default harness `omp`;
  per-role models (oracle=glm-5.2/max/fb grok-4.5, planner/coder/reviewer/housekeeper=
  deepseek-v4-flash, scout=big-pickle), effort-tuned, retry-on-429 fallback wired. Per-step
  harness override (`spec.harness`) for review workflows.
- **Authoring guide + references** — `docs/workflow-authoring.md` + the skill references
  (workflow-authoring, coalition-orchestration, cron-authoring standalone model).

### 🐛 Fixes

- **Loop stale-landed dedup (`bd-dedup-stale`, 2026-08-11)** — a re-queued (un-claimed) bead kept
  its stale `landed` dedup record, so the loop dedup-skipped its re-dispatch forever.
  `clear_stale_landed_for_idle` invalidates landed records (same invocation) for idle/done agents
  before each dispatch; working/blocked/unknown preserved (crash-resume dedup intact). 3 new
  `keep_landed_record` tests; 515 flywheel tests pass.
- **`cron run` `call_workflow` resolution (2026-08-11)** — the on-demand cron run injected the
  WorkflowResolver + ProfileResolver (`local_with_resolvers`) so a cron body's `call_workflow`
  resolves against the catalog/built-ins.

- **Live toron ↔ agent-mail search parity** (2026-08-08) — toron's
  unwrap-loop projection now attributes inbound gift-wraps to their project
  via the `agents` index: registration upserts `agents` with the verbatim
  project slug, the pipeline resolves `project_slug` from the recipient key
  at projection time (was hardcoded empty → project-filtered search always
  returned `[]`); `search_messages` emits the AM-shaped
  `{result, next_cursor}` envelope; `mail_shim` passes the daemon envelope
  through instead of double-wrapping it; the live parity test polls the async
  projection (bounded 10s) and `live_parity_full_surface` passes against both
  backends end-to-end.
- **Collaboration patterns carry real content** — debate/plan-review harvest reply
  **bodies** (not subjects); plan-review gate is a real phase-driven loop honoring
  `--max-change-rounds`; debate/plan-review default agents resolve to spawned
  `flywheel-*` names (review-fix F1/F2/F7, 2026-08-05).
- **Blocked escalation halts** — T3 (bounded) exits the durable loop with a
  distinguishable `halted:` result under `--on-blocked mail` (review-fix F4).
- **xproj honesty** — `--to-project` is routing context (validated + carried in the
  body, never silently dropped); loop-triggered spawns are background, no focus
  steal (review-fix F3/F5).
- **Backend migration completed for the durable loop** — `__send`/`__kind`/`__read`/
  `__respawn`/`__xproj-inbox` route through the backend factory; trait gained
  `agent_prompt_coded` (landing semantics) + `MailBackend::agent_name`; CLI commands
  stay concrete per documented deferral (review-fix F6).
- **Quality gates honest** — verify-0001/0002 check real behavior; new
  `verify-review-fixes.sh` regression gate; live `--ignored` suite runbook documented;
  clippy `--workspace` 0 warnings 0 errors (review-fix F8/F9).

### 🚀 Features

- **Unified workflow CLI (ADR-0005 P3.7)** — `flywheel workflow {list,show,run,
  lint,new,history,crew}`: one vocabulary over built-ins + canonical YAML +
  absorbed legacy. `run` invokes through the engine (lint gate first); `crew
  <name>` previews the auto-crew; `lint` is the fail-stop gate adding the
  flywheel-tier checks the engine cannot do (profile `@name`, `stage:` alias,
  `{{inputs.x}}` resolution) on top of engine lint.
- **Collaboration spine (ADR-0001)** — event-wake loop + poll floor; patterns
  `debate`, `plan-review`, `xproj`; mail contact/product; recipe catalog
  `docs/collab/recipes.md`; agent ack/lease/build-slot contracts in spawn prompts.
- **Pluggable backends (ADR-0002 R0)** — `MultiplexerBackend` + `MailBackend`
  traits; factory env/`.env`/config selection; `flywheel backend list|status|default`;
  feature-gated stubs (superlogical/tmux/toron). Defaults unchanged: herdr + agentmail.
- Herdr swarm orchestrator — ntm-parity CLI + durable loop (Resonate SDK 0.6.0)
- Flywheel loop --work — bead-driven mode sourcing from br ready (dynamic plans)
- Work-mode resilience — interrupt resume (in-progress beads) + context-limit respawn
- __send --confirm prompt-landing confirmation in hidden send leaf
- *(loop)* Confirmed sends through durable workflow (bd-send-confirm-durable-8ut)
- *(agentstate)* State-file contract for context signals (bd-agent-state-contract-ukt)
- *(herdr)* Typed pane wait-output helper (bd-pane-wait-output-helper-efq)
- Escalate stalled sends, hard-stop at 3 failures
- Respawn agents stuck in unknown for 2+ polls
- State-file contract in bead/resume prompts
- Context verdict from terminal + state file
- --interval-active config surface for adaptive cadence
- Per-agent last-sent prompt registry
- Herdr protocol drift check in flywheel deps
- __wait-output leaf + README recipe
- Dedupe re-sends against the last-sent registry
- Integration gate for the hardening epic
- Flywheel integration: beads SUBAGENT generator for opencode/pi/grok
- Flywheel integration generator epic (bd-cor) — 8 beads documenting the subagent architecture, all closed with evidence

### 🐛 Bug Fixes

- *(loop)* Never dispatch work to unknown agents (bd-unknown-skip-dispatch-s03)
- *(loop)* Adaptive poll cadence (bd-adaptive-interval)
- Bd-nil + bd-w17: duplicate-label workspace resolution + spawn-time agent naming

### 💼 Other

- Add herdr agent-automation hardening epic + 17 task beads
- Correct bd-ojx to herdr 0.7.5-preview, complete stall-path evidence
- Close epic bd-herdr-automation-hardening-q9t — all 16 tracks closed
- Restore epic close reason (backtick mangling)

### 📚 Documentation

- AGENTS.md + README launchd note; beads tracker init

### ⚙️ Miscellaneous Tasks

- Bump herdr protocol docs v17->v18 (bd-protocol-v18-kkd)
- Zero warnings across all targets

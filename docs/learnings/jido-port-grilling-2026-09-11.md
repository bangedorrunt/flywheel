<!-- governed-by: ADR-0028, ADR-0024, ADR-0026 -->

# Grilling: Jido → toron-workflow port plan (audit + interview) — 2026-09-11

Agent: QuietHeron · Bead: bd-zvp0 · Outcome ADR: toron `docs/decisions/0032-workflow-engine-honesty-jido-contracts.md` (proposed)

Question: which Jido (agentjido, Elixir) concepts genuinely improve the flywheel decision + toron-workflow engine for frictionless multi-agent automation, and which current implementation bugs/gaps/unwires block it?

## Verdict

The stack already implements ~60% of Jido's architecture on better primitives (tick = cmd/2 pure fold, Cmd = Directive, panes = agents, Resonate > Jido's opt-in persistence). "Port 100%" is the wrong target. The real blockers are 25 verified defects (24 audited + 1 found during close) where the engine silently lies about success, plus a decision layer that is a hardcoded keyword ladder with two divergent copies. Fix honesty first; port Jido's *contracts* (schemas, error-class discipline, compensation, catalog semantics) second; refuse its runtime (ADR-0028 Option B rejection re-affirmed).

Captain's five decisions (grilling rounds, 2026-09-11):

1. **Port target**: contracts + commit-before-effects (v3 Turn idea), NOT the runtime.
2. **Turn granularity**: per-step turns — validate candidate → write receipt (corr, owed) → dispatch with corr as idempotency key → dedupe on resume. Whole-run Turn rejected (can't buffer effects behind a 20-min parked await).
3. **Decide revision**: catalog-driven router — `decide_trigger` fields become the one routing table; ambiguity = loud error; no silent steer fallback; briefing reads the same table (kills the decide/briefing divergence).
4. **Schema home**: closed Action enum + per-variant schema tables (JSON-Schema literal per variant; lint + invoke boundary validate from the same tables). Open registry rejected: every dispatch becomes a lookup with a miss path — "miss = quiet nothing" is exactly the shipped `Ok(Null)` bug class.
5. **Compensation**: lease-shaped auto-undo only (fail_run of an effectful step → release reservations by bead reason + captain mail + bead comment). General on_error undo DSL rejected.

Sequencing: **wave 1** = engine honesty gate (toron) + decide router (flywheel), parallel lanes; **wave 2** = schema tables + per-step Turn receipts + lease compensation.

## Live evidence witnessed this session (2026-09-11)

The briefing classifier routed this session to `captain-audit --execute --mode headless` (P2 argv). Run `fw-cli-2026-09-11T13:00:33Z`:

- Failed in **1.8s**: `failed_step: synthesize`, `harness exited exit status: 1`; zero panes materialized for the packed crew (oracle/reviewer/scout).
- `flywheel artifacts <run_id>` → `[]` (no artifact trail for a failed run).
- `flywheel workflow history captain-audit` recorded the same run as `"status": "done"` with `"project": "workflow"` (wrong project, wrong status).
- Lint passed with warnings: steps `challenge`/`synthesize` carry `on_error: fail_run` with no `kind` classification (ADR-0024 Spec 1 R-E2) — the builtin violates its own taxonomy ADR.

## Defect inventory — toron-workflow / toron-mcp (12)

Spot-verified first-hand where marked ✓.

| # | Location | Mechanism | Sev |
|---|---|---|---|
| 1 | ✓ `toron-mcp/src/workflow_host.rs:193` | `mailbox_runner` catch-all `_ => Ok(Null)`: `react` and `call_webhook` silently succeed doing nothing; docstring at :76 claims honest `{ok:false}` | high |
| 2 | ✓ `toron-workflow/src/schema.rs:591` | CallWebhook docstring claims "SSRF-guarded in the engine" — no HTTP client exists in the crate, runner never executes webhooks | high |
| 3 | `toron-mcp/src/workflow_host.rs:556` ✓ | Trigger loop drops `engine.run` outcome to `debug!`; never reads approval_sink/spawn_sink/verify_sink; publishes no 46011 pending, subscribes no resolutions, never calls `build_workflow_run_terminal` — parked promise_ids accumulate undrained in prod (ADR-0024 W1 owns draining this sink; still unlanded) | high |
| 4 | `toron-workflow/src/engine/control_flow.rs:207` | `run_steps`/`run_step_with_enclosing` never read `Step.guard`; only graph-node guards (:1140) and branch if-guards (:633) evaluate — linear-step `guard:` silently ignored | high |
| 5 | `toron-workflow/src/schema.rs:651` | `Spawn.no_claim_timeout` serde-defaults to 0 (= wait forever, control_flow.rs:831) contradicting documented 600 and `default_no_claim_timeout` (schema.rs:308) | med |
| 6 | `toron-workflow/src/engine/control_flow.rs:931` | `run_graph` breaks on empty ready-set: cyclic graph fed directly to `run`/`invoke` (bypassing lint) returns partial outputs with success shape | med |
| 7 | `toron-workflow/src/engine/control_flow.rs:767` | `run_call_workflow` sets child trigger = raw inputs + parent_steps, skipping `bind_input_defaults` (engine/mod.rs:472) — `{{inputs.x}}` unresolved inside called workflows | med |
| 8 | `toron-workflow/src/engine/control_flow.rs:993` | Graph fail_run kind-retry bound hardcoded `max_attempts=3`, no schema field, ignores enclosing Retry config | low |
| 9 | `toron-workflow/src/engine/control_flow.rs:670` | foreach over non-list → `[]` silently, no error | low |
| 10 | `toron-workflow/src/engine/control_flow.rs:703` | parallel over non-numeric → `unwrap_or(1)` single copy silently | low |
| 11 | `toron-workflow/src/lint.rs:624` | delay-over-270s warning cites an in-process cap the engine no longer enforces (control_flow.rs:321 unbounded ctx.sleep) — stale advisory | low |
| 12 | `toron-cli/src/workflow.rs:79` | CLI `workflow run` uses noop runner without approval/spawn resolvers — any approval/spawn-await workflow parks forever, no timeout, no resolve path | med |

Engine facts: Action is a closed serde-tagged 14-variant enum (schema.rs:566); no per-action input-schema validation framework; no compensation; retry is a wrapping `Action::Retry` variant; failures collapse to `{ok:false, error:string}` (ADR-0024 §"Binary on_error, no error taxonomy" names this; W1's `{ok:false, kind}` is the mandated fix). Adding an action = 6 mechanical edit sites (enum+type_name, template, run_step dispatch, graph routing, lint, prod runner).

## Defect inventory — flywheel (12)

| # | Location | Mechanism | Sev |
|---|---|---|---|
| 13 | ✓ `src/decide.rs:922` | `Archetype::Review::default_parts()` returns `["code-review"]`; no such stage exists (only `ce-review`) — review-archetype compose fails affinity/lint | high |
| 14 | ✓ `src/main.rs` runs-row writer | Failed run recorded `"status":"done"` in workflow history (witnessed live, above); `project:"workflow"` attribution wrong; artifacts empty for failed runs | high |
| 15 | ✓ `src/main.rs:346-349` | `stage_prompt()` loads untiered `StageLibrary::load()` — project-tier `.flywheel/stages.toml` overrides silently ignored on send/headless prompt path | high |
| 16 | `src/main.rs:31040-31046` | headless_dispatch leaf failure returns `Ok(json!({ok:false}))` not Err — `on_error: continue` steps record failure-shaped output and the run sails on | high |
| 17 | `src/decide.rs:56` | `topology_for_primitive _ => "swarm"` buckets every unlisted primitive silently | med |
| 18 | `src/decide.rs:594-597` | `wave_shaped` fires on any `swarm/waves/lanes` substring or any budget parse (`coder:2`) — misroutes non-wave goals into wave-swarm | med |
| 19 | `src/briefing.rs:97-106` vs `decide.rs:50-57` | briefing duplicates the primitive→topology map with divergence (babysit-bead: open vs swarm) — two truths for one axis | med |
| 20 | `src/workflow_catalog.rs:388-400` | `absorb_pipeline` emits `DispatchJob{extra.call_workflow:true}` instead of `Action::CallWorkflow` — absorbed pipelines list but can never run | med |
| 21 | `src/builtin_workflows.rs:1400-1466` | beads-lifecycle guards use `trigger_until`/`trigger_gate_before_implement` aliases because `{{inputs.*}}` in guards degrades to `'0'` — early-exit depends on an engine aliasing quirk | med |
| 22 | `src/crew.rs:862-867` | each/parallel/when/retry wrapper hardcodes `on_error: Continue` outer over FailRun inner — dilutes leaf failure policy | med |
| 23 | `src/pane.rs:622-649` | per-step `spec.harness` override reroutes to a different pane than the crew plan or hard-errors — same YAML behaves differently headless vs pane | med |
| 24 | `src/decide.rs:832-835` + `src/stages.rs:render_stage` | Unmatched targets silently become steer runs; typo'd `--param` warns to stderr and renders on | low |
| 25 | toron daemon (`toron serve --mcp-http 127.0.0.1:18080`) reservation roster + `guard.rs:62` | Daemon roster truncates the holder pubkey to 8 chars and drops `agent_name` (store row: `agent_name=QuietHeron, agent_pubkey=25b8b7c0…`, guard sees holder `25b8b7c0`). Strict guard raw-compares attribution vs truncated holder (no-resolver path), so **the holder's own exclusive lease blocks their own commit** — discovered landing this session's ADR commit; passing attribution is the 8-char pubkey prefix. Fix direction: roster projects `agent_name` (or guard resolves npub→pin per bd-pin-public-plane-s-easy.1) | high |

## Close-note (2026-09-11)

Defect #25 was discovered after the audit freeze, during the bd-zvp0 close ceremony (guard blocked my own lease three ways: shared lease invisible to strict own-coverage fetch → exclusive lease → npub-vs-truncated-holder mismatch). The ADR-0032 commit (`f92b1df` in toron) landed under the holder-matching attribution — guard satisfied on its own terms, no `TORON_GUARD_STRICT=0`, no `--no-verify`.

## Jido ground truth (v2.3.x shipped; v3 = preview)

Worth porting (contracts): per-action input/output schemas (NimbleOptions/Zoi), Splode error classes with centrally-derived `retryable?`, compensation (`on_error/4`, supervised, timeout), Exec options (timeout/max_retries/backoff defaults, per-call override), Catalog semantics (canonical merge, ambiguity errors, search), Instruction normalize layer, Plan DAG phase computation, telemetry/sanitizer/context-propagators.

Already here in stronger form (do not port): `workspace::tick` = `cmd/2` purity boundary; `Cmd` = Directive/StateOp split (ADR-0027/0028); doorbell sensors = Jido sensors (fault-as-data, bounded channel, backpressure); trigger_filter + kind registry = signal router; Resonate durable functions > Jido's opt-in persistence + at-most-once timers; receipts sqlite `owed→acked→resulted` + corr authority (ADR-0028 E3) = the Turn substrate.

Refused (ADR-0028 §6, re-affirmed): OTP GenServer runtime, supervision-as-heal, CloudEvents envelope, Composer FSM, Pods/InstanceManager, Phoenix.PubSub, memory plugin reserved keys. Plus today's: open action registry, whole-run Turn, Splode-as-dependency (kind set stays ADR-0024/0026's frozen `Auth|RateLimit|Timeout|Blocked|Provider|Cancelled|Unknown|Billing`; Jido's `invalid`/`config` classes live at parse/lint time, not runtime kinds), general undo DSL, LLM decide.

## Constraint map discovered (prior art that binds this plan)

- **ADR-0028** (accepted): behavior-first contracts on Wheelhouse nouns; Option B (Jido runtime port) rejected; E1 exhaustive DirectiveExec; E3 corr authority + receipt ownership; E7 "no Zoi/NimbleOptions port" governs the flywheel Cmd/Msg boundary — today's schema-table decision extends schema discipline to the *workflow YAML/engine* boundary without amending E7.
- **ADR-0024** (accepted): W1 owns runtime-failure kinds at the ActionRunner boundary, ApprovalSink drain, graph-first; `kind` optional until every runner site classifies, then one-line default-flip.
- **ADR-0026** (accepted): frozen FailureKind spelling, signature table, retry-vs-reroute rules; no second retry policy — the Turn/idempotency work must not create one.
- **ADR-0014**: kind registry frozen; 46011/46012 already exist for approval/terminal — Turn needs zero new kinds.
- The beads-lifecycle guard alias hack (#21) exists *because* silent template degradation is the norm; schema tables + loud parse are what delete the hack.

## Grilling transcript index

Five decision rounds + two clarifications, all via structured ask: port target → Turn granularity → decide revision → schema home (+ elaboration on dynamic authoring surfaces) + compensation scope → sequencing + deliverable shape. Full interview in session transcript; decisions restated in ADR-0032 §Decision Outcome.

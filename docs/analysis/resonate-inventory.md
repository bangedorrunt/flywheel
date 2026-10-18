# Resonate inventory (bd-xt1e) — the spine-program migration map

Operator decision (revised): Resonate SDK stays as the durable substrate;
the Elm Loop becomes the dispatch spine driving over it. Removal is off
the table. Read each row below as a drive-path onto `Program`/`Interpret`/
`Loop`, not as a deletion: the SDK API in column 3 keeps working until its
Loop replacement proves parity, then the call site moves.

## Flywheel (`resonate-sdk = "0.6.0"`, `src/`)

| # | File | API used | Replacement |
|---|------|----------|-------------|
| 1 | `durable.rs` `run_durable_loop` | `Resonate::new` + `ResonateConfig{url, group}` + `with_dependency(authority)` + 28× `.register(leaf)` + `swarm_loop` root | One `Loop` per host thread (ADR-0040 hosting); leaves become `Cmd`s of a swarm `Program`; `RuntimeAuthority` stays an injected service |
| 2 | `durable.rs` leaves (28: `poll_swarm`, `poll_events`, `send_prompt`, `dispatch_guard_scan`, `mail_not_ready`, `close_check`, `reclaim_stranded`, …) | `#[resonate_sdk::function]`, invoked via `ctx.run` (21×) | Pure `update` arms returning `Cmd`; dispatch through `plan_attempt` + `Interpret` (bd-b61z seam is the template) |
| 3 | `durable.rs` waits | `ctx.sleep` (2×), `ctx.now` (1×) | `Park` + `upsert_pending` resume rows (bd-lm27 primitive needs its caller) |
| 4 | `durable.rs` deps | `ctx.get_dependency` (3×: `RuntimeAuthority`…) | Framework services (`ManagedRuntime` layers); already the pattern (`ParkClock`) |
| 5 | `doorbell.rs` `heal_tick` + `supervisor_function_registered` | `#[resonate_sdk::function]`, server-side every-minute schedule | `Loop` + durable-log timer; schedule state in the journal, not the server |
| 6 | `main.rs` test harness | `Resonate::new` + `.schedule(...)` | Same as 5; harness boots a `Loop` instead |
| 7 | `cron.rs` → toron-workflow `CronManager` | In-process Resonate runtime driving `cron-spec:{name}` loops | `Loop` + `sleep`-as-park; tick ids from the journal |
| 8 | `Cargo.toml` | `resonate-sdk = "0.6.0"` | Removed last, when 1–7 are migrated |

## Toron (`resonate-sdk = "0.6"`, `resonate-sdk-macros = "0.1"`)

| # | File | API used | Replacement |
|---|------|----------|-------------|
| 9 | `toron-workflow/control_flow.rs` root fn | `#[resonate_sdk::function]`, `Context`, `resonate_sdk::Result` | `Program::init` + `Loop::boot`; errors as `Failure`/`Update` |
| 10 | control_flow deps (16×) | `ctx.get_dependency` (`RunJournal`, `EffectRunner`, `VerifySink`, `ParkClock`, `ApprovalSink`…) | `interpret_in_env` services; runner becomes `Interpret` |
| 11 | control_flow children (5×) | `ctx.run` | Child `Program` dispatch via shared `plan_attempt` vectors (bd-b123) |
| 12 | control_flow waits (5×) | `ctx.sleep` | `Park` + `park_effect` caller (unwired — agenda item 1) |
| 13 | control_flow HITL (2×) | `ctx.promise::<Value>().data(..)`, `ctx.promise::<Decision>()` + `.timeout()` (task dispatch, approval) | Latent journal promises + `reconcile` query; wake via pending index |
| 14 | `toron-workflow/engine/mod.rs` | `Resonate`, `ResonateConfig` (`Resonate::local()` or URL server) | In-process `Loop` hosting; no server process |
| 15 | `toron-workflow/cron.rs` `CronManager` | Resonate durable-sleep loop, child promises, server-side schedules | Same as 7 |
| 16 | `toron-workflow/Cargo.toml` | `resonate-sdk`, `resonate-sdk-macros` | Removed last |
| 17 | `toron-mcp` | No direct SDK use (drives via `EffectDispatcher` + store) | Already spine-adjacent; retires with 9–13 |

## Non-goals (stay)

- The journal stores (`ToronJournal`, `WorkflowEffectStore`, sqlx) — the spine
  persists through them, not around them.
- The vendor policy (ADR-0040): the spine is still reached through
  `toron_core::kernel`, one port per repo.
- `PendingKind`/rebuild/prune semantics — orthogonal.

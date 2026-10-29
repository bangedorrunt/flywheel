# bd-bm6v: async/detached interpret shape (DESIGN — no code until reviewed)

Gap named by the bd-tlct ceiling (learnings `sync-interpret-ends-at-brain-and-detached.md`):
only sync `interpret_in_env` exists. Three arms remain on the Resonate path:

- **Prompt** — delivery is Brain-owned (budgets `take_allowed` 5/30/3, `inject_allowed`,
  sinks, digest, stats, journal-first `deliver()` in `crates/doorbell-rt/src/brain.rs`).
  A host sync edge would fork the prompt gate (storm risk).
- **FetchInbox / CheckAckSla** — signals for DETACHED async work by design (bd-1i8c:
  serial dispatch must never wait on subprocess/scan). Sync interpret would park the loop.
- (Scope note from comment #272: the production Loop boot + detached polling cutover
  joins this bead as residual — same machinery, see §4.)

T4 proceeds without these arms on the documented boundary. This shape keeps that
boundary intact: the live path (`handle_workspace_cmds` + rt `Effect`) stays until
each arm cuts over from SHADOW.

## Precedent (landed, bd-ma3y)

`DurableInterpret::interpret_in_env` in `src/doorbell_host.rs`: one arm (Audit) moves
from the host's `Effect` match into Interpret, backed by boot-built services
(`RtHost::durable_services`, process-static `ManagedRuntime` — never dropped inside
the async Brain). Edge trait (`AuditEdge`) + `ServiceTag` + `use_service`; success folds
quiet (T2 sentinel `DiagnoseFailed{SHADOW_FAILURE_NAME}`, emits nothing); `interpret()`
without services fails typed, never drops.

## Shape

### 1. Prompt — Brain-gated enqueue edge (no second gate)

- New edge trait, same file neighborhood as `AuditEdge`:
  `PromptEdge::enqueue(pin, line) -> Result<Enqueued, GateRefused>`.
- The production edge is a thin handle into the Brain's OWN gated queue — the same
  `deliver()` path (`take_allowed` budgets, `inject_allowed`, sinks, digest, journal).
  Interpret never budgets, never sinks, never digests.
- `interpret_in_env(Cmd::Prompt)`:
  - services present → `use_service::<PromptEdgeTag>()` → `enqueue` →
    `Ok` maps to `FxEffect::succeed(quiet)` (delivery receipt already travels the
    existing `Delivered`/`hear` path — no second confirmation);
    `GateRefused` maps to typed `FxEffect::fail` (refusal is data, never a drop).
  - services absent → typed fail, same as Audit today.
- Shadow-first: boot the edge in record mode beside the live `Effect::Prompt` path;
  cut over only when shadow enqueue set == live deliver set over the replay corpus
  (bead 1's transcript fixture is the seed corpus).

### 2. FetchInbox / CheckAckSla — detached spawn edge + existing pipe re-entry

The re-entry Msgs already exist (bd-1i8c pipe): `BrainMsg::InboxFetched` →
`Msg::InboxRows`, and scan → `SlaFired`. The arm must not invent a second pipe.

- New edge trait: `DetachedEdge::spawn(kind, targets) -> Result<JoinHandle, SpawnRefused>`
  on the boot runtime (`durable_services` `ManagedRuntime` owns the Tokio executor).
- `interpret_in_env(Cmd::FetchInbox | Cmd::CheckAckSla)`:
  - claims intent under the same effect key FIRST (journal parity with
    `effect_journal`: prepare → claim → invoke → settle-or-unknown);
  - spawns via the edge and succeeds IMMEDIATELY with a no-op (never parks);
  - the detached task's rows/overdue-ids re-enter through the EXISTING pipe Msgs,
    which carry the settle (cursor-hold, bd-7qg6 at-least-once, makes a late
    duplicate landing safe; Unknown is never settled as success).
  - spawn refusal → typed fail; cancellation (superseding fetch) drops the handle,
    and the next sweep re-asks — identical to today's detached semantics.
- Journal parity: the claim row is written before spawn; a crash between spawn and
  re-entry leaves `Unknown`, which pages (bd-effect-sweep-x464 policy), never success.

### 3. What does NOT move

- `RestartWatch` — execution is already host-side (single-flight coalescing); the
  Effect is journal-only.
- `FactMiss` — contract keeps `Effect::Audit`.
- Budgets, sinks, digest, `hear`/`deaf` tracking, ack-SLA dedupe (`sla_seen`) —
  all stay Brain-owned. Edges are handles, never policy.

### 4. Poll-driver residual (comment #272)

The production Loop boot + detached polling uses the SAME `DetachedEdge`: the poll
driver is one more spawner whose results re-enter as pipe Msgs. No second edge type;
the boot path and the doorbell path share the spawn service off `durable_services`.

## Verification shape (when code lands)

Mirror `doorbell_host::durable_tests`: leaf tests with a recorder edge —

- Prompt: enqueue-ok succeeds quiet; gate-refused fails typed; no-services fails typed.
- Detached: spawn-ok succeeds immediately with NO rows yet (rows arrive via pipe
  re-entry in a second step); spawn-refused fails typed; cancelled handle re-asks
  on next sweep.
- Journal: claim row precedes spawn; Unknown never settles as success.
- VERIFY stays `doorbell_host` green (`-p flywheel --bin flywheel doorbell_host`).

## Open questions for review

1. Should the Prompt shadow comparator live in `durable_tests` or as an
   integration replay over the bead-1 transcript corpus?
2. `DetachedEdge` spawn target: reuse `ManagedRuntime`'s Tokio executor vs a
   dedicated bounded pool (the 4-worker 4096-bound question belongs to bd-b61z —
   this design assumes that decision, does not make it).

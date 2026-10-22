<!-- governed-by: elm-effect-rs ADR-0002; toron ADR-0040; flywheel ADR-0015, ADR-0017 -->
---
status: accepted
date: 2026-10-22
decision-makers: bangedorrunt
consulted: effect-oriented skill; elm-effect skill (guides/15-use-the-apis, migrate-toron, migrate-flywheel); flywheel ADR-0015/0016/0017; toron ADR-0034/0035/0040
informed: flywheel, toron, beads, chiebukuro maintainers
---

# ADR-0018: elm-effect-rs is the core of the stack

**Decision in one sentence:** Every plane of this stack — flywheel, toron, beads,
chiebukuro — runs its durable work on `elm-effect-rs`, reached through one port
module per repo (`toron_core::kernel` in toron, `src/elm/` in flywheel), and
**no plane may ship a second spelling of the framework's shapes**: a second
`EffectState`, a hand-built offset, a breaker beside `policy::remote`, a
saga or listener that starts work the fold did not return.

**Status:** Accepted. This is the decision ADR-0017 left open: `foldcore` was
rejected because a second small kernel beside elm-effect-rs is the "third
spelling" ADR-0002 forbids. This record makes the replacement binding, and
states what is still owed.

## 1. What the framework already owns

The framework is not a library we call occasionally. It owns the shapes that
were duplicated in every plane:

| Shape | The framework's one | Where the stack reaches it |
|---|---|---|
| The durable log, its states and its plans | `DurableLog`, `FileLog`, `MemLog`, `plan_attempt`, `may_settle`, `check_replay`, `plan_park` | `toron_core::kernel` |
| The fold | `Program`, `Update`, `Loop`, `Interpret`, `Effect`, `Boot`, `Durability` | `toron_core::kernel` |
| Policy | `policy::remote` (retry, timeout, rate limit, bulkhead) | `toron_core::kernel` |
| The durable identity | `Producer` (producer id + epoch + seq), `Offset` | `toron_core::kernel` |

**No second spelling.** A duplicate adapter (`src/elm_framework.rs`, weaker
semantics: a breaker config that truncated the cooldown to whole seconds, and
a `decide_commands` alias with a vacuous drop test) is gone, along with its
`mod` line; flywheel's one adapter is `src/elm/` (bd-t13a, bd-1io4). A
sibling that wants the framework's shapes reaches them through its port
module, or the ADR-0002 "third spelling" rule is broken again.

**One persisted `EffectState`, the core one.** toron's boundary mirror is
gone (flywheel `bd-ozpj`). The journal's identity is the framework's
`(producer_id, epoch, seq)`, and the resume field is `offset` — never `seq`,
never a hand-built offset.

**The port module is the only door.** A plane reaches the framework through
its repo's port module, never through a direct dependency on the vendored
crate. toron's port module is `toron_core::kernel` (toron ADR-0040); flywheel's
is `src/elm/`.

## 2. What has landed

| Port | Where | State |
|---|---|---|
| The journal's `FileLog` export | toron `bd-6iyy` | Landed |
| The checkpoint-leaf adapter as an elm-effect `Program` | flywheel `bd-f572` | Landed |
| The leaf fold's four outcomes kept apart | flywheel `bd-q77h` | Landed |
| The journal's effect reads scoped to one key | toron `bd-uc8m` | Landed |
| The park reads the runner the host injects; `PARK_MAX_WAIT_SECS` is still 86400 | toron `bd-oz5d` | Landed |
| A changed re-render refuses instead of minting a second effect key | toron `bd-vt9y` | Landed |

The leaf port is the shape the rest copies: a pure fold, an interpreter at the
edge, the framework's log and producer for the durable identity, and one
implementation of the child wait.

## 3. What is still owed

In the order the branch files name (toron first, then flywheel):

1. **toron: the comms kernel on the framework** (Phases 1-3). The mailbox's
   send and reserve become durable commands; a reservation acquire is a claim
   (generation, lease, `may_settle`); NIP-44 stays a service at the edge.
2. **toron: the wake server's per-subscriber reads are scoped.** Right now each
   subscriber drains the arrivals with an unscoped scan, so a subscriber count
   starves the daemon's connection pool (toron `bd-dtua`, open).
3. **flywheel: the doorbell's models become programs.** `DoorbellModel` and
   `WakeModel` become one `InboxProgram`; doorbell ticks, herdr events and mail
   ingress become messages and never call the model.
4. **flywheel: `loop-mode supervise` becomes a `SupervisorProgram`** (Phase 4a).
5. **flywheel: `breaker.rs` becomes the breaker argument to `policy::remote`.**
   No second breaker beside it.
6. **flywheel: `durable.rs` stays on Resonate** until a `WorkflowEngine`
   adapter runs the same loop. `resonate_sdk` is not deleted in the change
   that turns `decide` into commands.
7. **beads and chiebukuro** follow their own branch files
   (`references/migrate-beads.md`, `references/migrate-chiebukuro.md`).

## 4. The rules that bind every later change

These are not advice; a change that breaks one of them is a defect, and the
review that lets it through is the defect.

- **One carrier.** No second `EffectState`, no second journal, no second
  breaker, no second scheduler. A plane adds a service, a policy value, a
  pure fold, or a log — not a runtime.
- **The four outcomes stay apart.** Success, typed failure, defect and
  interruption are four. A defect is not the child's answer; a cancelled run
  does not settle as a failed attempt.
- **The fold is pure.** No clock, no environment, no store, no randomness in
  `update`. The interpreter is the edge; a dropped command value performs no
  I/O.
- **Intent before the world changes.** A real effect is prepared and claimed in
  the journal before its provider runs, then settled once. A retry keeps the
  same identity, so replay cannot double-commit. Unknown stays unknown.
- **The resume field is `offset`.** A `seq` gap is a defect. A producer id
  with only an epoch, or only a seq, is a defect.
- **The park is not a new effect-row edge.** A row already `Parked` is
  `ResumeParked`, and a longer park than the ceiling is a typed refusal.
- **Every wait is bounded.** A retry has a cap, a backoff and a delay ceiling;
  a queue and an in-flight limit are bounded.
- **The model is a `BTreeMap` when its order changes the command list.**
- **No `or_die` after a retry, no `catch_cause` outside the supervisor**, and
  no `tokio::time::sleep` inside an effect.

## 5. The gates

- `cargo test -p elm-effect` is green before a sibling edit, and the sibling's
  own suite is green before the close. The framework's conformance witness
  (`sh conformance/witness.sh` in elm-effect-rs) is the framework's gate, not
  ours; we do not rebuild it.
- A port ships with the shape it copies: a test that goes red when the fix is
  inverted, and the checklist of the branch file it follows
  (`references/migrate-toron.md`, `references/migrate-flywheel.md`).
- A migration that ends is measured, not asserted: the sibling's suite, the
  framework's witness, and the live numbers the change was supposed to move.

## 6. What this costs

The framework is strict where our code was loose, and the strictness is the
point: the two live outages this stack had in one week (a daemon that could
not accept connections because it held one descriptor per subscriber, and a
wake brain that could not open a socket because launchd capped it at 256) were
both the stack re-implementing the framework's shapes badly. elm-effect-rs
owns the log, the fold, the policy and the identity; our job is to use them
correctly and to keep the ports thin.

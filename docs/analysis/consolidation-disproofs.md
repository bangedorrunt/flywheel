# Consolidation disproofs — which "duplicates" are real

Wave 8 / lane E (`bd-wave8-disproofs-buehf`). Three consolidation candidates
looked like duplicates on first read. Each row below carries file:line
evidence and ends in one of KEEP / CONSOLIDATE / DISPROVED / DONE. A
documented KEEP is a successful outcome — a forced merge of two
different concepts would be a regression.

## 1. Record lanes — DISPROVED (different concepts, same word)

**Candidate A — operator swarm checkpoints** (`flywheel/src/state.rs:2099-2135`):
`Checkpoint { ts, project, message, workspace_id, agents, git_head }`,
written by `save_checkpoint` (`state.rs:2118`) to
`checkpoints/<project>/<ts>-<slug>.json`, driven by the
`checkpoint save/resume` doors (`flywheel/src/cmd/checkpoint.rs:59`).
Content is a roster: agent names, kinds, pane ids, git head. Readers are
operators recovering a swarm (`loop-mode` resume paths,
`src/cmd/loop_mode.rs`, `src/durable.rs`).

**Candidate B — engine model boundaries** (`flywheel/src/run_state_recorder.rs:335-345`):
`RunStateRecorder::record(model: &RunStateModel, effect_keys: &[String])`
persists an engine state-model plus its effect keys into the toron index
`run_state_checkpoints` table (seq-ordered, prune floor via `retain`,
`run_state_recorder.rs:348-356`). Readers are `run-state replay/fork`
(read-only forensics) and a future crash-resume path.

| axis | A (swarm checkpoint) | B (model boundary) |
|---|---|---|
| storage | JSON files, `checkpoints/<project>/` | sqlite, toron index |
| content | roster (who/panes/git) | model + effect keys (what the engine knew) |
| writer | operator door | engine run path |
| readers | human resume | replay/fork forensics |

Verdict: **DISPROVED**. Same word, disjoint storage/content/writer/readers.
Follow-up: none, keep as-is (this page is the name-disambiguation).

## 2. Build slots — DISPROVED as duplication (one FSM + one adapter)

The lease state machine lives once in
`toron/crates/toron-core/src/index/slots.rs:19-67`
(`BuildSlot`, `SlotStatus`, `GrantedSlot`, `SlotConflict`, `SlotLease`,
`try_acquire` at `:67`, `renew_slot`, `release_slot`, `active_slots`).
`toron/crates/toron-core/src/mailbox/build_slots.rs:23-196` is the
bus/tool wrapper: every verb delegates (`slots::try_acquire` at `:67`,
`slots::release_slot` at `:93`, `:169`, `slots::renew_slot` at `:133`,
`slots::active_slots` at `:196`). Count of FSMs: **one**.

Verdict: **DISPROVED** (as "two implementations"). Already layered
correctly. Follow-up: none, keep as-is.

## 3. Inbox reads — one core projection, three different products

The one core projection is
`toron/crates/toron-core/src/mailbox/messaging.rs:718`
(`Mailbox::fetch_inbox(&FetchInbox) -> Vec<InboxMessage>`, rumor-time order).
Its readers:

- **Plane-P `fetch_inbox` (MCP) + `mail inbox` (CLI)**: direct projection
  reads with the `since` high-water cursor. Same product, two transports.
  Verdict: KEEP.
- **TUI inbox** (`toron/crates/toron-cli/src/tui/host.rs:177-210`): calls the
  same `mailbox.fetch_inbox(&FetchInbox{...})`, reshapes rows via `mail_row`
  (acked-corr join). Same projection, display shaping. Verdict: KEEP.
- **`toron next`** (`toron/crates/toron-cli/src/comms.rs:30-50`): consumes
  the per-recipient envelope LOG via `InboxStore::next`
  (`toron/crates/toron-core/src/inbox.rs:1037`) — advancing the Delivered
  cursor in the same transaction. A destructive read, not a projection read.
  Verdict per pair (fetch vs next): **DISPROVED** — different products.
  Follow-up: none, keep as-is.
- **`fetch_inbox_product`** (`toron/crates/toron-core/src/mailbox/product.rs:184`):
  fan-out across member projects, delegating per project. One core +
  fan-in adapter, same shape as §2. Verdict: KEEP, no consolidation needed.

## 4. Landed consolidation items — DONE (commits, not open)

- **ToJson**: DONE — `551441c` (single definition in `toron-core/src/json.rs`,
  re-exported; `bd-wave5-tojson-core-x7kj` closed LEGAL).
- **Host**: DONE — `4da63cc` (durable host moved into toron-workflow;
  `bd-wave4-host-move-hvc9` closed).
- **Dispatcher**: DONE — `ea4ecfb` (dispatcher surface moved to
  toron-workflow; `bd-wave1-dispatcher-move-9hpl` closed LEGAL).
- **Plane**: DONE — `a1fb7d9` (port inversion) + `9342e7a5` (physical
  `toron-plane` split; `bd-wave6-plane-port-8udj` LEGAL,
  `bd-wave7-plane-crate-tx3w` CEILING with flywheel-leg residual only).

## 5. Wake path — referenced, not re-litigated

`bd-wake-decommission-dsd8` (toron tracker, **open**): bus/wake +
wake_sock have live callers; the decommission decision is open. This page
takes no position on it.

## No open item unnamed

All three candidates resolve to documented KEEPs; the four landed items are
DONE with commits; wake stays with its open bead. No CONSOLIDATE verdicts,
so no follow-up beads are proposed. No row is a cross-repo deliberate
divergence, so `docs/known-divergences.md` gains no link.

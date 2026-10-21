# Stack audit: flywheel x toron x beads

Date: 2026-10-21. Lane: `bd-aec2`. Vehicle: open (current pane), pin `IvoryMarlin`.
Rubric: effect-oriented principles 1-15 + the elm-effect ADR-0002 sibling-port "Done"
checklists for flywheel / toron / beads (`~/.agents/skills/elm-effect/references/migrate-*.md`).

Every finding names the code path that breaks. Anything I could not read myself was
spot-verified before it went in; the verification is stated inline.
Live measurements are against the running system on 2026-10-21 (macOS, `~/.local/state`,
`~/.local/share/toron`).

**Summary: 9 S1, 16 S2, 10 S3.** The S1s are not nine bugs: five of them are the same
defect seen from five ends (an ack-SLA flood), and the rest are identity, verdict
preservation, and timeout faults.

| Section | Severity | One line |
|---------|----------|----------|
| 1 | S1 | ack-SLA reminder floods the kind-9 room bus; the bus saturates; the doorbell wake brain drops real wakes |
| 2 | S1 | the agent-identity ladder has **three different orders** across flywheel and toron |
| 3 | S1 | the lease holder is reported as an 8-char pubkey but compared against a pin: a lane treats its own lease as a peer's |
| 4 | S1 | the durable-leaf watchdog bounds the child's exit but not the pipe drain: a grandchild holding stdout hangs the loop forever |
| 5 | S1 | the pane dispatch retry has no attempt timeout, no budget timeout, and the breaker outside the retry |
| 6 | S1 | the yield board is raw-keyed while the lease half of the same join is canonicalized (latent: fires on a path-form project) |
| 7 | S1 | `br doctor --repair --allow-repeated-repair` destroys every gate verdict row: the "cannot be bypassed" guard has an ordinary-flag bypass |
| 8 | S1 | three more paths rebuild the beads DB from the issues-only JSONL with **no** verdict guard |
| 9 | S1 | one orphan FK row aborts the whole gates-sidecar reload, and the failure is swallowed at warn level |
| 10 | S2 | `flywheel arrive` refuses a **correct** arrival when the pane's pin is the captain alias |
| 11 | S2 | `toron contact captain --pin` refuses a fold that **does** route in open mode; the error says the opposite |
| 12 | S2 | the state lock's stale-steal is a TOCTOU: two writers can both hold "the lock" |
| 13 | S2 | the inject budget is an unlocked, non-atomic read-modify-write |
| 14 | S2 | the durable-intent journal is one fixed file per project, deleted on success: concurrent passes lose each other's intent |
| 15 | S2 | flywheel declares a **second `EffectState`** (5 states) beside toron's and the framework's (9 each) - the elm-effect "Never" list |
| 16 | S2 | the bead-id citation is case-**insensitive** in beads and case-**sensitive** in flywheel: a legal close is invisible to the ledger check, and a closed bead cannot be re-gated |
| 17 | S2 | beads binds the sha you pass; flywheel **derives** it by tier: they disagree on which commit proves the bead |
| 18 | S2 | a `policy.yaml` with `strict: true` and no `gates:` block turns the fail-closed close **off** |
| 19 | S2 | the sha-citation check is warn-only when the sha resolves nowhere |
| 20 | S2 | beads accepts a `.beads/**`-only commit as "carries work"; flywheel skips exactly those commits |
| 21 | S2 | the journaled engine constructor falls back to `Recording` when the store is unreachable, contradicting "Enforced refuses"; and `bypassed-sink-uncovered` is documented by toron and emitted by nobody |
| 22 | S2 | the spawn registry is append-only and resolves first-match: 12 pane ids carry 2-7 rows each |
| 23 | S2 | the toron daemon runs a 3-day-old build |
| 24 | S2 | the shipped claim recipe omits `--claim`, so the atomic claim guard does not run: two lanes can work one bead. And `br update --claim` without `--actor` stores `$USER` as the assignee |
| 25 | S3 | waste and latent risk: hyphen pins, 6 legacy slug inboxes, ~100 fixture project dirs, unbounded full-file reads, `br ready` O(n^2), a full gates-sidecar rewrite per flush, the breaker cooldown, a blocking join on a Tokio worker, dead flags, and seven AGENTS.md claims the code contradicts |

---

## 1. S1 - the ack-SLA flood saturates the bus and the doorbell drops real wakes

One defect, seen from five ends. It spans toron's mailbox, toron's bus, and flywheel's
doorbell, and it is live right now.

### The chain

| # | Where | What |
|---|-------|------|
| 1 | toron `crates/toron-core/src/mailbox/messaging.rs:1148-1158` | The candidate set is every message with `ack_required=1 AND importance IN ('high','urgent')` older than the SLA. **No terminal mute, no per-scan cap.** |
| 2 | toron `messaging.rs:1211-1227` | The only dedup is a **reminder window** (`ack_sla_woken_within`, 30 min). A post nobody acks is re-woken every 30 minutes **forever**. |
| 3 | toron `messaging.rs:1126-1127` | The wake is published as **`toron_kind(9)`** - the kind-9 **ROOM** plane, the same plane as crew posts. A wake is not a chat message. |
| 4 | toron bus | `bus_events` sits at its **100,000-row cap**. |
| 5 | flywheel `crates/doorbell-rt/src/file_host.rs:29` | `WATCH_KINDS = "9,14,20001,30013,43001,30031,39002"` - **kind 9 first**. Every ack-SLA post is forwarded into the doorbell's wake stream. |
| 6 | flywheel `crates/doorbell-rt/src/brain.rs:881-897` -> `file_host.rs:1720-1759` -> `file_host.rs:200-240` | The brain is a **single serial consumer**; its 1 Hz sweep runs a **blocking** per-pin `toron mail inbox` subprocess loop **inline in the async dispatch**. The 1,024-slot channel is not drained while it runs. |
| 7 | flywheel `crates/doorbell-rt/src/file_host.rs:259, 1777-1780` | The daemon's 60 s ack-SLA tick shells `toron ack-sla` with a **15 s** timeout. One scan needs >2 min, so it is **killed every minute** and never reaches a terminal state. |

### Measurements

```
candidate posts for project flywheel (ack_required + high/urgent, > 30 min old): 2,300
  714  elm-effect-rs-wake-brain-drops-events-mcc9
  714  elm-effect-rs-doorbell-digest-project-scope-x3s2
  714  elm-effect-rs-approval-drainer-inbox-panic-erbo

bus_events rows                                  100,000   <- AT THE CAP
  of which kind 9                                 95,186
    of which "[doorbell] ack-sla-expired"         95,077   <- 99.9 % of the bus
    of which real room posts                          88

wake brain, workspace flywheel-flywheel
  forwarded      196,167
  wake_brain_full 27,791
  backpressured   25,912
  dropped          1,879
  events_lost         45

~/.local/state/doorbell/doorbell.log
  "brain flywheel-flywheel dispatch sweep still running after 2s"          x5
  "straggler loop(s) still running after 10s: flywheel/flywheel-flywheel" x4

~/.local/state/flywheel/doorbell/flywheel-flywheel.audit.jsonl
  "kind":"ack_sla_error","error":"toron spawn/timeout failed"            5,029 rows

live: /health -> degraded: ["wake_brain_dropping"]
live: `toron ack-sla --project flywheel` still running after 1m57s
```

### Impact

- **Real wake lines are dropped.** 1,879 lines never reached the brain; 45 events were
  lost upstream of the cursor. A dropped wake line means an agent is never prompted.
- **The bus is 99.9 % noise.** At the cap, every real kind-9 room post competes with
  ack-SLA posts for the same slots.
- **The scan never converges.** Killed at 15 s of a >2 min scan, it re-reads the head of
  the list every minute. `ack_sla_wakes` has grown to 2,312 rows and the flood is
  self-sustaining.
- **The blocking call inside async is the amplifier.** `run_cli` polls with
  `std::thread::sleep(25ms)` inside a plain `fn`, called straight from an `async fn`
  dispatch on a multi-thread runtime: it burns a worker thread and stops the brain
  draining. The comment at `brain.rs:873-874` already names the fix ("enables future
  off-loop pipe") - it is still a comment.

### Fix directions (not applied here)

1. Give the ack-SLA wake **its own kind**, not kind 9.
2. Give the reminder a **terminal state** (an unreadable/closed thread stops being
   re-woken) and a **per-scan emission cap**.
3. Drop kind 9 from the doorbell's `WATCH_KINDS`, or filter
   `toron-kind=ack-sla-expired` at the wake sidecar.
4. Move the per-pin inbox fetch off the brain's serial dispatch (`spawn_blocking` or a
   second lane) and give `check_ack_sla_expired` a deadline.

---

## 2. S1 - the agent-identity ladder has three different orders

The canonical order (`AGENTS.md`, flywheel skill rule "Identity is one ladder") is
**explicit -> `TORON_AGENT` -> `FLYWHEEL_MAIL_AS` -> `AGENT_NAME` -> config
`[agent_mail].name`**. Three implementations do not agree. Verified by reading each
site:

| Site | Order |
|------|-------|
| toron `crates/toron-cli/src/robot.rs:2038` `guard_agent`, `:2050` `guard_record_agent` | explicit -> `TORON_AGENT` -> `FLYWHEEL_MAIL_AS` -> `AGENT_NAME`  **(canonical)** |
| flywheel `src/authority.rs:161-165` `resolve_uncached` | **`AGENT_NAME`** -> `TORON_AGENT` -> `FLYWHEEL_MAIL_AS` (no config fallback) |
| flywheel `src/comms_send.rs:610-616` `sender_token` | explicit -> `FLYWHEEL_MAIL_AS` -> `AGENT_NAME` (**`TORON_AGENT` missing entirely**) |

With `AGENT_NAME=A` and `TORON_AGENT=B` set: toron's guard (lease holder, commit
attribution) resolves **B**; flywheel's `HostAuthority` (the doorbell actor) resolves
**A**; flywheel's comms sender resolves **A**. The lease is held under one identity
and the commit is attributed to another - precisely the seam
`resolve_mail_actor_roster`'s own doc comment names.

`flywheel/src/cmd/doorbell.rs:180` falls back to `HostAuthority` for `--as`, so the
doorbell's actor and the lease holder disagree under a two-variable environment.

Fix: one resolver, one order, used by all three sites.

---

## 3. S1 - the lease holder is an 8-char pubkey, compared against a pin

`toron` `crates/toron-core/src/mailbox/reservations.rs:1912-1918` (`fill_agent_names`)
falls back to **the first 8 chars of the pubkey** when the holder's profile name is
not resolvable. `flywheel` `src/durable.rs:3294` (`peer_held`) compares that string
against the bead's `assignee`, which is a **pin**.

When the holder's profile is absent from the project `agents` table (registered under
another project spelling, pruned, open identity mode), the board says `"a1b2c3d4"` and
the assignee says `"IvoryRidge"` -> `holder != self_pane` -> `should_resume_bead`
suppresses the bead **forever**, and `idle_pane_rotation` (`durable.rs:3409`) skips it
as peer-held. The lane starves on a bead it is actually holding.

The same fold exists in `toron` `crates/toron-core/src/mailbox/identity.rs` (the
8-char `npub[..8]` holder fallback), so both sides need the same canonicalization, not
one of them.

---

## 4. S1 - the leaf watchdog bounds the exit but not the pipe drain

`flywheel` `src/durable.rs:2239-2269`: the `try_wait` loop enforces
`DEFAULT_LEAF_WATCHDOG` (120 s) and kills the **process group** on timeout. On the
**success** path the loop breaks and then does `out_handle.join()` / `err_handle.join()`
with **no deadline**. The child was spawned `process_group(0)` with `Stdio::piped()`
(`durable.rs:2209-2211`), so a grandchild it spawned inherits the write end:
`read_to_string` never sees EOF and the join blocks forever. The watchdog is already
satisfied (the child exited), so nothing can break the hang.

This codebase already knows the class - `crates/doorbell-rt/src/file_host.rs:3783-3790`
("Reading the whole file would OOM the daemon - the file this exists to bound was
measured at 14 GB") - and solves it there by seeking to the tail. The leaf path does
not.

---

## 5. S1 - the pane dispatch retry has no timeout and the breaker outside the retry

`flywheel` `src/pane.rs:1396-1404` and `1449-1500`. The repo mandates the order in
`docs/agent-instructions/effect-kernel.md:43`: **fallback outside, breaker inside the
retry, budget timeout around the chain, attempt timeout on the call.** The pane path
inverts it:

- `cmd.output().with_context(...)?` (`pane.rs:1458-1460`) has **no attempt timeout**.
- The breaker is consulted **once before** the loop (`1396-1404`); the loop re-drives
  the call up to `MAX_ATTEMPTS = 6` (`1449-1478`) without re-checking it, so a dead
  pane is hammered 6x after the breaker opened.
- The 429 fallback (the reroute, `1548-1639`) is **inside** the retry, not outside.
- There is **no budget timeout** around the 6-attempt chain.

On the default comms path `__send` returns early (`src/main.rs:32988-32996`) with no
timeout of its own, so nothing in the chain bounds the child. The durable-leaf path
has a 120 s watchdog for exactly this class (`src/durable.rs:2170` names a measured
">300 s toron daemon reserve-write hang"); the pane path has none.

Related: the breaker counts **dispatches**, not failures. `record_failure_at` runs
once per exhausted dispatch (`pane.rs:1492-1495`), so 6 transient retries inside one
dispatch cost the breaker a single strike, and the breaker is never consulted between
attempts - so "breaker inside the retry" does not hold in either sense.

---

## 6. S1 - the yield board is raw-keyed while the lease half of the same join is canonicalized

`toron` `crates/toron-core/src/mailbox/yields.rs:120-145` (`reservation_yield_board`):

- line 124 `self.list_reservation_yields(project_slug)` -> `yields.rs:90`:
  `WHERE project_slug = ?` - **raw**, no canonicalization.
- line 126 `self.active_reservations(project_slug)` -> `reservations.rs:1542`:
  `let project_slug = &self.canonical_project(project_slug).await;` - **canonicalized**.

One join mixes a raw-keyed table with a canonicalized one. The write side is raw too:
`toron-cli/src/robot.rs:1106` passes `&args.project` straight through.

`flywheel` reads the two halves with two different keys: the lease half uses the
absolute path (`src/main.rs:22797` `project_dir_str`), the yield half the bare label
(`src/main.rs:22831` `mail.yielded_pairs(project)`).

Live data today is all short slugs (`reservation_yields`: 11 rows, `elm-effect-rs` x10
and `yield-probe` x1; `file_reservations`: `flywheel` x1467, `elm-effect-rs` x1343,
...), so this is **latent**: it fires the moment a yield is recorded with a path-form
or alias project spelling. Then the marker is invisible to the join, the peer-hold
suppression never fires, and the loop resumes a bead a live peer holds.

---

## 7. S1 - the gate-verdict guard has an ordinary-flag bypass

`beads` `src/cli/commands/doctor.rs:3309-3313`:

```rust
pub fn gate_verdict_rebuild_refusal(gate_rows: usize, allow_repeated_repair: bool) -> Option<String> {
    if allow_repeated_repair || gate_rows == 0 {
        return None;
    }
    ...
}
```

The call site (`doctor.rs:14061`) passes `args.allow_repeated_repair` straight
through, and the flag (`src/cli/mod.rs:3359-3361`) carries **no `BR_OPERATOR` gate**
(the only `BR_OPERATOR` checks in the tree are `src/cli/commands/close.rs:1391, 1618`).
It is not `conflicts_with = "repair"`, and its help text says only "Allow another JSONL
rebuild after prior failed recovery evidence" - nothing about verdict destruction.

The chain, verified end to end:

```
1. br doctor --repair --allow-repeated-repair
2.   -> the refusal short-circuits on the flag (doctor.rs:3310)
3.   -> rebuild_database_family recreates gate_result_history EMPTY
4.   -> doctor NEVER reloads .beads/gates.jsonl
        (load_gates_sidecar_if_present is called only from sync.rs:3528, 3668)
5.   -> the next `br sync --flush-only` calls export_gates_to_jsonl
        (sync.rs:2541, 2674), which writes the DB's gate rows over the sidecar
6.   -> .beads/gates.jsonl is REWRITTEN with zero rows
7.   -> every closed bead reports "no legal verdict", and closed -> closed
        is not a legal gate transition, so there is no repair short of
        reopen -> re-gate -> re-close for each one
```

Blast radius on this repo: **744 gate verdict rows** live in `.beads/gates.jsonl`
right now. The `br` skill's own close ceremony ends with `br sync --flush-only`, so
step 5 is not an edge case - it is the next command in the standard sequence.

The flywheel AGENTS.md states: "`doctor --repair` now counts the gate rows it would
destroy and refuses, naming the count". The guard exists and works - and it is
bypassable by one ordinary flag with no operator gate and no hint in the help text.

---

## 8. S1 - three more rebuild paths with no verdict guard

The ledger-repair refusal lives **only** in `doctor.rs`. `grep` for
`gate_verdict_rebuild_refusal` / `gate_verdict_row_count` in
`src/cli/commands/sync.rs` returns **nothing**. Three paths reach
`rebuild_database_family` without ever consulting it:

- `src/cli/commands/sync.rs:3641-3652` - `br sync --force` / `--rebuild` replaces the
  whole DB family from the issues JSONL. (Partial mitigation: `sync.rs:3668` reloads
  the gates sidecar afterwards, so this path restores the rows from the file - unless
  the file is already gone.)
- `src/cli/commands/mod.rs:626-645` -> `src/config/mod.rs:2274` - the automatic
  recovery path (`retry_mutation_with_jsonl_recovery` -> `recover_database_from_jsonl`
  -> `rebuild_database_family`) rebuilds the DB on a corruption-class write error
  **with no verdict guard**. `br close` runs through this path
  (`update_issues_atomically_with_recovery`), so a single corrupt-write recovery
  empties the live verdict store mid-close.
- `src/cli/commands/sync.rs:3166-3202` - `repair_import_integrity_if_needed` rebuilds
  whenever a post-import `integrity_check` is dirty, same unguarded emptying.

The old DB survives only in a recovery directory.

---

## 9. S1 - one orphan FK row aborts the whole gates-sidecar reload

`src/cli/commands/sync.rs:1888-1892`: the gates-sidecar reload failure is downgraded
to `warn!` and swallowed ("the issues import must not be rolled back because a sidecar
row went stale").

But `insert_gate_result_record` (`src/storage/sqlite.rs:4319-4379`) inserts without
checking that `issue_id` exists, and the table carries
`FOREIGN KEY (issue_id) REFERENCES issues(id)` (`src/storage/schema.rs:25`) with
`PRAGMA foreign_keys = ON` (`src/storage/schema.rs:1237`). **One orphan row aborts the
entire sidecar reload** with an FK error -> every verdict row is lost, with only a
warn-level log and every closed bead reporting "no legal verdict".

The doc comment on `import_gates_from_jsonl` (`src/sync/mod.rs:11740-11743`) claims
"rows referencing unknown issues are refused rather than dropped". The body
(`src/sync/mod.rs:11779-11788`) does no such per-row check; the only refusal is the
whole-batch FK abort above. The documented contract is not the implemented one.

---

## 10. S2 - `flywheel arrive` refuses a correct arrival

`flywheel` `src/main.rs:15932-15937`:

```rust
if registry_pin.is_some() && captain_pin == Some(requested) {
    return ArriveIdentityVerdict::Refused { ... };
}
```

The guard fires whenever the requested pin equals the captain alias **and the pane has
a registry row** - including when the requested pin **is that pane's own injected
identity**. Measured on this session:

```
$ flywheel arrive flywheel --as quiet-spark
refused: the captain pin is not a valid arrival target for a spawned pane (injected: quiet-spark)
```

The pane's own pin is `quiet-spark` (registry row for pane `w22:p3`) and the project
captain alias is also `quiet-spark`. The correct arrival is refused, and the error
tells the operator to re-run with the pin it just rejected.

Fix: one clause - the refusal must also require `requested != injected`.
`src/main.rs:21137-21166` covers the other three shapes (match, wrong pin, captain
pin != injected) but never this one.

---

## 11. S2 - the captain write gate refuses a fold that does route

`toron` `crates/toron-cli/src/robot.rs:2627-2644` refuses `contact captain --pin` in
open identity mode:

> captain fold is swarm-only (ADR-0018): project flywheel runs identity_mode=open,
> **where the fold can never route**

**That sentence is false.** The same repo says the opposite twice:

- `toron-core/src/mailbox/identity.rs:1043-1048` - "role and captain aliases are
  authoritative in **BOTH modes** ... Check alias first."
- `toron-core/src/mailbox/messaging.rs:1651-1660` - "the alias table folds there too,
  so `send --to captain` resolves like `whois captain`" (the open-mode fix, labelled
  with the alias-table ticket).

Live proof: `toron mail whois flywheel captain` resolves to `quiet-spark` **right now**
in the open-mode `flywheel` project.

- The gate blocks a legitimate operator action (`--force` is the only way through).
- The **read** path (`robot.rs:1695-1702`) applies no mode gate, so
  `toron contact captain --project flywheel` reports a captain the same command
  refuses to write.

The unit test at `robot.rs:2653-2662` pins the wrong behaviour ("open mode must
refuse the fold write"), so a false invariant is guarded by a test.

---

## 12. S2 - the state lock's stale-steal is a TOCTOU

`flywheel` `src/state.rs:1501-1538`: `try_acquire` sees `AlreadyExists`, checks
`lock_is_stale` (mtime older than 10 s), then `std::fs::remove_file(&lock)`. The
metadata read and the remove are not atomic:

- A removes the stale lock and `create_new`s its own.
- B read the *old* metadata a moment earlier, so it removes **A's fresh lock** and
  `create_new`s its own.

Both believe they hold it; both run `update_locked`; last writer wins. `Drop for
StateLock` (`state.rs:1527-1531`) also removes the path unconditionally, so A's drop
can remove B's lock. Every state write that goes through `update_locked` (resume
window, spawn registry, loop pids) is exposed. The 10 s staleness threshold also
means a legitimate slow write (>10 s) has its lock stolen.

---

## 13. S2 - the inject budget is an unlocked, non-atomic read-modify-write

`flywheel` `src/doorbell.rs:648-666`: read the budget map, `stamps.retain(...)`, check
`len >= 3`, `push(now)`, `std::fs::write(budget_path, ...)`. Three defects in one
place:

- `std::fs::write` truncates in place: a crash mid-write leaves a corrupt file, and
  `inject_budget_load` (`doorbell.rs:607-611`) `unwrap_or_default()`s it to an empty
  map - **the budget silently resets**.
- The read-modify-write has **no lock**, while every other state file in this repo
  uses `update_locked` (`state.rs:1544`). A daemon injector and a CLI inject both read
  `len == 2` and both write `len == 3`; 4+ injections land.
- The budget is written **before** the journal append (`doorbell.rs:675`), so a
  failure after the write consumes a slot for an inject that never happened.

---

## 14. S2 - the durable-intent journal is one fixed file, deleted on success

`flywheel` `src/elm/leaf.rs:238-249` + `192-194`: `run_leaf_journaled_at` opens
`<state>/loops/<project>.leaf.journal` via `FileLog::open` and, on success,
`std::fs::remove_file(&path)`. `run_leaf_journaled` is called from 11 sites in
`src/durable.rs` (7335, 7379, 7659, 7795, 7826, 7849, 7934, 7978, 8008, 8056, 8128).

Two overlapping passes (or a pass plus the cron daemon) open the same `FileLog`: the
first to finish unlinks the path the second is still appending to. The journal is the
"intent before the effect" record the module's own doc (`leaf.rs:1-14`) promises;
losing it loses the audit trail for a real effect. The intent is also **not keyed per
attempt**, so two leaves in the same project share one file.

---

## 15. S2 - flywheel declares a second `EffectState`

`flywheel` `src/agent_ref.rs:241` declares
`enum EffectState {Prepared, InFlight, Settled, Failed, Unknown}` and persists it
inside `RebindIntent` into flywheel's own event log (`agent_ref.rs:526`
`journal_rebind` -> `state::log_event`).

Beside it: toron-core's persisted enum (`crates/toron-core/src/index/workflow_effects.rs:162`,
9 states) and the framework's (`elm-effect-rs/crates/elm-effect-journal/src/lib.rs:39`,
9 states).

This is on the elm-effect skill's hard **Never** list ("A second `EffectState`").
The flywheel copy is a 5-state subset: it cannot express `Accepted`,
`ReconcilePending`, `Cancelled`, or `Parked`, so a flywheel rebind row and a
workflow-effect row of the same effect are not comparable, and the two enums can drift.

Everything else on the flywheel "Done" checklist is **satisfied** (verified):
`decide` reads no clock/env/rand; effectful leaves go through the journal; the worker
count and queue bound are still 4 and 4096 (`effect_journal.rs:1719` asserts it);
`resonate_sdk` is still a dependency; the leaf covers success / retry-then-success /
`unknown`-not-success.

---

## 16. S2 - the bead-id citation is case-insensitive in beads, case-sensitive in flywheel

`beads` `src/cli/commands/close.rs:192` `sha_message_cites_bead` is explicitly
**case-insensitive** (`to_lowercase()` on both sides, word-boundary aware).
`flywheel` `src/verdict.rs:557` `derive_head_sha` uses
`git log --all --grep=<bead_id>` - git's `--grep` is **case-sensitive**.

Verified empirically in a scratch repo:

```
$ git commit -m "fix: thing (BD-2QU9)"
$ git log --all --grep=<the same id, lowercased> --oneline
(empty - no match)
$ git log --all --grep=<the same id, lowercased> --regexp-ignore-case --oneline
e08f7c2 fix: thing (BD-2QU9)
```

So a commit citing `BD-2QU9` **closes legally at beads** and is **invisible to
flywheel's ledger check** ("SHA rule: no commit carries the bead id"). A closed bead
has no legal gate transition, so it cannot be repaired without
reopen -> re-gate -> re-close.

Same concept, two matching rules. Fix on either side (add `--regexp-ignore-case` to
the grep, or make beads case-sensitive) - but they must agree.

---

## 17. S2 - beads binds the sha you pass, flywheel derives it

`beads` `src/cli/commands/close.rs:290` `validate_close_sha` binds **the sha the
operator passes** to `br close --commit-sha`. `flywheel` `src/verdict.rs:557-620`
**derives** the sha by ranking: bracketed-subject citation > bare subject > body-only,
newest-first inside a tier, skipping bookkeeping-only commits.

When two commits cite the bead, beads binds one and flywheel binds the other: a later
sibling-lane commit carrying a bracketed bead id in the subject outranks the close
commit, so `flywheel ledger check` looks for a gate row at a sha that was never the
close's sha -> "no legal verdict bound to sha ..." for a legal close.

---

## 18. S2 - a `policy.yaml` with `strict: true` and no `gates:` turns the fail-closed close OFF

`beads` `src/cli/commands/close.rs:1599-1600`:

```rust
let fail_closed_default = !beads_dir.join(POLICY_FILE_NAME).exists()
    || close_policy::workflow_gating_unconfigured(&policy_doc.workflow);
```

and `src/close_policy.rs:2959-2961`:

```rust
pub fn workflow_gating_unconfigured(workflow: &Workflow) -> bool {
    !workflow.strict && workflow.gates.is_empty()
}
```

So a policy file that sets `workflow.strict: true` and declares **no** `gates:` block
gives:

- `workflow_gating_unconfigured` = `!true && ...` = **false**
- `fail_closed_default` = **false**
- `workflow.gates_enforced()` = `strict && !gates.is_empty()` = **false**
- `close_policy.is_active()` = false by default
- -> `policy_active == false` -> `evaluate_close_policy` (`close.rs:1807`) never runs
  and **no verdict row is required to close**.

Worse: `br gate report --to closed` is **also** refused in that configuration
(`src/cli/commands/gate.rs:380-387`, `resolve_gate_target` finds no rule), so the
gate discipline is not merely unused - it is unreachable. A project that writes
`strict: true` believing it turns strictness ON has turned the fail-closed default
OFF and cannot record a verdict even if it wants to.

(This repo's live `.beads/policy.yaml` does declare a `gates:` block with
`require_legal_close: true` for both `open -> closed` and `in_progress -> closed`, so
the live flywheel project is not in this state. The trap is in the default path for
any project that writes a policy file by hand.)

---

## 19. S2 - the sha-citation check is warn-only when the sha resolves nowhere

`beads` `src/cli/commands/close.rs:1579-1587` + `166-168`: when the sha resolves in
no searched repo (`ShaVerdict::Unresolvable`), the close **proceeds with a warning**.
A close therefore succeeds with a sha that names a different bead, or with no
verifiable message at all.

The other half is fail-closed and fine: a sha that *does* resolve but does not cite
the bead is a hard error (`close.rs:298-303`).

---

## 20. S2 - beads accepts a `.beads/**`-only commit as "carries work"; flywheel skips exactly those

`beads` `src/cli/commands/close.rs:332-351` `commit_changes_nothing` rejects only a
commit that changes **no files at all**. A commit that touches only `.beads/**`
bookkeeping passes beads' "carries work" test.

`flywheel`'s `derive_head_sha` (per `flywheel/AGENTS.md` §BEADS) **skips**
`.beads/**` and `.flywheel/**` commits. So the two sides disagree on which sha binds
a bead: beads will accept a sha that flywheel's ledger check refuses to bind.

---

## 21. S2 - the journaled engine constructor falls back to `Recording`

`flywheel` `src/pane.rs:2300-2319` (`local_engine_with`): when `journals()` cannot
open the toron index, the **journaled** constructor drops the runner entirely
(`effect = None`) and picks `Recording`, not `Enforced`. That contradicts flywheel's
own AGENTS.md ("a journaled constructor is `EffectJournalMode::Enforced`" and "an
effectful leaf with no injected runner **refuses**").

No bypass (the leaf still fails), but the run writes **no `workflow_runs` row** and
the operator sees a generic "missing sink" instead of the explicit Enforced refusal.

Related: `toron` `crates/toron-workflow/src/engine/control_flow.rs:546` defines
`EFFECT_JOURNAL_SINK_UNCOVERED = "bypassed-sink-uncovered"` and documents at `:539`
that flywheel's mailbox-without-an-in-process-plane path emits it. Flywheel's
partial-coverage case **refuses** instead (`src/effect_journal.rs:247` + `:311`), so
**neither repo ever emits the marker** - a consumer keyed on that string sees nothing,
and `toron/crates/toron-workflow/src/engine/boundary.rs:32` documents a contract no
side implements.

---

## 22. S2 - the spawn registry is append-only and resolves first-match

`flywheel` `src/state.rs:755-774` + `src/main.rs:15988-15993` resolve a pane's pin with
`.find(|e| &e.pane_id == pane)` - **first row wins**. The registry is never
deduplicated.

Measured across `~/.local/state/flywheel/agents/*.json`:

```
75 registry rows
12 pane_ids carry >1 row  (22 extra rows)
flywheel-d5080499.json : same mail_identity on two rows per pane; w22:p1 has 2 rows
dotfiles-46a7d24f.json  : 7 rows for one pane_id (w6:pC)
```

When a pane carries two identities, `--to <pin>`, `flywheel rebind`, and the arrive
guard all read whichever row was written first. That is the identity-divergence
hazard the global AGENTS.md describes, produced by the registry itself.

---

## 23. S2 - the toron daemon runs a 3-day-old build

```
$ toron daemon status --json
"verdict": "replaced", "restart_required": true
daemon booted 2026-10-18T02:52:43Z; file on disk built 2026-10-18T09:06:28Z

$ toron doctor
FAIL daemon_build - build - REPLACED ...
```

The doorbell daemon shells the **current** `~/.cargo/bin/toron` for its inbox and
ack-SLA arms (`/health.binaries.toron` = that path) while the toron **daemon** holds
the same sqlite with the old code: two processes sharing one store, different builds.

Scope, so this is not over-claimed: the store is at migration **27** and the current
source ships migrations up to `0027_comms_lead.sql`, so both builds agree on the
schema - this is a **behavioural** skew (the daemon misses ~6 h of fixes), not a
`store_skew` schema refusal. The `store_skew` probes stay green here, which is why
`daemon_build` is the check that catches it.

---

## 24. S2 - the shipped claim recipe omits `--claim`

`beads` `src/cli/commands/update.rs:1400` (`expect_unassigned: args.claim`) and
`src/storage/sqlite.rs:7404-7429` (the "issue already assigned to X" guard) run the
atomic claim check **only under `--claim`**.

`flywheel` `src/stages.rs:326` (the ce-execute dispatch contract) ships
`br update <id> --status=in_progress --assignee <herdr-name>` - **no `--claim`**. So
the atomic guard never runs on the dispatch path and a second lane **overwrites** the
first lane's assignee; both work the bead.

Flywheel's *other* recipe (`src/durable.rs:5280` `claim_nudge_prompt`) does use
`--claim --actor "$AGENT_NAME"` and is guarded. Two contradictory claim recipes, the
unguarded one in the dispatch contract.

Two more claim-dialect problems in the same area:

- `beads` `src/agent_registry.rs:143` `fold_assignee_for_store` **refuses** an
  explicit `--assignee` shaped like a herdr pane name unless it is registered in
  `.flywheel/agent-names.json` - while `flywheel/src/stages.rs:326` says the assignee
  is "the herdr slug ... **never a pin**". The contract names the one dialect beads
  refuses. An unregistered slug makes the claim hard-fail (the bead is worked
  unclaimed); a registered one makes beads store the **pin**, contradicting the
  instruction.
- `beads` `src/config/mod.rs:5801` `resolve_actor` falls back to **`$USER`**, and
  `src/cli/commands/update.rs:1340` stores the actor as the assignee under `--claim`.
  `toron-core/src/mailbox/identity.rs:1064-1077` rejects a username-class name as a
  mailbox identity ("do not register your username"), and
  `flywheel/src/durable.rs:3418, 3457-3474` matches the assignee against a live pane
  pin. A `$USER` assignee matches no live pane, so `idle_pane_rotation` rule 4 ("a
  ready bead whose assignee is NOT a live pane") hands the bead to a **different** pane
  while the first lane still holds it.

---

## 25. S3 - waste and latent risk

- **Hyphen pins are live and unfixable by the current tools.** `toron agent bootstrap`
  refuses any shape that is not CamelCase AdjNoun, but the registries still carry
  hyphen slugs (`toron.dev-4f8867ee.json` -> pane `w1W:p3` -> `swift-otter`). Those
  rows cannot be re-created, so the holder cannot onboard and their leases are held
  under an identity the policy forbids. This session's own pane carried `quiet-spark`
  and had to be migrated by hand (registry row rewritten + `toron agent bootstrap`).
  The old identity still holds `docs/analysis/resonate-inventory.md` (reason
  the open elm-effect integration bead that owns it, expiry 2027) -
  `toron reserve sweep --closed-beads` will not release it
  because the bead is open.
- **6 leftover legacy slug identities** (`toron doctor`): `flywheel-flywheel-oc2 ->
  BlackGorge`, `flywheel-toron-oc -> SandySparrow`, `flywheel-toron-oc3 -> TanFern`,
  `flywheel-toron-oc4 -> PinkCastle`, `dbproof19797:captain -> ProudLeopard`,
  `toron:captain -> CoralHarbor`. Each is a second inbox for a folded slug.
  `toron agent prune --project flywheel` is the fix; it reports `kept`/`held` rows
  that need a human.
- **~100 fixture project directories** in `~/.local/share/toron/projects/`
  (`probe-a`, `probe-blocked`, `sNNNNNN`, `fw-gate-*`, `tmp.*`) share the production
  store the ack-SLA scan and the agent scans walk.
- **Unbounded full-file reads on hot paths.** `flywheel/src/doorbell.rs:624` reads the
  whole workspace journal to look at 32 lines (the journal is rotation-bounded to
  4,000 lines, so it is a few hundred KB per inject - a tail seek would be O(1), and
  `rotate_audit_if_oversized` already does that at `file_host.rs:3790-3801`).
  `flywheel/src/agent_ref.rs:717-741` parses the entire project log to find one row by
  `effect_key`, on every reclassify, on a log with no rotation.
  `flywheel/src/state.rs:1843-1862` `read_log` is an unbounded full-file JSONL parse
  behind `workflow history`, `runs list`, `--robot-snapshot`, `cron history`.
- **`record_pane_starvation` writes a fixed temp path with no lock**
  (`flywheel/src/state.rs:1631-1640`), unlike its sibling `record_resume` which uses
  `update_locked` with a pid-suffixed temp name. Two concurrent loop passes write the
  same temp file -> interleaved bytes -> `load_pane_starvation` `unwrap_or_default()`s
  it to an empty map -> the starvation counter resets and the window exemption never
  fires.
- **`submit_journal_wait` blocks a Tokio worker on a thread join**
  (`flywheel/src/effect_journal.rs:114-118`): `std::thread::spawn(...).join()` inside
  a runtime. Under wave-swarm fan-out this parks runtime workers - the exact case the
  module doc cites as its motivation.
- **The breaker never clears its failure count at cooldown**
  (`flywheel/src/breaker.rs:46-58`): `allows_at` sets `half_open = true` and returns
  true; `failures` is cleared only by `record_success_at` (`66-70`).
- **No version check on `br`.** flywheel resolves `br` from `PATH` with no
  `br --version` call anywhere, so a stale `br` is undetectable. `toron` is a **path**
  dependency with no version constraint (`flywheel/Cargo.toml:52-58`), so a local edit
  in `../toron` silently changes flywheel's build.
- **`flywheel briefing --json` costs ~2.9 s** (measured), and it is the session door.
  ~1 s is CPU in-process; the rest is subprocess round trips (herdr, toron, br).
- **Every beads command re-hashes the whole issues JSONL.**
  `src/sync/mod.rs:10788-10791` `compute_jsonl_newer_impl` calls
  `compute_jsonl_hash(jsonl_path)` - a full read plus SHA-256 - on **every** command,
  read-only ones included (`src/main.rs:525, 532, 1466`). The comment justifies it
  (mtime and size are mutable), but a 50 MB `issues.jsonl` is fully re-hashed per
  invocation.
- **`br ready` is O(n^2).** `src/storage/sqlite.rs:10646-10652` runs a correlated
  `NOT EXISTS` over `issues` per candidate row, and there is **no index on `wave`**
  (`src/storage/schema.rs` indexes status / priority / assignee / created_at /
  updated_at, none on `wave`).
- **Every flush rewrites the whole gates sidecar.**
  `src/sync/mod.rs:11689-11727` `export_gates_to_jsonl` reads
  `storage.get_all_gate_result_records()` (the entire append-only history), builds a
  `BTreeMap` of every line in memory, and rewrites `.beads/gates.jsonl`. Flush cost
  grows without bound over a repo's life.

### Unwired / dead (flywheel)

- `src/cmd/workflow.rs:224-225` - `allow_crew_expand` is parsed by clap and **never
  read** anywhere in the workspace. The doc comment says it is kept so old argv does
  not hard-bail, i.e. an intentional no-op flag.
- `src/pane.rs:2287-2304` - `local_engine_with`'s `journal: bool` parameter is only
  ever called with `true` (`src/pane.rs:2256`); the `else { None }` branch
  (`2302-2304`) and the `mode = Recording` fallback for `journal == false`
  (`2315-2319`) are unreachable.
- `src/breaker.rs:41-44, 61-64, 73-76` - `allows`, `record_success`, `record_failure`
  are `#[allow(dead_code)]` shims with zero callers; every live site uses the `*_at`
  variants.
- `src/durable.rs:6164-6171` - `reclaim_swarm_secs` / `reclaim_human_secs` are
  `#[allow(dead_code)]` env-reading adapters with no callers; the live loop uses
  `ReclaimPolicy::from_environment()` once at loop start (`src/main.rs:23529`).

### Docs-vs-code mismatches (flywheel AGENTS.md)

- `AGENTS.md:51` - "**`0|false|off`, any other set env value**, or `inbox_log = false`
  turns it off." The code turns it **on** for `1|true|on`
  (`crates/doorbell-rt/src/sinks.rs:455-461`, toron-core `inbox::resolve_enabled`).
  `src/comms_send.rs:52` states the rule correctly - AGENTS.md is the wrong copy, and
  it is the copy an operator reads.
- `AGENTS.md:84` - "`ConsecutiveBreaker` **clears its failure count when the cooldown
  elapses**, then judges the next call as closed." `src/breaker.rs:46-58` does not
  clear `failures` at cooldown, and the call after the cooldown is judged **allowed**
  (the half-open trial), not closed.
- `AGENTS.md:39` - "`derive_head_sha` walks the `--grep` matches **newest-first** ...
  and falls back to the **newest** bookkeeping match." `src/verdict.rs:614-627` ranks
  by **citation tier first** (bracketed subject > bare subject > body), newest-first
  only inside a tier, and the fallback is `ranked[0]`, the highest-**ranked** match.
- `AGENTS.md:39` - "`derive_head_sha` ... skips bookkeeping-only commits
  (`.beads/**`, `.flywheel/**` - **nothing outside those two**)". `src/verdict.rs:477-503`
  implements the exclusion via the `BOOKKEEPING_PATH_PREFIXES` constant; the
  parenthetical is a claim about that constant, and it silently changes if the
  constant grows.

### Docs-vs-code mismatches (beads, and flywheel's BEADS section)

- `flywheel/AGENTS.md` §BEADS - "A bead's ledger binding is resolved by SEARCHING
  COMMIT MESSAGES ... re-gate at the newer sha rather than hunting for a cause" is
  **not executable in beads**: `br gate report` on a closed bead is refused because
  `closed -> closed` is not a legal transition (`src/cli/commands/gate.rs:380-387`
  with the default policy's `closed: [open]` map at `src/close_policy.rs:786`). beads
  is right per ADR-0001 §5.3; the flywheel-side repair advice names a step the ledger
  forbids.
- `flywheel/AGENTS.md` §BEADS - "the gate row's `--note` must carry `sha=<FULL 40-hex
  sha>`" is enforced only in flywheel (`src/cli/commands/gate.rs:121-183` warns and
  does not refuse). beads' `--task-id/--verdict-id` proved-close path
  (`src/cli/commands/close.rs:92-115, 1628-1644`) closes a bead with **no** sha at
  all and a note that has no `sha=` token, so a proved close is legal in beads and
  clean-false in flywheel's `ledger check`.
- `beads/AGENTS.md` §"The elm-effect core" - "one `BEGIN IMMEDIATE` that also
  enforces the transition map, required fields, and scoped gate rules" is accurate for
  the transition map and required fields (`src/storage/sqlite.rs:7100-7132`), but the
  ADR-0001 §5.3 **legal-close** rule is enforced only in the CLI layer
  (`src/cli/commands/close.rs:514-536` -> the fold), never inside the write
  transaction; the in-transaction gate check is gated on
  `workflow.gates_enforced()` (`src/storage/sqlite.rs:7116`), which is false under
  the fail-closed default. The "one transaction" framing overstates the atomicity.
- `beads/AGENTS.md` §"The Close Ceremony" - the close does not cross-check that the
  gate row's `sha=` token equals the `--commit-sha` it was given; that binding lives
  entirely in flywheel's `ledger check`.
- `beads/src/cli/mod.rs:3360` - the `--allow-repeated-repair` help text ("Allow
  another JSONL rebuild after prior failed recovery evidence") does not mention that
  it also disables the gate-verdict-destruction refusal. The flag name gives no hint
  that it is a verdict-loss escape hatch.

---

## 26. What was checked and found clean

- **Reducer purity (the Tick Law) holds.** `flywheel/src/workspace.rs:1567`
  `workspace::tick`, `crates/doorbell-core/src/lib.rs:760` `doorbell_core::tick`, and
  `flywheel/src/durable.rs:577` `durable::tick` are pure: no clock, env, randomness,
  lock, or `&mut`; the clock enters only as a `Msg` payload. `BTreeMap`/`BTreeSet`
  where iteration order reaches output.
- **`flywheel/src/decide.rs:983`** - earliest position wins, then longest needle, then
  an explicit `none`; `check_routing_table`/`check_catalog` fail loud on overlap.
- **Run-ledger honesty holds.** `src/main.rs:28169` `history_row_status` backs
  `workflow history`, `runs list`, `--robot-snapshot`, and `cron history`; a row with
  no recorded status reads `unknown` everywhere and annotation rows are skipped by
  `cron history` (`src/cron.rs:1384`).
- **Journal-before-provider holds on the flywheel path.** `src/effect_journal.rs:213-305,
  366-390`: prepared -> claim -> provider invoked **once** via `FnOnce` -> settle;
  uncovered sinks fail closed via `refuse_unjournaled` (`311-321`);
  `Ambiguous` and `ProvenFailed` are distinct (`726-731`). `Enforced` vs `Recording`
  is selected correctly at `src/pane.rs:2315-2319` (the one exception is finding 15).
- **`flywheel/src/verdict.rs:557`** - `git log --all --grep`, no bare-HEAD fallback,
  empty commits refused, bookkeeping-only commits skipped with a fallback.
- **`flywheel/src/elm/leaf.rs:83-108`** - the leaf fold is terminal-once, so no double
  settle; `run_leaf_cmd` has a 120 s watchdog and kills the process group.
- **`flywheel/src/cmd/captain.rs:177` `stall_verdict`** and
  **`src/durable.rs:2381` `dead_stop_action`** - one escalation per stall, `Hold`
  afterwards, an unreadable fleet or a repo with no history never alarms.
- **Corr authority holds.** toron's `Corr`/`ThreadId` newtypes reject empty at parse
  and `send_message` fails loud on conflation; every flywheel mint site is non-empty
  and never equals a `thread_id`.
- **The two bead streams stay separate.** No reader infers a gate verdict from
  `.beads/issues.jsonl`: toron's tracker reads issues only
  (`crates/toron-cli/src/tracker.rs:17`), beads refuses a JSONL rebuild that would
  drop gate rows (`src/cli/commands/doctor.rs:3258-3322`), and flywheel uses
  `br list --all --json` (`src/main.rs:37369`).
- **The beads fold is pure.** `beads/src/elm/tests.rs:153` folds the same event log
  twice into the same state and commands; `:203` a close with no PASS row leaves the
  status open; `:280` importing issues without `gates.jsonl` leaves the verdict table
  empty. All three "Done" items on the beads checklist are **satisfied**.
- **The toron "Done" checklist is satisfied**: one persisted `EffectState`
  (`crates/toron-core/src/index/workflow_effects.rs:162`; `boundary::EffectState` is
  gone), `plan_attempt` parity against the framework on every transition pair
  (`crates/toron-workflow/src/engine/elm.rs:64-79`), `PARK_MAX_WAIT_SECS` still 86400
  (`engine/park.rs:70`), and a replay digest mismatch mints no second effect key
  (`engine/reconcile.rs:132`).
- **Every flywheel -> toron/beads/doorbell/herdr CLI argv matches the real surface.**
  All 8 herdr verbs flywheel invokes exist on this machine (`attach`, `api schema`,
  `agent list`, `agent prompt`, `agent rename`, `agent status`, `pane list`,
  `workspace list`).
- **Stores are healthy.** `flywheel` beads ledger: `scripts/check-beads-ledger.sh
  --history` -> "OK: no row-set regression vs HEAD". `beads`: `br doctor` exits 0,
  1,096 records in both sqlite and JSONL and in sync. `toron`:
  `sqlite.integrity_check` ok, `name_collisions` ok, `identity_divergence` ok.
- **The doorbell injection probe behaves as designed.** `pins 17 / panes_seen 1 /
  injectable 0` does **not** degrade, because the one known pane is working - the
  documented "all-working fleet stays healthy" rule.
- **The wake-drop policy itself is sound.** Intent is recorded, drops are counted
  exactly, the replacement resumes from the last accepted cursor, and `unknown` is
  never reported as success. The defect is upstream of it.
- **The beads fold is pure and its gates are separate.** `beads/src/elm/mod.rs:227-295`
  `update` reads no clock, env, or DB; the model is `BTreeMap` throughout
  (`src/elm/mod.rs:122-133`); `BeadsMsg::GatesLoaded` is the only writer of
  `model.gates` and `BeadsMsg::Event` cannot mint a verdict.
  `src/elm/tests.rs:153` folds the same event log twice into the same state and
  commands; `:203` a close with no PASS row leaves the status open; `:280` importing
  issues without `gates.jsonl` leaves the verdict table empty.
- **beads' close policy is fail-closed in the default configuration.**
  `src/close_policy.rs:1036-1099` `evaluate_require_legal_close` refuses when no
  legal PASS row exists and refuses on any FAIL row for a legal kind;
  `src/cli/commands/close.rs:1628-1644` refuses without `--commit-sha` unless a
  proved pair is supplied; `--bypass-policy` requires `BR_OPERATOR=1`
  (`close.rs:1618-1624`).
- **beads' punctuation and cycle rules hold.** `sha_message_cites_bead` /
  `is_bead_id_glue` / `after_char_is_glue` (`src/cli/commands/close.rs:174-207`) treat
  `.` and `:` as punctuation, keep a dotted child id from citing its parent, and read
  the **full** message via `git log -1 --format=%B`. The dependency cycle check runs
  inside the write transaction on every add path
  (`src/storage/sqlite.rs:12940-12957`, `6850-6863`), BFS depth-capped at 500.
- **beads' waits are bounded.** Write-transaction retry is capped at 8 attempts with
  jittered backoff (~12.7 s, `src/storage/sqlite.rs:3022-3043`); flock acquisition is
  bounded by `DEFAULT_WRITE_LOCK_TIMEOUT_MS = 30_000` with a 25 ms poll
  (`src/sync/mod.rs:54-55, 1377-1413`); id-collision retry is capped at 10.
- **beads has no dead CLI surface.** All 49 `*Args` structs were checked
  programmatically: every field is consumed at least once outside its own definition.
  `br --robot-triage/next/plan/insights` all land on JSON-emitting verbs
  (`src/main.rs:44-58`). `br q` rows are `Status::Draft` with no verify/principles,
  so they are excluded from `ready` - and `br update --verify/--principle`
  (`src/cli/mod.rs:1373-1394`) makes them dispatchable, so "quick-capture rows that
  can never become ready" does not hold.

---

## 27. Operator actions (not taken here)

| Action | Why | Risk |
|---|---|---|
| `toron daemon restart` | the daemon runs a 3-day-old build (`toron doctor` fails) | brief mailbox outage; the doorbell's `toron` CLI arms keep working |
| stop the ack-SLA flood (terminal state for unacked posts, or a kind that is not 9) | the bus is 99.9 % noise and the wake brain is dropping lines | needs a decision on which of the 2,300 posts are still live |
| `toron agent prune --project flywheel` | 6 duplicate slug identities | prune reports `kept`/`held` rows and asks for a human on those |
| `toron reserve release` for the stale `quiet-spark` lease on `docs/analysis/resonate-inventory.md` | held by an identity the policy forbids | that file is another lane's work - check the bead first |
| restart the doorbell daemon | it has been up 3 d 15 h and is the component dropping wakes | wakes pause for the drain window |
| **do not** run `br doctor --repair` on this repo | `--allow-repeated-repair` bypasses the verdict guard; 744 gate rows are at risk | the guard works only while the flag is absent |
| gate `--allow-repeated-repair` behind `BR_OPERATOR` in beads | it is the only unguarded verdict-loss path | needs a beads release |

---

## Appendix: how the live numbers were taken

```bash
curl -s http://127.0.0.1:18980/health | jq .wake_brain          # counters + degraded
sqlite3 ~/.local/share/toron/index.sqlite \
  "SELECT COUNT(*) FROM bus_events;
   SELECT COUNT(*) FROM bus_event_tags WHERE tag_value='ack-sla-expired';"
grep "still running after" ~/.local/state/doorbell/doorbell.log  # parked brain dispatches
grep -c ack_sla_error ~/.local/state/flywheel/doorbell/flywheel-flywheel.audit.jsonl
toron daemon status --json; toron doctor                        # stale daemon build
toron mail whois flywheel captain                              # the captain fold that routes
git log --all --grep=<id> --regexp-ignore-case --oneline         # the citation case split
```

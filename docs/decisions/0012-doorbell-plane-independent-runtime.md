<!-- governed-by: ADR-0005, ADR-0010, ADR-0011, toron ADR-0020, toron ADR-0027, toron ADR-0028 -->
---
status: accepted
revision: 3 (2026-09-10 — accepted; Phase 0 landed via bd-asr2)
date: 2026-09-09 (rev 1), 2026-09-10 (rev 2)
decision-makers: Captain (bangedorrunt)
consulted: |
  BrightOtter stack study 2026-09-09 (doorbell deep-dives, verified against stack HEAD of that day);
  AmberBeacon stack study 2026-09-10 — dispatcher/journal/sweep/fence verification against flywheel
  HEAD 6f82af2 (bd-7qg6 cursor-hold, bd-8hv0 delta re-subscribe included), toron daemon/watch/
  receipts/presence/inbox surface, beads storage + cron/serve always-on audit, kameo actor-pattern
  study (github.com/tqwewe/kameo — concepts only, no dependency).
informed: flywheel × toron × beads maintainers; harness agents (grok/pi/omp/opencode/codex)
---

# ADR-0012: Spin doorbell out as an independent plane (always-on wake runtime)

| Field | Value |
|-------|-------|
| **Status** | **Accepted** (rev 3, 2026-09-10; Phase 0 landed via bd-asr2) |
| **Decision-makers** | Captain (bangedorrunt) |
| **Extends** | [ADR-0005](0005-always-on-collab-baseline-and-command-surface.md) (doorbell Layer 0, peer-aware, 5/30/3 budget), [ADR-0010](0010-delivery-semantics-attention-triage-decision-lifecycle.md) (delivery semantics, attention triage, `why`-fold), [ADR-0011](0011-session-conductor-crew-budget-captain-fcfs-join-loud.md) (arrive/briefing/join-loud, doorbell-on-arrive), [toron ADR-0020](../../toron/docs/decisions/0020-nostr-two-plane-cli-and-crew-steering.md) (planes P/S/J/N/W, CLI-first), [toron ADR-0027](../../toron/docs/decisions/0027-one-workspace-tick-control-plane.md) (one tick, Tick Law, one-RAM-owner — the flywheel-side implementation), [toron ADR-0028](../../toron/docs/decisions/0028-jido-behavior-first-on-wheelhouse-nouns.md) (control-plane vocabulary: sensors are Msg producers; no second bus) |
| **Does not amend** | frozen MCP tool surface; toron kind registry; one-way deps (extends them: doorbell crates become new leaves); Tick Law (slices stay private, one live reducer per owner); fail-closed bead close; SIGNOFF captain-only |
| **Governs** (on acceptance) | new `doorbell-core` crate (pure reducer + policy state); new `doorbell-rt` crate (runtime library — sensors, brain, sinks, store); new `doorbell` daemon binary; `src/doorbell.rs` + `src/doorbell_tick.rs` + the `cmd_internal_doorbell` effect loop migrate; `flywheel doorbell` CLI as thin proxy; UDS + port **18980**; launchd/systemd unit `com.doorbell.serve`; `state_dir/doorbell/` + `state_dir/doorbell-wake-*.json` ownership |
| **Tracker** | beads (`bd-asr2` — acceptance + Phase 0). Never pull requests. |
| **Verification** | `docs/decisions/_scripts/verify-0012.sh` (lands with Phase 0; per-phase sections specified in §8) |

**Rev 2 changelog** (what changed and why — captain's gaps, each fixed by a numbered decision):

1. **Three design pillars now structure the whole ADR** (§4): extensible, durable, trustworthy. Rev 1 asserted these as adjectives; rev 2 binds each to enforceable invariants and tests.
2. **One implementation, two hosts** (D1): rev 1 risked maintaining flywheel's embedded dispatcher AND a daemon as two behavior copies — the exact C6 disease reintroduced one level up. Rev 2 extracts a `doorbell-rt` library embedded by flywheel or hosted by the daemon binary (the stack's own `[mail.toron]` url / `mailbox_source::resolve` pattern), so Phase 3 flips a default instead of deleting a code path.
3. **Split-brain fence specified** (D9): nothing in rev 1 stopped embedded dispatcher and daemon from running concurrently and double-prompting. Now: ownership lease + liveness ping + toron `serve_lock` precedent.
4. **Durability spec added** (D4): rev 1 said "at-least-once" without specifying journal semantics. Now specified: versioned envelope, decided-before-delivery write-ahead invariant (already true in code, now stated + tested), rotation instead of silent head-trim, torn-tail stop-and-loud, cursor-hold rule (bd-7qg6), durable budgets (today `PinBudget` is RAM-only — a restart resets the 30-min window and can buy 3 extra prompts per pin), and the crash-resume recipe.
5. **Actor topology with bounded mailboxes** (D3, kameo-derived, no dependency): rev 1 said "Tokio tasks + mpsc" and left the load-bearing decisions out. Today's loop runs on an **unbounded** `std::sync::mpsc` (`cmd_internal_doorbell`, `Bell` enum) — tolerable for one subscriber, a wedge vector for N. Now: single-writer brain, sensor actors, per-sink bounded queues with shed-to-digest, fault-as-Msg degradation tracking, panic capture at edges, supervision with capped restarts.
6. **Trust model added** (D6): rev 1 had operator-only budget raises and nothing else. Now: threat table, per-verb authz matrix, webhook egress allowlist (anti-exfil), key custody rules (daemon never holds human keys), journal perms.
7. **Behavior changes pulled out of the migration** (D5/D10): rev 1's arrival triple and roster digests change observable behavior while Phase 1's acceptance is "journal diff empty" — a contradiction. All behavior extensions are now Phase 4 capability flags, each with its own acceptance; the journal envelope is versioned from Phase 0 so later bumps are mechanical.
8. **Producer gaps made honest** (D7): rev 1's S1/E10 ("hold expiry wakes at 03:00 with no swarm") has no mechanism — `br` hold expiry is lazy (noticed on the next `br` command), cron needs a foreground flywheel process, and the stack has zero inbound HTTP. Rev 2 names the missing sensors (beads poller, cron sink, `emit` API) and re-phases them.
9. **Toron-parity claims corrected** (D2): toron has no `/health` endpoint (health = TCP accept + MCP `health_check`), units ship `Restart=on-failure` not always, pidfile is fallback-only, and `heal_tick`/Resonate supervision is flywheel-new (not parity). Doorbell daemon *adds* a real `/health` because its consumers are non-MCP. Hourly 1 MiB logrotate is parity shape rev 1 omitted.
10. **API versioning, clocks, and stats** (D8): version handshake, errors-as-values, SSE `Last-Event-ID` replay, `afk` alias preserved, a named clock table (ten timers, was implicit), `stats` verb so the SLA is measurable.
11. **Verification hardened** (§8): crash-harness (SIGKILL at each journal stage), torn-tail, clock-jump, split-brain, slow-sink, SSE-replay, proptest determinism — plus rev 1's journal-diff parity gate with the normalize rule pinned down before Phase 1.
12. **Citations de-rotted**: rev 1's file:line cites were stale within a day (bd-7qg6/bd-8hv0 moved them). Rev 2 cites `file::symbol` verified at 6f82af2; line numbers only where load-bearing.

---

## 1. TL;DR

Doorbell is already a runtime wearing a flywheel costume: one pure reducer (`workspace::tick`, folded with the private slice `doorbell_tick::tick`) plus one spawn-owned effect process (`cmd_internal_doorbell`) that watches toron's bus, sweeps plane-P inboxes, and injects exactly one `[doorbell]` line via `herdr agent.prompt`. It is the only always-on piece in the stack, but it inherits flywheel's lifecycle, herdr-only transport, spawn-registry identity, and per-workspace scope — which caps it at pane-wake for spawned crews.

**Decision: extract doorbell as plane D, an independent always-on daemon** — sibling to toron (18080) and flywheel serve (8787), on UDS + port **18980**, unit `com.doorbell.serve`. Three crates: the pure core moves verbatim into dependency-free `doorbell-core`; a new `doorbell-rt` runtime library holds the sensor/brain/sink actors and the store (ONE implementation, hosted either embedded in flywheel or by the `doorbell` daemon binary — never both, lease-fenced); flywheel becomes its first subscriber with zero behavior change (Phase 1), general subscriptions arrive (Phase 2), spawn-ownership of the dispatcher is cut (Phase 3), and all behavior extensions land behind capability flags (Phase 4).

**Trigger (2026-09-09):** captain intent to serve cron, human, and CI wake with zero live swarm; every named consumer blocked by C1–C7.

---

## 2. Context — the stack and the doorbell as they are at HEAD 6f82af2

Self-contained primer. A reader with no prior context should be able to implement from this section plus the cited files.

### 2.1 Ownership: who owns what

| System | Owns | Key paths |
|--------|------|-----------|
| **toron** | wire + store: mailbox/bus/Nostr archive (`InProcessBus` over sqlite `bus_events`, 100k cap / 90d prune), kinds registry, receipts, reservations, presence broker, daemon 18080 | `toron/crates/toron-core/src/{bus,mailbox,nostr}/` |
| **br (beads)** | ledger predicate: ready set, gate store, hold rows, dep graph, JSONL sync; never runs git; sqlite WAL | `beads/src/{model,hold}.rs`, `beads/src/storage/{schema,sqlite}.rs` |
| **flywheel** | dispatch: `workspace::tick` reducer, doorbell dispatcher, `loop-mode --work`, wave-swarm token, spawn/send, ledger check, artifacts, serve 8787 | `flywheel/src/workspace.rs`, `src/doorbell.rs`, `src/doorbell_tick.rs`, `src/durable.rs`, `src/main.rs` |
| **doorbell (today)** | wake: ring classification, LWW queues, inject gate, digest, `why`-fold — lives inside flywheel | `src/doorbell.rs` (~2900 lines), `src/doorbell_tick.rs`, `cmd_internal_doorbell` in `src/main.rs` |

Dep rule (load-bearing): **flywheel → toron-workflow → toron-core, never reverse.** Mail embed fork: absent `[mail.toron] url` → in-process `MailSurface`; set → standalone `toron serve --mcp-http` (`Config::toron_mail_config`, `src/config.rs`; toron's client-side resolution order lives in `toron-cli/src/mailbox_source.rs`).

### 2.2 Toron wire in one paragraph

Five planes on a frozen kind registry (`toron-core/src/nostr/kinds.rs`): **P** private (14 rumor / 1059 gift-wrap), **S** shared (9 post, 39000 group-meta, 39002 members; 40/41/42 ingest-only), **J** jobs (43001 request → 43002 claim first-wins → 43003 progress → 43004 result, 43005 cancel / 43006 error), **N** now (20001 ephemeral, **never stored** — presence broker keeps a last-heartbeat-wins map with 60s TTL; a store crash wipes the view to empty), **W** live (`subscribe_trusted` = snapshot + delta + EOSE, exactly-once in-process via snapshot-id sets; cross-process sharing is only the `bus_events` sqlite store via the next snapshot poll; channel-full = publisher-side drop + counter, recoverable by store replay, cap 1024). Receipts sqlite (`corr` PK; owed→acked→resulted, monotonic, never downgrades; WAL + busy_timeout — safe for read-only external pollers). Roles via 30013 `role=` tags. Reservations 30031 (intent 5min / held default 90d / released / revoked; REQUEST_RELEASE window 15min). Channel posts cap 4096 chars. Bead coupling is three strings: mail `--thread` = bead id, subject `[bd-###]`, reserve `--reason` = bead id.

### 2.3 Doorbell today: reducer, dispatcher, rings, delivery, honesty

**Reducer (pure).** `doorbell_tick::tick(DoorbellModel, DoorbellMsg) -> (DoorbellModel, Vec<DoorbellAction>)` (no I/O, no `&mut`) handles Watch/Status/Died/AckSla. `workspace::tick(&WorkspaceModel, &Msg) -> (WorkspaceModel, Vec<Cmd>)` (pure/total/deterministic, `seq++`, clock only as Msg payload) folds it as a private slice alongside `durable::tick` — the Tick Law (ADR-0027). `WorkspaceModel` partitions: `doorbell`, `swarm`, `assignment` (receipts cache), `roles`, `presence`, `open_human`, `pending_wait` (TTL 3600s / cap 512, Sweep-enforced, evictions emit `Audit wait-timeout` and promote to `open_human`), `rering` (90s grace × 3), `presented`/`wake_acked` (per-pin wake ack watermark — **derived deterministically from the Msg log, so crash-resume refold reproduces it**), `digest_queue`, `wake_counters`. Tests assert same-Msg-log → byte-identical Action sequence.

**Effect loop (`cmd_internal_doorbell`, spawn-owned).** Msg sources feed one **unbounded** `std::sync::mpsc::channel::<Bell>` (`Bell::{Watch,Status,Died}`): the bus watch (in-process `subscribe_trusted` thread, filter kinds `9,14,20001,30013,43001,30031,39002` hashtag-scoped — else `toron watch --live` child fallback, the C6 fork), per-pane herdr `events.subscribe` forwarders (bd-8hv0: re-spawned from the sweep whenever the spawn registry grows), and a 1s `recv_timeout` pass that drives `Msg::Sweep` (cursor-gated inbox fetch), presence projection (`presence.jsonl`, <30s = heard), hear-expiry, and delta re-subscribe. Effects run via `handle_workspace_cmds` (Publish/WaitCorr/SqliteUpsert/Invoke→durable Run gate) between tick and journal. Supervision: pid+gen files, lazy-heal on `status/start/recover` + Resonate `heal_tick` cron (`* * * * *`, checkpointed, fires even with no human touching flywheel as long as any worker process is registered), watch child never exits (5-burst 1→16s backoff, then a 16s retry gate forever), and a **binary-generation gate**: a watcher whose exe changed (mtime/len/8k-sha1) refuses all further wakes and self-retires (`WatcherGen`, `inject_allowed`).

**Rings (five classes, LWW per class+hint).** `classify` (`src/doorbell.rs`): Channel (kind-9 addressed to self, incl. 39002 join/leave-addressed and mail-delivered gift-wrap wake → inbox pull), Job (43001 addressed), RequestRelease, ReservationConflict (30031 self), PlanePUrgent (high/urgent). Presence, ack-only, and unaddressed chatter never ring. `should_inject` = idle|done|blocked only (blocked is await-signal, not do-not-disturb); working panes queue (LWW per `(class, hint)`) and drain on the working→injectable transition.

**Delivery (one line, never the body) + budgets.** Inject is `[doorbell] … Pull: …` via `herdr agent.prompt`. Budget **5/30/3** (`SpawnOpts::default`): 5 = peer-poll cadence in **minutes** during spawn settle (`peer_poll_interval_minutes`), 30 = rolling budget window minutes, 3 = max prompts per pin per window (`PinBudget`, `take_allowed`). `take_allowed` runs in the **effect layer** (not the reducer, not journaled as state, RAM-only — see D4.5). Peer handler must show heard within **120s** (`HearTracker`, persisted via `.unanswered` sidecar) or the pin is digested as deaf; tracked corrs re-ring at 90s × max 3. Urgent (`is_urgent_line`: plane-P urgent / request-release / 30031 / ack-sla-expired / unacked-for) skips the queue; routine addressed kind-9 chatter collapses to counters (zero prompts); digest batches at len≥10 or ≥300s (`DIGEST_INTERVAL_DEFAULT_SECS`/`DIGEST_THRESHOLD_DEFAULT`, env-overridable), earliest-deadline-first, 2×-interval wedge alarm.

**Persistence today (the durability baseline D4 builds on).**

| Artifact | Path (flywheel `state_dir`) | Written when | Crash effect |
|---|---|---|---|
| wake state (`since_by_pin`, `plane_s_cursor_us`, `last_wake` ratchet) | `doorbell-wake-{ws}.json` | after every processed wake; plane-S cursor persisted **early** (before classification) so a crash right after doesn't re-ring | survives; missing/corrupt → DEFAULT + 24h `REDELIVERY_LOOKBACK` re-delivery floor |
| journal (decision log, `stage: decided/executed/sweep/drain/…`) | `journals/doorbell-journal-{ws}.jsonl` | after tick per Msg (decided **before** delivery), executed rows after | 5k-line head-trim by rewrite at append; corrupt lines silently skipped on read ("a journal is a trust surface, not a WAL") |
| audit trail (budget_drop, watch_restart, inbox_gap_skip, ack_sla_fired…) | `doorbell/{ws}.audit.jsonl` | per event | append-only |
| hear/deaf | `doorbell/{ws}.unanswered` | per heard/expired | survives |
| dispatcher identity | `doorbell/{ws}.pid`, `{ws}.gen`, `{ws}.project` | start/heal | lazy-heal + `heal_tick` |
| budgets, LWW queues, rering ladder, digest queue | **RAM only** | — | **lost on restart** (budget window resets → up to 3 extra prompts per pin; queues/ringer re-derive from journal refold) |

**Honesty invariants (must survive the move).** `last_wake` only from real observations (same-second ratchet `note_wake` refuses self-mint; GS-039: never invent-green from planned injects). Inbox high-water never advances on unparseable rows — cursor held, batch re-sweeps, `inbox_gap_skip` audited (bd-7qg6: at-least-once, never at-most-once). Presence trust: 20001 forge (author npub≠pin npub) → `LoudMiss`; native herdr idle never trusted; Unknown never prompts; presence truth is `Model.presence` (the dispatcher is the trust root for the internal `Msg::Presence` edge), never a second oracle. Corr authority: sender mints, `corr`≠`thread_id`, vocab-checked. `why`-fold replays the journal asserting `Cmd` sequence equality (per-workspace, bd-7qg6).

---

## 3. Constraints — seven limits of doorbell-inside-flywheel

Unchanged from rev 1 (re-verified at 6f82af2); ordered by how hard each caps future use.

### C1 — Lifecycle is spawn-owned, so wake dies with the swarm

The dispatcher starts from spawn (`pane.rs` spawn path → `doorbell::ensure_running`), stops with `kill`, and heals via flywheel verbs + `heal_tick`. No swarm = no doorbell, although toron + beads + herdr + the serve daemon are all alive. **Forbids:** cron-driven wake, overnight holds, human digests with no live fleet.

### C2 — Identity is the spawn registry, so only spawned pins are wakenable

`mail_identity` is written at spawn (`pane.rs`, `pinned_mail_identity`); `SelfIndex` resolves pins-or-npubs from that registry; `re-inject` iterates registry pins. Humans, cron jobs, external services, cross-project pins have no wake address. **Forbids:** human wake subscription, service identities, cross-project mentions.

### C3 — Transport is herdr-prompt-only, so wake = one harness

Delivery ends at `herdr agent.prompt` while the reducer already emits abstract `Prompt{pin,line}`. **Forbids:** CI callbacks, phone digests, headless agents, webhook/mobile consumers.

### C4 — Scope is one workspace, so no cross-project or cross-host wake

Journal, pid/gen, `since_by_pin`, `last_wake` are per-`{workspace}`. **Forbids:** cross-repo unblock, multi-host fleets, coalition-wide attention.

### C5 — Tick shares a process with swarm/loop, so attention competes with dispatch

One `WorkspaceModel` owns perception + assignment + swarm + human partitions; wake tuning ships as a flywheel rebuild + reinstall (the `WatcherGen` binary gate at least refuses stale-watcher wakes meanwhile). **Forbids:** independent deploy cadence; wake SLAs under swarm load.

### C6 — Inbox path is CLI-or-embed forked, so latency and durability split

Fast path `Mail::inbox_filtered`; fallback shells `toron mail inbox`. Toron's `--since last` cursor (`inbox_cursors`) vs flywheel's `since_by_pin` joined by gap-skip logic — two code paths, two failure modes. Toron's own study verdict: do NOT copy this; the daemon must own ONE cursor keyed `(subscriber, scope)`. **Forbids:** a single crash-resume story.

### C7 — No public API surface, so nothing else can subscribe

`doorbell {status,start,stop,re-inject,afk,why}` are flywheel verbs; `Digest`/`LoudMiss`/`Audit` render to stderr/journal. Against toron's 40 tools and flywheel's 63 commands, doorbell has zero addressable surface. **Forbids:** every consumer in §10 except today's spawned panes.

---

## 4. Design pillars (rev 2)

Every decision below binds to one of three pillars. A future edit that weakens a pillar invariant must amend this ADR.

**P1 — Extensible by addition, never by edit.** New transport (webhook, DM, push), new identity class (role, npub, service key), new producer (beads poller, cron, CI), new scope (cross-workspace) each land as a **new sensor or sink adapter plus a config flag**. The reducer, the ring vocabulary, the journal envelope, and the budget semantics never change for a new edge. Corollary: `Prompt{pin,line}` stays the only delivery intent; sinks translate it.

**P2 — Durable by construction, at-least-once, visibly.** Every state that gates delivery is journaled or persisted before it gates (write-ahead); every redelivery is absorbed by a named dedup lattice (LWW queues, ack watermarks, same-second ratchet, budget window, 24h lookback) and *shown* in the journal; recovery is deterministic refold + side tables, never "best effort". A crash may re-ring; it may never silently drop, silently skip a cursor, or reset a budget.

**P3 — Trustworthy by default, degraded loudly.** Untrusted inputs (bus bytes) are classified, never executed; authority verbs are operator-gated; egress is allowlisted; the daemon's own health is a first-class subscriber of itself — every sensor/sink degradation is a `Msg` (fault-as-data), journaled, visible in `/health`, `stats`, and the digest, so "why didn't I get woken?" always has a journal-backed answer.

---

## 5. Decision set

### D1 — Crate split: pure core, runtime library, daemon binary (one implementation, two hosts)

- **`doorbell-core`** (leaf, zero runtime deps — serde/serde_json/anyhow/sha1/hex/chrono as today): `doorbell_tick` verbatim + the pure helpers from `doorbell.rs` (`classify`, `parse_watch_ndjson`, `parse_inbox_item`, `doorbell_line[_with_event]`, `should_inject`, `is_drain_transition`, `DoorbellQueue`, `PinBudget`, `take_allowed`, `SelfIndex`, `HearTracker`, high-water/cursor helpers, `is_urgent_line`/`is_routine_wake_line`/`classify_cmd`). **No** tokio, resonate, herdr, toron, or `crate::(config|workspace|durable|main|mail_shim|herdr_shim|backend)` — wire shapes are parsed from `serde_json::Value` exactly as today, so no toron-core type dependency and no wire-type duplication.
- **`doorbell-rt`** (runtime library): the effect loop ported from `cmd_internal_doorbell` — sensor tasks, the brain (single-writer owner of the model + journal), sink actors, the store (wake state, hear, subscriptions, cursors), the API service (UDS/HTTP+SSE), and the supervision tree. Depends on tokio, toron-core (bus/mailbox/receipts as library), the herdr client shim, and `doorbell-core`.
- **`doorbell`** (daemon binary): `main()` + daemon lifecycle (`start/stop/restart/status`, unit install) around `doorbell-rt`. Owns `state_dir/doorbell/` + journals.
- **One implementation, two hosts.** Flywheel embeds `doorbell-rt` exactly as it embeds toron today (absent `[doorbell] url` → embedded in-process host; set → the daemon is the host and flywheel is a pure API client). This is the stack's proven `mailbox_source::resolve` pattern (TORON_MCP_URL → data-dir pin → 200ms probe → config → embedded) copied for wake. The embedded host is *not* deleted at Phase 3 — it stays as degraded mode — but the D9 fence guarantees only one host ever runs the loop per workspace. This kills the rev-1 double-maintenance risk (two behavior copies of the dispatcher) which would have reintroduced C6 at the dispatcher level.
- Dep graph: `flywheel → doorbell-rt → doorbell-core`; `doorbell-rt → toron-core` (leaf); never reverse. `// governed-by: ADR-0012` heads every moved file; Tick Law preserved (one live reducer per owner; slices stay private).

### D2 — Runtime shape (toron-parity, corrected; differences deliberate)

- **Addressing:** Unix-domain socket primary (`$XDG_RUNTIME_DIR/doorbell.sock` or `state_dir/doorbell.sock`, dir perms 0700 — file perms are the same-host auth) + optional TCP bind, default `127.0.0.1:18980` (toron 18080, flywheel serve 8787; remote hosts = explicit config, never default).
- **Unit** `com.doorbell.serve`: launchd plist / systemd user unit / pidfile fallback — the three mechanisms copied from `toron-cli/src/daemon.rs`. **Corrected parity:** units ship `Restart=on-failure` with `SuccessfulExit=false` semantics (a clean exit over a port conflict must STAY dead — toron's bd-serve-double-spawn lesson; rev 1's "always" text was wrong), `RunAtLoad`, `ThrottleInterval 5`, secrets via 0600 files never inline, logs under the state dir, and **hourly 1 MiB copy-truncate logrotate from day one** (toron parity rev 1 omitted; the 1s-watch RSS bomb bd-741i is the paid lesson).
- **Lifecycle** `{start,stop,restart,status}` with verify-before-exit-0: `stop` asserts port+socket dead within 5s grace; `restart` re-verifies after the swap; `stop` refuses unmanaged socket holders without `--force`.
- **`/health` (JSON, added — toron has none; doorbell's consumers are non-MCP):** `{protocol, core_v, journal_v, uptime_s, watch:{state, last_event_age_s}, sweep:{last_age_s}, subscribers:n, degraded:[…], journal:{seq, lag_lines}, owner:{pid, gen, host}}`. Read-only, unauthenticated on the UDS/TCP loopback bind.
- **State inventory migrates byte-identical** (rev 1 said "state_dir/doorbell/" and missed files): `{ws}.pid`, `{ws}.gen`, `{ws}.project`, `{ws}.unanswered`, `{ws}.audit.jsonl` (dir `state_dir/doorbell/`), **plus** `doorbell-wake-{ws}.json` (at the state-dir root today) and `journals/doorbell-journal-{ws}.jsonl`. Journal line format unchanged (envelope gains an additive `v` field, D4.1) so existing `why` proofs replay.
- **Supervision:** lazy-heal + Resonate `heal_tick` retained but rekeyed from workspaces to **subscriptions** (this part is flywheel-new, not toron-parity — say so); watch child never exits (16s retry gate retained); `WatcherGen` binary gate retained as the version-skew fence (stale binary refuses wakes and retires; the unit restarts into the new binary).
- **Config:** `~/.config/doorbell/config.toml` (operator layer, 0600): toron URL, herdr socket, bind addresses, budget defaults, sink allowlist, service identities. Identity resolution ladder unchanged in spirit: toron `whois`/30013 + flywheel spawn-registry as *one resolver among many* (ends C2).

### D3 — Actor topology (kameo concepts, zero new dependencies)

Tokio tasks + `mpsc`/`oneshot` channels only. The pattern vocabulary is borrowed from kameo (see Appendix A); no actor framework, no new crate.

```
                       ┌────────────────────────────────────────────┐
 sensors (tell)        │  BRAIN — single writer                    │   ask (oneshot + timeout)
 BusWatch ──Msg──▶     │  owns WorkspaceModel + journal + store    │ ◀── CLI / API / flywheel proxy
 InboxSweep ──Msg──▶   │  loop: recv → tick → journal(decided)     │
 HerdrFwd ×N ──Msg──▶  │        → dispatch effects → journal(out)  │
 AckSlaTimer ──Msg──▶  │  never awaits I/O while holding the model │
 PresencePoll ──Msg──▶ └──────────────┬─────────────────────────────┘
 BeadsPoll (P4)                       │ Prompt intents (bounded mpsc per sink)
 CronSink (P4)                        ▼
                       SinkManager ──▶ herdr-prompt │ webhook │ ncrypt-DM │ SSE
                          (each: bounded queue, shed-to-digest, restart w/ backoff)
```

- **Single-writer brain.** Exactly one task owns the model, the journal, and the store. All sensors `tell` Msgs into ONE bounded channel (capacity 1024; `send().await` = natural backpressure — a slow brain parks sensors, never drops silently; toron's publisher-side-drop + counter remains the wire-side escape). This is ADR-0027's one-RAM-owner rule lifted to process scope: **no shared mutable state anywhere; state changes only by message.**
- **Sensors are replaceable edges.** Each sensor is a small task with its own lifecycle: spawn, health, retry/backoff, death → `Msg::Degraded{source, reason}` into the brain (fault-as-data — kameo's link-death notification as a Msg). The pipe pattern (fetch off-loop, deliver back as a Msg) is exactly today's `FetchInbox` Cmd → sweep → `InboxRows` Msg; sensors never block the brain on I/O.
- **Sinks are bounded, shed, supervised.** Per-subscription sink actor, bounded queue (default 64, kameo's default; configurable). Full queue or slow endpoint → **shed-to-digest**: the intent is dropped from the prompt path, recorded as a journal effect row, and enters the digest batch — attention contracts favor batching over blocking; the brain's latency never depends on a sink. Sink death → supervisor restarts (Transient policy: failures only; cap 5 restarts / 5s → park + `Degraded` + audit; the daemon itself keeps serving). Graceful stop drains sink queues and journals undelivered intents (kameo's `on_undelivered`) so resume redelivers; kill aborts in-flight but the journal already holds the intent.
- **ask vs tell.** CLI/API verbs (`digest`, `why`, `budget`, `stats`, `subscribe`) are `ask`: request + reply channel + timeout, errors as typed values (`busy | wedged | absent | refused`, kameo's `SendError` taxonomy adapted) — the CLI can render "daemon wedged (last sweep 4m ago)" instead of hanging. Sensors are `tell`: fire-and-forget, failures land in the brain as `Degraded`.
- **Panic isolation.** The brain is pure (panics impossible by construction — same guarantee as `tick` today). Adapter panics are caught at the task boundary (`std::panic::catch_unwind` / `JoinError::is_panic`) and converted to `Msg::Degraded{source, panic}` — journaled, visible, never fatal to the runtime.
- **What this fixes:** today's unbounded `std::sync::mpsc::<Bell>` (fine for one subscriber) becomes a wedge vector the moment N subscribers × slow sinks exist. Bounded-queue + shed-to-digest is the difference between "one stalled webhook delays every crew's wake" and "one stalled webhook degrades to its own digest, everything else unaffected".

### D4 — Durability specification (the section rev 1 lacked)

**D4.1 Journal envelope, versioned from Phase 0.** Every line: `{"v":1,"ws":…,"seq":N,"ts":…,"stage":"decided|executed|sweep|drain|role_bound|fact|…","msg":{…},"cmds":[…],"pins":[…],"effect":{…}}`. Missing `v` reads as 1 (existing lines replay unchanged). Readers tolerate unknown fields (forward compat); writers bump `v` only for semantic changes (v2 in Phase 2 adds `subscriber` scoping + budget rows — mechanical, pre-announced). `why` refuses to fold a journal whose `v` it does not understand, loudly.

**D4.2 Write-ahead invariant (state the existing behavior, then test it).** The journal row for a decision is appended and flushed BEFORE the effect runs (today: `stage:"decided"` at the watch arm precedes `doorbell_prompt_and_hear`; the sweep journals per-pin lines with the batch-gate outcome before prompting). Invariant: **no Prompt delivery without a prior journaled intent**. `verify-0012.sh` asserts the invariant by construction (the brain code path is journal→effect, structurally) and by crash-harness (§8). `fsync` policy: line-flush per batch; `sync_data()` before Prompt-intent delivery (at most a few/sec — cheap) and before cursor/wake-state writes.

**D4.3 Rotation, not silent head-trim.** Today's 5k-line cap rewrites the file in place, silently deleting the oldest decisions — a `why` query after a busy night folds less history with no trace. Replace with rotation: `doorbell-journal-{ws}.jsonl` (active) + `.1` … `.N` (retention: total 5k lines or 5 MiB across generations, whichever first — same bound, no silent loss within retention; a rotation emits an audit row `journal_rotated{dropped_lines}`). `why` folds active + retained generations; if a query needs older history it says so (`why: seq X predates retention, first foldable seq is Y`) instead of lying by omission.

**D4.4 Torn tail = stop-and-loud, never skip-and-continue.** On load, if the final line is unparseable or `seq` is non-contiguous, the fold stops at the gap: `why` reports `fold stopped at seq N (torn tail after crash)`, `/health` shows `degraded: journal_torn_tail`, and a `LoudMiss` audit row is written. The journal file is never truncated or repaired in place (append-only trust surface — unchanged from today's philosophy, but the skip becomes loud instead of silent).

**D4.5 Budgets become durable model state.** Today `PinBudget` lives in an effect-layer `BTreeMap` (RAM-only): a daemon restart resets the 30-min window and can buy every pin 3 more prompts — a P2 honesty hole (the budget is a promise; a crash must not break it). Move budget state into the reducer model (per-pin, applied inside `tick` from `now`-bearing Msgs; `take_allowed` becomes a pure model transition), so journal refold reproduces the window exactly. Lands in **Phase 2 with journal v2** (it adds journal rows; Phase 0–1 keep the RAM gate for byte parity, and the Phase-1 crash test explicitly asserts the known limitation pre-v2, post-v2 asserts the fix).

**D4.6 Crash-resume recipe (one story, not two).** Resume = (1) load wake state (high-waters, last-wake ratchet; missing/corrupt → DEFAULT + 24h `REDELIVERY_LOOKBACK` re-delivery — the existing floor); (2) load `.unanswered` hear state; (3) rebuild `assignment` from receipts sqlite + roles from binds (existing seeds); (4) refold the journal tail to rebuild `presented`/`wake_acked`/queues/rering/digest/budget(v2); (5) reconcile plane-S from the toron store (snapshot poll covers the outage window, 100k/90d bounded) and plane-P from `--since` cursors; (6) redeliver journaled-but-unconfirmed Prompt intents **within budget**. The dedup lattice that makes redelivery safe: LWW per (class,hint) + ack watermarks + same-second ratchet + budget window + 24h lookback. Everything in this table already exists except (4)-budget and the rotation/torn-tail guards — the move is mostly *stating and testing* what the code already does.

**D4.7 Gap reconciliation is journaled proof.** On watch reconnect, the sensor reconciles from the store and the brain journals `stage:"reconcile", source:"archive", from_seq, to_seq`. `why` can then *prove* no event was missed during an outage ("watch reconnecting 14:02:10–14:02:26; reconciled 37 events from store at seq 4812"). This turns "we think nothing was lost" into a checkable claim — the core of trustworthy-by-design.

### D5 — Identity, subscriptions, and delivery generalization (ends C2–C4; behavior extensions deferred to Phase 4)

- **Subscription = identity × scope × filter × sink × budget.** Identity classes: toron pin, `role:<name>` (30013), operator npub, service key (D7). Scope: one workspace, N workspaces (journal key `(workspace, corr)` at v2). Filters: ring class / kind. Sinks: herdr prompt (today), webhook POST, NIP-17 DM; push/mobile later as adapters (P1: new sink = new adapter + flag, reducer untouched). Budgets: per-subscription overrides, defaults 5/30/3, raises operator-only.
- **Perception/policy/delivery separation (the layering rev 1 left implicit):** one shared classification pass per workspace (rings are facts, classified once, journaled once); N per-subscriber policy evaluations (budget/queue/digest state machines, pure, in `doorbell-core`); delivery adapters in `doorbell-rt`. `why <subscriber> --corr` filters the single totally-ordered journal by subscriber tag — no per-subscriber logs (interleaving ambiguity), no N-times reclassification (cost + drift).
- **Arrival triple (discovery without introduction) moves to Phase 4** behind `arrival_triple = false` default: presence publish + role auto-bind (closing the GS-030 no-bind-invisible gap) + channel join ring + roster headers in digests. It changes spawn-time and ring behavior — incompatible with Phase 1's byte-parity gate, so it is a flagged extension with its own acceptance (E2-style), not part of the migration. Rosters stay scope-bound (a subscription sees only its own workspaces).

### D6 — Trust model (threat table + authz matrix)

**Threat table.**

| Trust boundary | Threat | Control |
|---|---|---|
| toron bus bytes (untrusted) | forged rings, presence forge, addressed-to-self spoofing | unchanged rules: `classify` sanitizes; forge → `LoudMiss`, no write; Unknown never prompts; addressed requires npub/index match |
| UDS clients (local, file-perm-trusted) | unauthorized subscribe/emit/inject | per-verb matrix below; socket dir 0700 |
| config file (operator) | budget inflation, egress redirect | 0600, operator-only; budget raises above default refuse non-operator |
| webhook egress | attention-metadata exfiltration | operator-approved host allowlist; default payload = hint + counts, full line opt-in per subscription |
| DM sink keys | key theft/custody | daemon stores ncryptsec **only for service identities it mints**; human/operator keys never held by the daemon |
| journal/audit/state | local metadata leak (bead titles, presence) | 0600, never leaves host except via operator-approved sinks |
| SSE/events stream | mutation via stream | stream is strictly read-only fan-out; no verbs ride it |
| emit producers (D7) | spam rings via stolen token | tokens operator-issued, scoped (identity × channel × rate), rate-limited, revocable |

**Verb authz matrix.**

| Verb | Local process | Operator |
|---|---|---|
| `status` / `health` / `why` / `digest` (read) / `events` / `stats` | ✔ | ✔ |
| `wake subscribe/unsubscribe` (herdr sink, own-workspace scope) | ✔ (spawn path needs it) | ✔ |
| `wake subscribe` with webhook/DM sink, or cross-workspace scope | ✖ refused | ✔ |
| `budget set` ≤ default | ✔ (own subscription) | ✔ |
| `budget set` > default, `inject`, service-identity mint, token issue | ✖ | ✔ (+ rate-limit; `inject` keeps the same-second ratchet) |

Presence-trust, corr-authority, and forge-rejection rules ride along verbatim (§2.3) — they are pillar-P3 invariants, not tuning knobs.

### D7 — Producers: what exists, what must be built (the honesty fix for S1/E10)

Rev 1's scenarios assumed producers that do not exist. Audited at HEAD: **`br` hold expiry is lazy** (evaluated on the next `br` command — `beads/src/hold.rs`, `storage/sqlite.rs`); **cron ticks require a foreground flywheel process** (Resonate schedules queue server-side but need a registered worker — `flywheel/src/cron.rs`); **the stack has zero inbound HTTP** (serve 8787 is herdr snapshot/send only). The daemon's sensor catalog, phased honestly:

| Producer | Today | Phase | Mechanism |
|---|---|---|---|
| toron bus events (planes P/S) | ✔ watch | 0–1 | unchanged |
| herdr pane status | ✔ forwarders | 0–1 | unchanged |
| presence | ✔ projection + broker | 0–1 | unchanged |
| br ledger changes (hold expiry, wave-gate, closes) | ✖ lazy | **4** | **beads-poller sensor**: read-only WAL poll of br sqlite (`captain_holds.expires_at`, `issues.updated_at`; WAL + busy_timeout makes concurrent read-only safe), 30s cadence, change-detect by `updated_at`/rowid, emits `Fact` Msgs; br is never written |
| cron ticks | flywheel-hosted only | **4** | cron stays in flywheel (no scheduler duplication — D10); `serve` is already launchd-supervised (`src/daemon.rs`), so an always-on flywheel exists without a swarm; cron jobs gain an optional `doorbell` sink (a `doorbell emit` call) for zero-swarm human wake |
| external CI/services | ✖ no inbound | **4** | **`doorbell emit`**: authenticated HTTP/UDS endpoint (operator-issued token bound to a config-declared service identity) that publishes a kind-9 addressed post *through toron* as that identity — an edge producer, not a second bus; rings flow the existing wire |

Phase 2's second consumer is therefore **human digest subscription** (delivery-side only, no new sensors — honest scope); beads/cron/emit sensors are Phase 4 flags.

### D8 — API surface, versioning, clocks, stats

| Verb | Semantics |
|------|-----------|
| `wake subscribe/unsubscribe/list` | identity + filter + scope + sink + budget (authz per D6) |
| `digest [--since] [--format]` | generalizes `afk` (alias preserved): per-identity, earliest-deadline-first, overdue escalated |
| `why <id> [--corr --seq]` | journal fold, now subscriber-filterable; prints msg / state-delta / `Vec<Cmd>` triple, stops loudly at torn tails (D4.4) |
| `budget {show,set}` | per-subscriber 5/30/3; operator-only raises |
| `inject <pin> <line>` | operator-only, journaled + audited, rate-limited, same-second ratchet preserved |
| `stats [--json]` | counters: rings by class, injects, drops, sheds, deaf, digest batches, sweep/journal lag — the SLA becomes measurable (D3 shed counts, D4 reconcile counts) |
| `ping` | version handshake: `{protocol, core_v, journal_v}`; version-skew refuses loudly (a stale CLI against a new daemon names both versions instead of mis-rendering) |
| `events` (SSE) | live wake/digest/audit fan-out with **`Last-Event-ID` replay** bounded by journal retention (a reconnecting consumer resumes; beyond retention it is told the first replayable seq — same honesty rule as `why`) |

`flywheel doorbell …` stays a byte-preserving thin proxy (flags pass through, output identical; `afk` included).

**Clock discipline (named, was implicit).** Ten timers: sweep 1s · presence TTL 60s · ack-SLA check 60s (30-min SLA) · watch retry 16s · hear 120s · re-ring 90s×3 · digest 300s/len 10 · pending_wait TTL 3600/cap 512 · budget 30min×3 · lookback 24h. Rule: wall-clock enters ONLY as Msg payloads (replayable — unchanged); monotonic time drives retry/backoff pacing only. Clock jumps (NTP) therefore never re-order the journal; at worst a deadline fires early/late once, and the digest ordering re-stabilizes on the next tick.

### D9 — Single-writer fence (split-brain is a double-prompt bug class)

Neither rev 1 nor the current code excludes two hosts (embedded dispatcher + daemon) watching one bus → double prompts. Fence, all three layers:

1. **Ownership lease**: `state_dir/doorbell/owner.json` `{pid, boot_id, host, started_unix, heartbeat_ts}` — heartbeat rewritten each sweep; stale = pid dead or heartbeat > 90s.
2. **Bind fence**: the daemon binds its UDS + TCP socket at startup (toron `serve_lock`/`classify_port_peer` precedent): AddrInUse + healthy doorbell peer → clean exit 0 (no respawn war); non-doorbell squatter → hard fail naming it.
3. **Embedded deference**: the embedded host (inside flywheel spawn) pings the daemon (200ms UDS) before starting its loop — alive → flywheel attaches as an API client (registers its workspace subscription, never runs a local loop); dead/stale → embedded takes the lease and runs. `WatcherGen` stays as the fourth fence (binary version).

Phase 3 flips the default host to the daemon; the embedded path survives as degraded mode but the fence guarantees it never runs concurrently. E13 tests this.

### D10 — Non-goals (explicit)

No new MCP tools in waves 1–2 (CLI-first per toron ADR-0020). No bead-ledger logic moves (ready predicate, gates, holds stay in br; D observes). No workflow-engine move (`Invoke` still routes through toron's engine + flywheel's durable Run gate). No presence-write authority (D consumes 20001, never mints it). No second bus (journal stays the audit trail; no 30015/wake kind — timers stay `Msg` producers per ADR-0028; `emit` is an edge that publishes through toron, not a bypass). No scheduler duplication (cron stays flywheel's; D only observes/emits). No kameo/actor-framework dependency (concepts only — Appendix A). No cross-host federation in this ADR (cross-workspace scopes are same-host; revisit when a second host joins — toron relays would be the transport).

---

## 6. Options

| Option | What | Verdict |
|--------|------|---------|
| A. Stay in flywheel | Tune budgets/digests in place | Rejected. Keeps C1–C7; every new consumer re-pays the spawn-lifecycle tax. |
| B. Extract as library only | `doorbell-core`, flywheel hosts the loop | Rejected as end-state (stepping-stone for Phase 0). Fixes C5 partially; C1–C4/C6/C7 untouched. |
| **C (chosen). Independent daemon, plane D** | D1–D9 above | Chosen. Ends C1–C7; `doorbell-rt` one-implementation rule keeps parity provable; fence keeps single-writer. |
| C′. Daemon + retained embedded fallback (rev 2 refinement) | C plus D1's two-host rule | The refinement folded into C: without it, Phase 1 creates two behavior copies (the C6 disease one level up); with it, Phase 3 flips a default. |
| D. Move into toron | Wake as toron subcommands | Rejected. Violates layer ownership (toron carries, flywheel drives; wake policy is neither), bloats toron's frozen surface, couples wake deploys to mailbox deploys. |
| E. Elixir/BEAM daemon | OTP supervision free | Rejected. Rewrite tax on the pure core (byte-identical replay re-proven from scratch), toron-core/herdr reuse becomes IPC, second runtime to operate. Revisit trigger: second host joins. |

Drivers: `separate-before-serializing-shared-state` (cut at `from_doorbell`/`from_loop` before adding subscribers), `foundational-thinking` (reducer first, sinks later), `laziness-protocol` (move verbatim, no rewrite), `prove-it-works` (journal-diff + crash-harness acceptance, not self-report), `boundary-discipline` (operator-only budget raises, egress allowlist, inject audited).

---

## 7. Implementation plan (five phases; 0–1 pure move, 2 delivery, 3 ownership, 4 behavior)

### Phase 0 — extract `doorbell-core` (flywheel repo, no new process)

**Affected:** `src/doorbell.rs` (pure fns move), `src/doorbell_tick.rs` (moves verbatim), new `crates/doorbell-core/`, `Cargo.toml` (member), `docs/decisions/_scripts/verify-0012.sh` (new — §8), `docs/decisions/README.md` row on landing. Journal envelope gains the additive `v:1` field (readers treat missing as 1 — no behavioral change, replay-compatible).
**Pattern:** pure move; no signature changes; imports become `doorbell_core::{…}`.
**Avoid:** behavior tweaks in the move; new sinks; touching `workspace.rs` partitions; journal format changes beyond the additive `v`.
**Tests:** existing `sc_doorbell_adr0005` + `adr0027_s1/s2/s3` slices green unmodified; same-Msg-log → byte-identical Actions through the crate path.

### Phase 1 — `doorbell-rt` + `doorbell` daemon beside flywheel (byte parity)

**Affected:** new `crates/doorbell-rt/` (effect loop ported from `cmd_internal_doorbell` into brain/sensors/sink tasks per D3), new `crates/doorbell-cli/ (`doorbell` daemon, lifecycle per D2), `src/main.rs` (`doorbell` subcommands → proxy; spawn path gains the D9 deference ping), state migration (copy the D2 inventory), unit files, UDS+18980 bind, `/health`.
**Pattern:** flywheel embeds `doorbell-rt` (replaces the in-file loop — one implementation, two hosts); toron `daemon.rs` lifecycle copied, not reinvented; proxy byte-preserving.
**Avoid:** auto-starting the daemon (opt-in via config `[doorbell] url` or `doorbell start`); behavior changes (arrival triple is Phase 4); deleting the embedded host.
**Tests:** one-release journal-diff acceptance (embedded-vs-daemon, normalize = strip `ts` fields only — v1 has no budget rows), proxy output diffed, crash-resume (SIGKILL mid-queue → restart → pending wakes re-fire at-least-once, no budget dup beyond the known pre-v2 RAM limitation, explicitly asserted), fence test (daemon alive → embedded defers).

### Phase 2 — subscriptions + first new consumer (human digest) + durable budgets

**Affected:** `doorbell-rt/src/subscribe.rs` (subscription store), `src/sinks/{webhook,ncrypt_dm}.rs` (first non-herdr sinks, allowlisted per D6), journal v2 (`subscriber` scoping + budget rows per D4.5), `events` SSE with `Last-Event-ID`, `stats`, `emit` authz plumbing (endpoint itself Phase 4).
**Acceptance:** a wake fires for a subscribed human identity with zero live swarm (ends C1 for that consumer); budget survives daemon restart (3 prompts pre-kill → 0 additional post-restart within the window); deaf-service digest names the subscriber; webhook without allowlist entry refused; `why` replays subscriber-filtered.

### Phase 3 — cut spawn-ownership

**Affected:** `src/main.rs` spawn/kill paths (spawn no longer implies starting the loop — it registers the subscription via API; `kill` never touches the daemon), `arrive` (attach-only), default flips to daemon host (`[doorbell] url` default `localhost:18980`), skill docs (`flywheel/SKILL.md` doorbell section), AGENTS.md doorbell bullets.
**Acceptance:** `flywheel kill <ws> --force` → daemon green, digests flowing; fresh boot, no swarm → subscriptions active; embedded degraded mode still works with the daemon stopped (fence inverts cleanly).

### Phase 4 — behavior extensions, each behind a config flag, each with its own acceptance

`arrival_triple` (E1/E2 roster semantics), `beads_poller` (E17), `cron_doorbell_sink`, `emit` endpoint + service identities, cross-workspace scopes (`(workspace, corr)` journal keys already v2-ready), wake analytics → `docs/learnings/` via the ce-compound gate (§10.12). Default-off flags keep "no observable change until opted in" true all the way through the migration — the fix for rev 1's D4-vs-Phase-1 contradiction.

---

## 8. Verification (acceptance)

Phase 0:

- [x] `verify-0012.sh` green (2026-09-13, `RESULT: PASS`): `doorbell-core` builds standalone; no `resonate_sdk|tokio|toron_|herdr` and no `crate::(config|workspace|durable|main|toron_embed|mail_shim|herdr_shim|backend)` inside it
- [x] Same Msg log → byte-identical Action sequence through the crate path
- [x] `sc_doorbell_adr0005` + `adr0027_s1/s2/s3` slices green unmodified
- [x] Journal replay: pre-v1 lines (missing `v`) fold identically to v1-tagged lines

Phase 1:

- [x] Embedded-vs-daemon journal diff empty modulo timestamps (normalize = strip `ts`; procedure in `verify-0012.sh`) for a full wave-swarm fixture run
- [x] `flywheel doorbell status/start/why/afk` output byte-identical via proxy
- [x] **Crash-harness**: SIGKILL at each journal stage (after `decided`, after effect, mid-sweep, mid-rotation) × fixture — no lost wake beyond the 24h-lookback floor, no budget breach, no unjournaled delivery; resume logs the redelivery rows
- [x] **Fence**: daemon alive + `flywheel spawn` → embedded defers (one prompt total); daemon dead + stale lease → embedded takes over; both-bind conflict → clean exit 0
- [x] **Host CLI resolution + degraded honesty** (2026-09-13, live). The daemon execs `toron` and `herdr`; under launchd `PATH=/usr/bin:/bin:/usr/sbin:/sbin` holds neither, so a bare-name spawn failed as an ordinary child death — the watcher respawned on the retry gate every 16s and every inbox fetch audited `toron spawn/timeout failed`, while `/health` reported `mode: hosted, degraded: []`. The daemon now resolves both to an absolute path (`PATH` → `~/.cargo/bin` → `~/.local/bin` → beside its own exe → mise shims **last**, because a shim needs env/cwd context and dies with `No version is set for shim`), logs the pair at startup, and exposes them as `/health.binaries`. `/health.degraded` now names `cli_unresolved`, `inbox_fetch_failing` (a fresh `inbox_error` audit row), and `watch_restart_loop` (restart frequency ≫ the designed ~109s idle-exit cadence, so a quiet watcher is never mistaken for a sick one). Proven live: two `toron watch` children alive where there were zero, `Watch` rows landing in the journal again, `degraded: []` true.

Phase 2:

- [ ] Subscribed human receives a wake with zero live flywheel swarm; `why` replays it subscriber-filtered — **re-opened 2026-09-13 (live verification), index half now landed (bd-cak3)**: the hosted loop's sweep pin set and `SelfIndex` were built from the **spawn registry** alone (`file_host::refresh_index` → `registry_rows`), so a subscribed identity that was never spawned in the project (the operator pin) was never fetched and never `addressed` — mail delivered, no ring, zero journal rows. `refresh_index` now unions subscribed pins for the swept workspace (`subscription_pins`), live-proven: `since_by_pin` gained the operator pin, its inbox was fetched, and an urgent wake was classified plane-P and journaled as `Audit queued` against it. **Still open:** those pins get no `panes`/`targets` entry (a subscription row carries no pane), so `should_inject` refuses on an `unknown` status and the wake queues rather than delivering. Closing this item needs the delivery half — a real pane to route to, or the non-herdr sinks (webhook/DM) that Phase 2 specifies — tracked as bd-ba8t.
- [x] **Budget durability**: 3 prompts, SIGKILL, restart → 0 further prompts in the window (v2); pre-v2 harness documents the RAM-reset as known-and-fixed-in-v2
- [x] **Slow sink**: webhook stalled 30s → shed-to-digest + `Degraded` row; herdr-prompt latency unaffected (brain SLA holds)
- [x] **Authz negatives**: non-operator webhook subscribe refused; non-allowlisted host refused; `emit` without token refused (Phase 4 endpoint, rule lands here)
- [x] **SSE replay**: disconnect/reconnect with `Last-Event-ID` → gap-free stream or explicit first-replayable-seq notice
- [x] 120s-heard + 5/30/3 enforced on the new sink; deaf-service digest names subscriber+corr+attempts

Phase 3:

- [x] `flywheel kill --force` leaves daemon green; cold boot, no swarm, serves `digest` + `why` + `events`
- [x] Embedded degraded mode (daemon stopped) passes the Phase-1 journal-diff gate
- [x] Skill + AGENTS.md doorbell sections describe plane D (no spawn-owned speech remains; toron skill role-bind section synced 2026-09-13)

Phase 4 (per flag, on opt-in):

- [x] `arrival_triple`: E2 semantics (P's digest carries Q's roster row; role-addressed first contact) with flag on; flag off = Phase-3 behavior byte-identical
- [x] `beads_poller`: E17 — hold expiry wake at 03:00, zero swarm, `ps` proves daemon-only; poller is read-only (br sqlite mtime/hash unchanged)
- [x] `emit`: tokened service identity rings via kind-9; revoked token refused; rate limit holds
- [ ] **Clock-jump**: NTP step mid-run → journal order unchanged, deadlines re-stabilize within one digest interval
- [x] **Torn tail**: truncate journal mid-line → `why` stops at gap loudly, `/health` degraded, no fold-past

Standing (all phases): `mbx +nightly-aarch64-apple-darwin test` green; proptest determinism (same Msg log → same Cmds, randomized sequences) in CI from Phase 0.

---

## 9. Consequences

**Positive**

- Wake outlives swarms (C1/C2 end): subscriptions, then beads/cron/CI producers, then human digests — each a flag away after Phase 2.
- One attention discipline everywhere: budgets, triage, digests, presence-trust uniform across harnesses, humans, services.
- One implementation, two hosts: parity stays provable forever (journal-diff gate reusable after any change); no C6 rebirth.
- Causal replay as a product: `why` for any wake, subscriber-filtered, retention-honest, torn-tail-honest.
- Durability is specified and crash-tested, not assumed: write-ahead invariant, rotation, durable budgets, reconciled gaps.
- Flywheel sheds its only always-on obligation; swarm/loop code assumes wake as infrastructure.

**Negative / costs**

- New runtime to operate: socket + port, unit file, state dir, supervision. Mitigated: toron-parity shape (corrected per D2), single binary, sqlite + journal + JSON sidecars only.
- Migration risk at the inbox seam (C6): mitigated by one-cursor ownership (`(subscriber, scope)`), the Phase-1 journal-diff gate, and at-least-once + dedup-lattice delivery.
- Split-brain surface exists until Phase 3: mitigated by the D9 fence from Phase 1 day one.
- Identity generalization widens the forge surface: mitigated by D6 (authz matrix, egress allowlist, key custody) and Unknown-never-prompts as the default posture for every new identity class.
- Version-skew surface (flywheel ↔ daemon ↔ journal): mitigated by the `ping` handshake, additive envelope versioning, and the retained `WatcherGen` retire gate.
- Two status pages (toron health, doorbell `/health`): accepted; `/health` carries the degraded flags that explain any silence.

**Honesty**

- Phases 0–1 change nothing observable; any delta is a bug. Phase 4 flags default off; "no observable change until opted in" holds end-to-end.
- At-least-once means a crash may re-ring a heard wake; the lattice absorbs it *visibly* (journal shows the re-ring), never silently. The 24h lookback bounds re-delivery volume on lost state.
- Pre-v2 budget durability is a known hole (RAM reset on restart) — documented, tested as such, fixed in Phase 2.
- Digest latency is a policy (300s/10 defaults), not a guarantee; urgent is the only bounded-latency path and its bound is budget-limited.
- `/health` and `stats` report the daemon's view of itself; the journal is the ground truth they point to.

---

## 10. What plane D unlocks — twelve leverage ideas (tiered by phase)

Tier 1 (unlocked by Phases 2–3): **1.** Wake for the unspawned — humans, cron, services subscribe (C2 end). **2.** Tiered attention as a service — per-subscriber budgets + batching + presence-trust everywhere. **3.** Multi-sink delivery — herdr/webhook/DM adapters, reducer untouched (C3 end). **4.** Audit-grade `why` for every wake — replayable msg→delta→Cmd proof, unique to D. **5.** Digest as the morning briefing — per-identity, cross-subscribed-workspace, earliest-deadline-first.

Tier 2 (unlocked by Phase 4 flags): **6.** Cross-project bead unblock — `(workspace, corr)` journal, foreign-close re-ring (C4 end). **7.** Captain-less coalitions — triage + digest with no swarm; captain becomes a subscriber role. **8.** Beads-native SLA engine — hold/wave-gate/T0–T3 ladders as beads-poller wake rules instead of loop polling. **9.** Job/offer routing beyond crews — 43001 first-claimer + mention coalescing as a fair scheduler with budget backpressure. **10.** Reservation-conflict mediation — request-release → respond → transfer with deadlines; silence escalates to digest. **11.** Presence as infrastructure — one shared reachability oracle (20001 + Unknown-never-prompts + forge-reject) instead of per-consumer herdr polling. **12.** Wake analytics → corpus feedback — ring/inject/shed/deaf stats through the ce-compound gate; the attention system tunes itself from its own receipts.

---

## 11. Scenario walk-throughs (S-series, design-level)

- **S1 — Overnight hold fires with no swarm.** *(Phase 4, `beads_poller` + human subscription.)* `br hold --kind captain --corr c1 bd-9x` expires 03:00. Poller emits a Fact on the next 30s pass; D wakes the owner's webhook + morning digest. No flywheel process exists. Pass: digest at 08:00 shows `bd-9x hold expired (corr c1)`; `why` folds the Fact; br sqlite untouched (read-only poll). Rev 1 claimed this without a producer; rev 2 re-phases it honestly.
- **S2 — Cross-repo unblock.** *(Phase 4.)* `bd-a` blocked on `external:toron:bd-77`; D subscribed to both projects; toron `bd-77` closes legally → re-ring with `Pull: br show bd-a`; `why --corr` folds both workspaces.
- **S3 — Deaf service.** CI sink misses three 90s re-rings → budget stops prompting, `Audit doorbell-unanswered-digest` names subscriber+corr+attempts, batches to digest. Pass: journal shows 3 attempts + audit row, zero 4th prompt.
- **S4 — Phase 1 parity.** Wave-swarm fixture under embedded host vs daemon. Pass: `diff <(normalize emb) <(normalize dmn)` empty; any divergence is P0.
- **S5 — Slow sink isolation.** Webhook endpoint stalls 30s. Pass: sink sheds to digest + `Degraded` row; herdr prompt latency unchanged (brain never blocked on the sink).

## 12. Live e2e scenario catalog (E-series; E1–E8 both, E9–E12 after-only, E13–E17 rev-2 additions)

E1–E8 (unchanged from rev 1): arrival handshake; discovery without introduction (roster semantics land with Phase-4 `arrival_triple` — until then E2's pass criterion is the addressed-ring behavior); idle wake delivery; working-pane queue/drain; urgent skips queue; budget exhaustion; deaf pin 90s×3; `why`-fold replay.

- **E9 — Cross-project unblock** (Phase 4): one wake within one sweep of the foreign close; zero flywheel swarm required.
- **E10 — Hold expiry, zero swarm** (Phase 4): wake within one poll of expiry; `ps` proves daemon-only involvement.
- **E11 — Kill swarm, wake survives** (Phase 3): `flywheel kill --force` → daemon green, E3 re-run succeeds post-kill.
- **E12 — Crash-resume at-least-once** (Phase 1): SIGKILL mid-queue → both wakes delivered post-restart, re-ring rows journaled, no budget breach.
- **E13 — Split-brain fence** (Phase 1, rev 2): daemon running, then `flywheel spawn` → embedded defers, exactly one prompt per ring; then daemon killed, lease stale → embedded takes over within 90s; two daemons → second exits 0 over the bind.
- **E14 — Torn journal tail** (Phase 1): truncate the active journal mid-line → resume folds to the gap, `why` reports the stop, `/health` shows `journal_torn_tail`, nothing folds past the gap.
- **E15 — Slow sink** (Phase 2): stalled webhook → shed-to-digest + `Degraded`; brain prompt SLA unaffected (assert herdr prompt latency < 2s throughout the stall).
- **E16 — Version skew** (Phase 2): old CLI vs new daemon → `ping` handshake error names both versions; stale daemon binary → `WatcherGen` retire, unit restarts into current.
- **E17 — Beads poll, zero swarm** (Phase 4): as S1, live: real hold, real expiry, real daemon, `ps` + br sqlite checksum prove no flywheel and no br mutation.

---

## Appendix A — kameo pattern mapping (concepts only; no dependency)

Studied 2026-09-10 (kameo v0.22.2 README, docs.rs, the Book, examples/, kameo_actors). What we take, where it lands, why not the crate:

| kameo concept | semantics there | doorbell landing | why not the crate |
|---|---|---|---|
| bounded mailbox (default 64), `send().await` backpressure, `try_send` → `MailboxFull` | per-actor queue; slow actor stalls only itself | brain inbox (cap 1024) + per-sink queues (cap 64) with shed-to-digest (D3) | two channels, not N actors; shed policy is domain logic |
| `ask` vs `tell`; reply + mailbox dual timeouts; `SendError` taxonomy (`ActorNotRunning/Stopped/Restarting/MailboxFull/Timeout`) | errors as values to the caller | CLI/API verbs = ask (typed busy/wedged/absent/refused); sensors = tell (D3) | oneshot + timeout is 20 lines |
| links / `on_link_died` → `ControlFlow` | peer death as a message; default stop, Continue to survive | sensor/sink death → `Msg::Degraded` into the brain; brain never dies of an edge (D3) | our "link set" is static and tiny |
| supervision policies (Permanent/Transient/Never; capped restarts 5/5s) | restart strategy per child | sink/sink-manager Transient + cap → park + Degraded + audit (D3) | one policy, one manager task |
| panic isolation (`on_panic` → Continue/Break; ask-path Err to caller) | panics never cross the actor boundary | adapter `catch_unwind` → `Msg::Degraded{panic}`; brain pure by construction (D3) | std `catch_unwind` + `JoinError` |
| `stop_gracefully` (drain) vs `kill`; `on_undelivered` (drain leftovers) | clean shutdown semantics | graceful stop drains sink queues + journals undelivered intents → resume redelivers (D4.2) | drain loop over one queue |
| `pipe` (fetch off-loop, deliver back as a message) | keep the actor responsive during I/O | exactly today's `FetchInbox` Cmd → sweep → `InboxRows` Msg; stays (D3) | already hand-rolled |
| PubSub/Broker with `DeliveryStrategy` (BestEffort default) | broadcast with per-subscriber pruning | SSE `events` fan-out + shed-to-digest = BestEffort with a journal (D8) | subscribers ≤ dozens; journal is our broker ledger |
| cheap-clonable `ActorRef`, `Weak` | addresses as values | subscriber/sink handles in the subscription store (D5) | plain struct handles |
| remote actors (libp2p/Kademlia) | transparent distributed ask/tell | **not taken** — cross-host rides toron relays when a second host joins (D10 revisit trigger) | would fork transport away from the bus we already trust |
| what kameo deliberately lacks | persistence/event-sourcing, retries, production proof | journal + dedup lattice + crash-harness are ours (D4, §8) | — |

Net: kameo confirms the shape (single-writer brain, bounded edges, death-as-message) and supplies the vocabulary; everything we need of it is ≤ ~200 lines of tokio plumbing inside `doorbell-rt`, and the parts that matter for *our* guarantees (journal, refold, budgets) are exactly the parts no actor framework provides.

---

## More information

- Rev 1 stack study (BrightOtter, 2026-09-09): reducer/dispatcher seam at `Cmd::from_doorbell`/`from_loop` (`src/workspace.rs`); seven constraints C1–C7.
- Rev 2 evidence base (AmberBeacon, 2026-09-10, flywheel HEAD 6f82af2): dispatcher loop `cmd_internal_doorbell` (`src/main.rs` — unbounded `Bell` mpsc, in-process watch + CLI fallback, per-pane forwarders, 1s sweep, decided-before-delivery journaling); journal caps/torn-skip (`src/doorbell.rs` `journal_append*`/`journal_read_all`); `WakeState`/`note_wake` ratchet; `PinBudget`/`take_allowed` RAM-only; bd-7qg6 (cursor-hold, never at-most-once) and bd-8hv0 (delta re-subscribe) diffs; toron `daemon.rs`/`plane.rs`/`mailbox_source.rs`/`receipts.rs`/`in_process.rs` parity audit; beads `hold.rs`/`storage/` (WAL, lazy expiry); flywheel `cron.rs`/`serve` (no always-on producer); kameo study (Appendix A).
- Prior visual analysis: `.lavish/doorbell-analysis.html` (illustrative, not normative).
- Revisit triggers: toron ships push transport (revisit D5 sink order); second host joins (revisit journal keying + remote actors row of Appendix A); Phase-1 journal-diff fails on any fixture (revisit the D1 cut line before proceeding).

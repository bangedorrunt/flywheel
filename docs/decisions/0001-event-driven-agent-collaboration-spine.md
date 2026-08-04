# ADR-0001 — Event-Driven Agent Collaboration Spine (herdr events + agent-mail parity)

| Field | Value |
|-------|-------|
| **Status** | Proposed |
| **Date** | 2026-08-05 |
| **Decision Maker** | Captain (bangedorrunt) |
| **Supersedes** | Informal decisions baked into the prior `bd-herdr-automation-hardening-*` epic (send-confirm, unknown-skip, adaptive-interval, `bd-protocol-v18-kkd`) |
| **Governs** | `src/herdr.rs`, `src/mail.rs`, `src/durable.rs`, `src/main.rs` (loop core), `src/state.rs` |
| **Verification** | `docs/decisions/_scripts/verify-0001.sh` (see §11) |

---

## 1. TL;DR

flywheel today is a **poll-based, orchestrator-only, one-directional** swarm: the durable loop polls every ~30s, dispatches work top-down via `__send`, and uses agent-mail only to fire-and-forget a single "blocked" notification. Meanwhile both upstream dependencies have evolved far past how flywheel uses them:

- **herdr 0.8.0-preview (socket protocol 19)** exposes a real push **event stream** (26 event types incl. `pane_agent_status_changed`, `pane_agent_detected`, `pane_exited`) that flywheel **subscribes to nowhere** — the loop polls instead, and `serve` even fakes SSE from the poll stream (README:195).
- **agent-mail (46 MCP tools)** — flywheel wraps **8**: `health, ensure_project, list_agents, fetch_inbox, send_message, file_reservation_paths, release_file_reservations, check_file_reservation_conflicts`. The other 38 — including `register_agent`, `reply_message`, `acknowledge_message`, `renew_file_reservations`, `acquire_build_slot`, `search_messages`, `summarize_thread`, `request_contact`, all macros — are **unused**.

**Decision:** Upgrade flywheel to an **event-driven, peer-capable, bidirectionally-coordinating collaboration spine**: (a) bump the protocol pin 18→19 and switch the loop core from poll to `events.subscribe`, (b) register agents on spawn + bridge herdr pane IDs into agent-mail identities, (c) add the missing mail primitives (reply/ack/renew/build-slot/search/cross-project), (d) promote the blocked-notification from one-shot to a tiered escalation protocol, and (e) ship composable recipes for the collaboration patterns the captain asked about (debate loop, plan→review→implement, peer-triggered, cross-project swarm).

This is the **foundational coordination architecture** for everything the project aspires to: multi-agent debates, autonomous plan-review gates, self-healing swarms, and cross-project fan-out (hachiko ↔ flywheel ↔ toron). Without it, those patterns are unreachable.

---

## 2. Background & Motivation

### 2.1 What flywheel is

flywheel is a Rust CLI: a **herdr-backed swarm orchestrator** with an NTM-parity command surface and a **durable monitoring loop** built on Resonate. The loop (`flywheel loop <project>` → `swarm_loop` workflow in `src/durable.rs`) is the orchestrator: it polls the swarm via the hidden `__poll` leaf, feeds queued instructions to idle agents via `__send`, sleeps durably, and exits when the queue drains. Crashes resume by re-running the same command.

```
src/
  main.rs      # clap surface, swarm helpers, sync loop, hidden __poll/__send leaves
  herdr.rs     # socket client: one request per connection, typed helpers, EventStream
  durable.rs   # Resonate workflow (swarm_loop) + leaves + resume/status
  config.rs    # XDG config, recipes/workflows/pipelines/policy
  state.rs     # event log, checkpoints, active-loop registry, analytics
  mail.rs      # agent-mail JSON-RPC over HTTP  ← 8/46 tools
  work.rs      # br/bv pass-through
  output.rs    # TTY-aware color, JSON printing
```

### 2.2 The overarching goal this serves

The captain's north star (from the side-story question): **can agents signal/trigger each other to work/collaborate through agent-mail and herdr autonomously, without HITL?** Concretely: debate loops, plan→review→implement gates, peer-triggered handoffs, cross-project swarms.

The honest answer today: **the primitives exist in the dependencies, but flywire uses them in a one-directional, orchestrator-driven way that cannot express any of those patterns.** This ADR closes that gap.

### 2.3 Why now

A measured audit (2026-08-05, see §3) against the live herdr 0.8.0-preview and the running agent-mail MCP found six concrete defects. Three are load-bearing for collaboration: **(A2)** fake SSE / poll-only, **(A3)** 8/46 mail tools, **(A4)** no herdr-pane ⇄ mail-identity bridge. The other three (protocol drift, `unknown` black hole, one-shot blocked notification) are reliability/safety issues that compound under autonomous operation. Fixing collaboration without fixing reliability first would produce a swarm that coordinates fast but dies silently.

---

## 3. Measured Evidence — the six gaps (2026-08-05 audit)

Each gap was verified against live artifacts, not assumed. Evidence anchors (file:line / live command) are recorded so a future auditor can re-derive.

### A1. Protocol version drift — `src/herdr.rs:19`

```
flywheel: pub const HDR_PROTOCOL: u64 = 18;     # src/herdr.rs:19
herdr live: "protocol": 19  (herdr api schema --json)  # 0.8.0-preview.2026-08-04-d78e3d3b5126
AGENTS.md: "protocol v18"   README:168 "socket protocol v18"
```

`flywheel deps` warns on mismatch (the `bd-deps-protocol-check` bead shipped this), but the **pin itself is stale** — the check is self-defeating when the constant lags the binary. Protocol 19 added fields flywheel's structs already tolerate via `#[allow(dead_code)]` (`agent_session`, `interactive_ready`, `state_labels`, `foreground_cwd`), but a protocol bump that adds a *required* request field would break the client at runtime with no compile-time signal.

### A2. SSE is fake; the loop polls — `README:195`, `src/durable.rs:530`

herdr exposes a real push event stream. The schema enumerates **26 event types**:

```
workspace_{created,updated,metadata_updated,closed,renamed,moved,reordered,focused}
worktree_{created,opened,removed}
tab_{created,closed,renamed,moved,focused}
pane_{created,closed,updated,focused,moved,output_changed,exited,
      agent_detected,agent_status_changed}
layout_updated
```

Plus subscription events: `PaneAgentStatusChangedEvent`, `PaneOutputMatchedEvent`, `PaneScrollChangedEvent`.

flywheel **implements** `EventStream::subscribe` + `events.subscribe` (`src/herdr.rs:311-380`) — and **calls it nowhere in the loop**. The durable loop (`poll_swarm` → `__poll` → `plan_actions`) samples state every `interval_secs` (default 30s; `active_interval_secs` is a band-aid). README:195 admits "serve SSE on real herdr events (currently state-poll stream)".

**Consequence for collaboration:** up to `interval_secs` latency detecting `done`/`blocked` → `idle`; transient states (idle→working→done between two polls) collapse to one sample and are lost; a peer that finishes in 2s waits 28s to be redispatched. For a *debate loop* or *peer-triggered handoff* this latency makes the pattern feel sluggish and, under Resonate durability, burns wall-clock on durable sleeps for no reason.

### A3. agent-mail surface used = 8/46 tools — `src/mail.rs`

Confirmed by grep across `src/`: **zero use of** `register_agent`, `create_agent_identity`, `reply_message`, `acknowledge_message`, `mark_message_read`, `renew_file_reservations`, `acquire_build_slot`, `release_build_slot`, `search_messages`, `summarize_thread`, `request_contact`, `respond_contact`, `set_contact_policy`, all four macros, all product tools. Concrete failure modes:

| Missing tool | Consequence |
|---|---|
| `register_agent` | Orchestrator never registers itself or workers → inbox addressed by a `name` string that may not be a real identity. `send_message` to an unknown recipient **fails fast**. |
| `reply_message` / `thread_id` | No threaded replies. A debate loop can't preserve context — every turn is a detached subject. |
| `acknowledge_message` / `mark_message_read` | Loop can't tell "peer saw it" from "peer ignored it". The `__mail-blocked` notification is fire-and-forget. |
| `renew_file_reservations` | Durable loop runs hours; reservations have TTL. A long-running strategy dies when its lease expires mid-edit — no renew path. |
| `acquire_build_slot` | Parallel workers race `cargo build`/`cargo test` on shared `target/`. No serialization. |
| `search_messages` / `summarize_thread` | Loop can't ask "did anyone already solve this?" before re-dispatching duplicate work. |

### A4. Identity bridge gap — herdr panes ⇄ agent-mail identities are NOT linked

`agent_mail_resolve_pane_identity` checks three locations:
1. `~/.config/agent-mail/identity/<project_hash>/<pane_id>`
2. `~/.claude/agent-mail/identity.<pane_id>` (legacy Claude Code)
3. `/tmp/agent-mail-name.<project_hash>.<pane_id>` (legacy NTM/tmux)

**None of these carry herdr pane IDs** (`wT:p1`). herdr is its own multiplexer, not tmux. So an agent running in a herdr pane has **no automatic agent-mail identity**, and the orchestrator must both `register_agent` *and* write the identity file under the herdr pane id itself. flywheel does neither — it passes `name` as a free string and hopes. The schema's `agent_session` field (`{agent, kind:"path", source:"herdr:pi", value:<session-path>}`) is the natural join key herdr already provides, but agent-mail doesn't read it.

This gap **alone blocks peer-triggered and cross-project collaboration**: without a registered, resolvable identity per pane, agents cannot be reliably addressed, and `resolve_pane_identity` returns nothing for herdr panes.

### A5. `unknown` is a black hole — `src/durable.rs:549`, `src/durable.rs:573`

The loop treats `unknown` as non-dispatchable (`Wait`, never consumes the queue) **and** keeps the loop alive (`all_settled` excludes `unknown`). Correct safety, but there's **no recovery action**: a worker stuck `unknown` (crashed agent, wrong kind, half-dead session) parks forever with the loop spinning at active cadence. No `mail.send` nudge, no `agent read` diagnostic, no respawn. The loop notices but can't act.

### A6. `--on-blocked mail` is one-shot, not a protocol — `src/durable.rs:512`

`notify_blocked` sends one high-importance message and returns. There's no: escalation after N intervals un-acked (the `ack_overdue_only` filter exists in the tool, unused), `set_contact_policy` so blocked agents auto-accept orchestrator pings, or thread continuity so a human/peer sees "still blocked, 3rd ping".

---

## 4. Decision

Adopt an **event-driven, peer-capable, bidirectionally-coordinating collaboration spine** across six workstreams (mapped 1:1 to bead phases in §8). The architectural commitments:

1. **Event-driven, not poll-driven.** The loop core subscribes to `pane_agent_status_changed` + `pane_agent_detected` + `pane_exited` for the project's workspace and reacts in event-time (~ms). Polling is retained **only** as a heartbeat/fallback (determinism under a dead event stream), not the primary signal.
2. **Every spawned agent is a first-class, addressable identity.** `agent start` is followed by `register_agent`; the identity file is written under the herdr pane id so `resolve_pane_identity` works. Spawn and registration are atomic from the loop's view.
3. **agent-mail becomes the async coordination plane; herdr remains the synchronous lifecycle plane.** herdr = spawn/prompt/wait/read (lifecycle). agent-mail = threads/acks/inbox/leases/slots (durable signaling). Patterns compose both.
4. **Blocked states escalate, they don't fire once.** A tiered protocol (ping → ack-overdue re-ping → urgent + thread summary) honors the SLA the tool already encodes.
5. **The `unknown`/`exited` states are recoverable, not terminal.** The loop diagnoses (`agent read` tail + mail), then respawns after N consecutive unclassified polls, with a bounded retry budget.
6. **Collaboration patterns are shipped as recipes**, not left implicit: debate loop, plan→review→implement, peer-triggered, cross-project swarm — each with a documented trigger contract and an e2e test.

---

## 5. Considered Alternatives

| Alternative | Verdict |
|---|---|
| **Keep polling, just lower `interval_secs`.** | Rejected. Shrinks latency but burns CPU/durable-sleep wall-clock; misses transient states; does not fix A3/A4/A5/A6. Treats the symptom. |
| **Adopt a different coordination backend** (e.g. NATS, Redis streams). | Rejected. Both deps are already in the stack and good at their jobs; the gap is *flywheel's usage*, not the tools. New infra = new failure modes + new learning curve, violates "simple over complex". |
| **Move the whole loop into Resonate durable promises keyed on herdr events.** | Partial / deferred. Tempting (every event = a durable promise), but couples two fast-moving surfaces and risks determinism bugs. Keep Resonate for the swarm_loop workflow; events feed the *leaves* (`__poll` event variant, `__send`), not the workflow topology. Revisit if event-volume overwhelms. |
| **Have agents self-register (no orchestrator involvement).** | Rejected for the orchestrator path. Agents are spawned by `agent start` with a prompt; they run one-shot and exit — they can't reliably self-register before their first turn. The orchestrator owns registration (it has the pane id + kind + name at spawn time). Self-poll inbox (peer-triggered) is a *separate* pattern that runs *inside* the agent's prompt, not at registration. |
| **Auto-accept all contact requests (policy=open).** | Rejected as default. `auto` (default in agent-mail) is the right balance for a trusted single-user swarm; `open` removes a safety rail. Patterns that need open coordination set it explicitly per-project. |

---

## 6. Consequences

### Positive
- **Dispatch latency** drops from ~30s (poll) to event-time (~ms). Debate/peer-triggered patterns feel live.
- **Collaboration patterns become expressible** for the first time: threaded debates, ack-gated review gates, peer handoffs, cross-project fan-out.
- **Self-healing**: `unknown`/`exited` recovery turns a silent stall into a respawn-with-diagnostic.
- **Lease safety**: multi-hour strategies keep their file reservations; no more mid-edit death.
- **Build serialization**: parallel workers stop thrashing `target/`.
- **Forward-compat**: protocol check becomes `>=` (forward-compatible), so the next herdr bump is a docs update, not a runtime break.

### Negative
- **Complexity surface grows**: 14 new mail wrappers + event reactor + escalation state machine. More code to test.
- **Event-stream determinism** must be proven: an event-driven loop that drops events is worse than a polling loop. Requires parity tests (§10).
- **Registration churn**: every spawn/close now writes the identity file + DB row. Under rapid respawn loops this is I/O; mitigated by idempotent `register_agent` + cleanup on `pane_exited`.
- **Two sources of truth** for "is this agent done?" (event stream vs poll heartbeat). Must be reconciled in `all_settled`.

### Neutral
- **AGENTS.md + README must be rewritten** to describe the new model. The "protocol v18" references and the fake-SSE note both go.
- **The prior `bd-protocol-v18-kkd` bead is implicitly superseded** by the v19 bump; left open in the tracker as historical context.

---

## 7. Implementation Blueprint — phases & blast radius

Each phase = one or more beads (§8). **Forbidden zone**: do not change the Resonate workflow topology (`swarm_loop` signature, leaf return types) — events feed leaves, not the workflow. **Target zone**: `herdr.rs`, `mail.rs`, `durable.rs` leaves + `plan_actions`, `main.rs` loop driver. **Context zone**: docs, config schema.

### Phase 1 — Protocol & schema currency (foundational)
Bump `HDR_PROTOCOL` 18→19, regenerate wire structs from the bundled schema, type all 26 event variants, flip the deps check to `>=`. Unblocks confident event typing.

### Phase 2 — Event-driven loop core
`events.subscribe` reactor replaces poll as primary; poll demoted to heartbeat. Map `pane_agent_status_changed`/`detected`/`exited` → `LoopAction`. Add `LoopAction::Diagnose` + `LoopAction::Respawn` for `unknown`/`exited` recovery.

### Phase 3 — agent-mail surface expansion
14 new wrappers in `mail.rs`: `register_agent`, `reply_message`, `acknowledge_message`, `mark_message_read`, `renew_file_reservations`, `acquire_build_slot`, `release_build_slot`, `search_messages`, `summarize_thread`, `request_contact`, `respond_contact`, `set_contact_policy`, + the pane-identity bridge writer. Orchestrator auto-registers on spawn.

### Phase 4 — Blocked escalation protocol
Tiered: ping T0 → `ack_overdue_only` re-ping T0+2×interval → `importance=urgent` + `summarize_thread` T0+4×interval. State machine in `durable.rs`.

### Phase 5 — Collaboration pattern recipes
Four documented, tested recipes (debate / plan-review-implement / peer-triggered / cross-project) implemented as reusable prompt templates + loop configs, each with an e2e test in an isolated herdr test session.

### Phase 6 — Tests & hardening
Differential parity (event-loop == poll-loop behavior), e2e pattern soak tests, docs rewrite (AGENTS.md + README collaboration section + recipes).

---

## 8. Bead map (dependency DAG)

See `.beads/` (created by `br`). Dependency edges (child → parent = "child depends on parent"):

```
EPIC: bd-event-driven-collab-spine-???

Phase 1 (foundation, no deps):
  bd-protocol-v19-bump-???
  bd-protocol-forward-range-???      → depends on protocol-v19-bump
  bd-schema-event-typing-???         → depends on protocol-v19-bump

Phase 2 (event loop, depends Phase 1):
  bd-event-subscribe-reactor-???     → depends on schema-event-typing
  bd-event-dispatch-zero-latency-??? → depends on event-subscribe-reactor
  bd-event-self-heal-unknown-???     → depends on event-subscribe-reactor

Phase 3 (mail surface, mostly independent):
  bd-mail-register-on-spawn-???      (no dep)
  bd-mail-pane-identity-bridge-???   → depends on mail-register-on-spawn
  bd-mail-reply-thread-???           → depends on mail-register-on-spawn
  bd-mail-ack-sla-tracking-???       → depends on mail-register-on-spawn
  bd-mail-lease-renewal-durable-???  (no dep)
  bd-mail-build-slot-gating-???      (no dep)
  bd-mail-search-summarize-???       (no dep)
  bd-mail-cross-project-contact-???  → depends on mail-register-on-spawn

Phase 4 (blocked protocol):
  bd-blocked-escalation-protocol-??? → depends on mail-ack-sla-tracking

Phase 5 (recipes, depend Phases 2+3):
  bd-pattern-debate-loop-???         → depends on mail-reply-thread, mail-ack-sla-tracking, event-dispatch-zero-latency
  bd-pattern-plan-review-implement-??? → depends on mail-reply-thread, mail-build-slot-gating, event-dispatch-zero-latency
  bd-pattern-peer-triggered-???      → depends on mail-register-on-spawn, mail-ack-sla-tracking, event-dispatch-zero-latency
  bd-pattern-cross-project-swarm-??? → depends on mail-cross-project-contact, pattern-debate-loop

Phase 6 (tests & hardening):
  bd-test-event-poll-parity-???      → depends on event-dispatch-zero-latency
  bd-test-collab-patterns-e2e-???    → depends on pattern-debate-loop, pattern-plan-review-implement, pattern-peer-triggered
  bd-docs-collab-spine-recipes-???   → depends on all four pattern beads
```

Hash suffixes are assigned by `br create --slug`; the DAG above is realized by `br dep add`.

---

## 9. The collaboration patterns (side-story, elaborated)

These are what the captain asked about. Each is expressible **only after** Phases 1-3 land. Documented here as the contract the Phase-5 recipe beads implement.

### 9.1 Pattern — Debate loop (agent A and B argue N rounds, judge decides)
```
orchestrator: register A, B, judge; thread = "debate-<ts>"
send(A, "Argue FOR X. Reply in thread.", thread_id=thread)
loop N rounds:
  wait A → done; reply = last message in thread from A
  send(B, "Counter: {reply}. Reply in thread.", thread_id=thread)
  wait B → done; reply = last message in thread from B
  send(A, "Counter: {reply}.", thread_id=thread)
send(judge, "Read thread {thread}, pick winner in <100 words.", thread_id=thread)
wait judge → done; read judge pane; mail summary
```
**Requires:** `reply_message` + `thread_id` (A3), `acknowledge_message` for turn-finality, `summarize_thread` for the judge's read. Without threading each "counter" is detached and context is lost.

### 9.2 Pattern — Plan → Review → Implement (classic gate)
```
spawn planner (codex), reviewer (claude-code), implementer (codex)
herdr agent prompt planner "<feature>" --wait
planner writes plan to /tmp/plan.md, replies "plan at <path>"
read planner → extract path
send(reviewer, "Review {path}. ack_required. Reply APPROVE or CHANGES.", thread="plan-<ts>")
wait reviewer → done (or blocked → escalate)
read reviewer reply:
  APPROVE   → acquire_build_slot; send(implementer, "Implement {path}"); prompt --wait; release_build_slot
  CHANGES   → reply planner with changes; loop back to planner
on implementer done → send(reviewer, "Verify diff matches plan"); gate
```
**Requires:** `acknowledge_message` (structured approve/reject, not free text), `acquire_build_slot` (implementer + reviewer's test-run don't fight over `target/`), and a "reply → re-route" branch the loop lacks today.

### 9.3 Pattern — Peer-triggered work (agent initiates, not orchestrator)
The one flywheel **cannot** do today. An idle agent noticing something wakes a peer:
```
agent A (running its own loop inside its prompt):
  poll own inbox: fetch_inbox(self, unread_only=true)
  if urgent msg from B: reply + herdr agent prompt B "<help>" --wait
```
**Blockers today:** A3 (no `register_agent` → inbox unaddressable) + A4 (no pane identity → `resolve_pane_identity` empty). Both closed by Phase 3. The spawned agent's prompt must include "on idle, fetch_inbox and act on urgent messages for up to M minutes".

### 9.4 Pattern — Event-driven fan-out (Phase 2 unlocked)
With `events.subscribe` instead of poll:
```
subscribe workspace W to pane_agent_status_changed
on event(status=done, agent=X): send(X, "Next bead: br-124")        # zero-latency dispatch
on event(status=blocked, agent=Y): send(reviewer, "Y blocked: {read Y tail}, help")  # peer-assist
on event(type=pane_exited, agent=Z): register cleanup + respawn     # self-heal
```
The orchestrator becomes a **reactor**, not a poller.

### 9.5 Pattern — Cross-project swarm (hachiko ↔ flywheel ↔ toron)
```
hachiko-strategist wants flywheel to orchestrate a backtest swarm:
  request_contact(from=hachiko-strategist, to=flywheel-orch, to_project=flywheel, reason="run 4-worker backtest")
  flywheel-orch: respond_contact(accept=true)
  send(to=flywheel-orch, "spawn 4 workers, recipe=backtest-5m, workflow=cross-val")
```
**Requires:** `request_contact`/`respond_contact`/`products_link` (A3, unused). Today the 5 live workspaces are isolated.

---

## 10. Test strategy

- **Differential parity** (`bd-test-event-poll-parity`): run the event-driven loop and the poll loop against the same scripted herdr test session (named session, deterministic agent scripts). Assert dispatch order, latency bounds, and final state are identical. Catches event-drop regressions.
- **E2E pattern soak** (`bd-test-collab-patterns-e2e`): each Phase-5 recipe runs in an isolated herdr test session (`herdr --session test-<pattern>`) with scripted agents, asserting the pattern's contract (debate produces a winner; plan-review either implements or loops; peer-triggered delivers help within M ms; cross-project fan-out spawns N workers). Per AGENTS.md: NO script-based code changes — these are *test harnesses*, not code mutators.
- **Unit**: each new `mail.rs` wrapper has a JSON-roundtrip test (mirrors existing `tool_result_parses_text_json` style); each event variant has a serde round-trip; the escalation state machine has table-driven tests.
- **Compliance gate**: `docs/decisions/_scripts/verify-0001.sh` asserts (a) `HDR_PROTOCOL >= 19`, (b) `register_agent` called in the spawn path, (c) no bare `sleep` in the event-driven dispatch path, (d) `br dep cycles` empty for the bead DAG.

---

## 11. Verification (executable)

```bash
# docs/decisions/_scripts/verify-0001.sh
#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# (a) protocol forward-compat
grep -qE 'pub const HDR_PROTOCOL: u64 = (19|[2-9][0-9]);' src/herdr.rs \
  || { echo "FAIL: HDR_PROTOCOL not >= 19"; exit 1; }

# (b) register-on-spawn wired
grep -q 'register_agent' src/main.rs src/durable.rs \
  || { echo "FAIL: register_agent not called in spawn path"; exit 1; }

# (c) no bare sleep in the event dispatch path
! grep -nE 'std::thread::sleep|tokio::time::sleep' src/durable.rs | grep -v '#' \
  || { echo "WARN: bare sleep in durable.rs — verify it is heartbeat-only"; }

# (d) bead DAG acyclic
br dep cycles --json | grep -q '"cycles":\[\]' \
  || { echo "FAIL: bead dependency cycle exists"; exit 1; }

echo "OK: ADR-0001 verification passed"
```

---

## 12. Open questions

1. **Event volume under large swarms.** At what agent count does `pane_output_changed` flooding overwhelm the event socket? May need subscription filtering (subscribe only to status/exited, not output). Defer to Phase 2 measurement.
2. **Resonate + events coupling depth.** Should `pane_exited` trigger a durable promise resolution (modeling the agent as a promise), or stay in the leaf layer? §5 deferred this; revisit if Phase 2 shows event-drop risk.
3. **Contact policy default for cross-project.** `auto` (agent-mail default) vs `open` for the trusted single-user swarm. Lean `auto`; patterns set explicitly.
4. **Identity-file cleanup race.** On rapid respawn, can the old identity file's mtime race the new write? Mitigated by idempotent register + pane_id uniqueness; verify in Phase 3.

---

## 13. References

- Live herdr: `herdr api schema --json` (protocol 19, 0.8.0-preview.2026-08-04-d78e3d3b5126); `herdr api snapshot` (5 workspaces observed 2026-08-05).
- Live agent-mail: 46 MCP tools enumerated via `mcp({server:"agent-mail"})`.
- Skills: `~/.agents/skills/herdr/SKILL.md`, `~/.agents/skills/agent-mail/SKILL.md`, `~/.agents/skills/beads-workflow/SKILL.md`, `~/.agents/skills/br/SKILL.md`.
- Prior epic: `.beads/beads.base.jsonl` (`bd-herdr-automation-hardening-q9t` + 17 children; status=open, DB reset — historical context).
- Code anchors: `src/herdr.rs:19` (protocol pin), `src/herdr.rs:311` (EventStream), `src/durable.rs:512` (notify_blocked), `src/durable.rs:530-580` (plan_actions / all_settled), `src/mail.rs` (8-tool surface), `README:168,195`.

---

## 14. Changelog

| Date | Change |
|------|--------|
| 2026-08-05 | Initial draft (Proposed). Captures the 2026-08-05 audit of herdr 0.8.0-preview + agent-mail 46-tool surface against flywheel's current implementation. |

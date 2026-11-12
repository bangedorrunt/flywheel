<!-- governed-by: ADR-0005, ADR-0010, ADR-0012, ADR-0016, ADR-0018, toron ADR-0007, toron ADR-0020, toron ADR-0027, toron ADR-0037 -->
---
status: accepted
date: 2026-11-12
decision-makers: Captain (bangedorrunt)
consulted: |
  AmberHarbor (captain stack analysis 2026-11-12);
  BlackGorge & FoggyDesert (P2P multiplayer mesh & Chie preflight live execution proofs);
  toron-core / toron-plane / toron-workflow maintainers;
  elm-effect-rs maintainers.
informed: flywheel × toron × beads maintainers; universal harness agents (opencode/pi/antigravity/gemini/claude/codex/grok)
supersedes: |
  Amends and consolidates ADR-0012 (independent daemon on :18980 retired; Plane D merged into toron serve on :18080)
extends: |
  ADR-0005 (always-on collab baseline, peer-aware mode),
  ADR-0010 (delivery semantics, attention triage, keyed decision lifecycle),
  ADR-0016 (functional effect kernel, pure tick, journal honesty),
  ADR-0018 (elm-effect-rs is the core),
  toron ADR-0020 (Nostr two-plane CLI and crew steering),
  toron ADR-0027 (one-workspace tick control plane)
---

# ADR-0020: Consolidate Plane D into Toron, In-Process Multiplayer P2P Mesh, and Atomic Bead-Lease Convergence

| Field | Value |
|---|---|
| **Status** | **Accepted** (2026-11-12) |
| **Decision-makers** | Captain (`bangedorrunt`) |
| **Supersedes** | Consolidates [ADR-0012](0012-doorbell-plane-independent-runtime.md) (retires port **`:18980`** and `com.doorbell.serve`) |
| **Extends** | [ADR-0005](0005-always-on-collab-baseline-and-command-surface.md), [ADR-0010](0010-delivery-semantics-attention-triage-decision-lifecycle.md), [ADR-0016](0016-functional-effect-kernel-redesign.md), [ADR-0018](0018-elm-effect-is-the-core.md) |
| **Does not amend** | One-way dependency invariant (`flywheel → toron-workflow → toron-core`); Tick Law (`(Model, Msg) -> (Model, Vec<Cmd>)`); Nostr cryptographic envelope signatures; Fail-closed bead close ceremony |
| **Governs** | Retirement of standalone `doorbell` daemon binary and port `:18980`; Integration of `crates/doorbell-rt` into `toron serve` (:18080); In-process `toron_core::MailSurface` fast-path in `flywheel peer`; Dynamic closed-bead auto-expiration in `toron-core` reservations; Swarm statechart unification under `elm-effect-xstate` |
| **Tracker** | `bd-unify-plane-d-toron-multiplayer` |
| **Verification** | `bash scripts/verify-clippy.sh`, `flywheel stack status --check`, `mbx test --workspace` |

---

## 1. TL;DR

Today's triad (**`flywheel` × `toron` × `doorbell`**) is functionally sound but architecturally fragmented across **3 binaries, 2 daemons, and 2 HTTP ports (`:18080`, `:18980`)**. This creates material IPC latency (40–120ms per message), duplicate wake paths (Doorbell Plane D vs direct P2P pane wakes), and multi-store convergence lag between `.beads/`, SQLite leases, and loop state.

**Decision**:
1. **Consolidate Plane D into Toron**: Fold `doorbell-rt`'s wake debouncing, coalescing, and sensor pipeline directly into the long-running `toron` daemon (`toron serve` on `:18080`). Sunset port `:18980`, the standalone `doorbell` daemon binary, and `com.doorbell.serve`.
2. **In-Process P2P Multiplayer Fast-Path**: Migrate `flywheel peer` commands from shelling out to child CLI processes (`Command::new("toron")`) to invoking `toron_core::MailSurface` in-memory with direct Rex/HC pane wake injection, reducing peer negotiation latency from **~120ms to < 15ms**.
3. **Atomic Bead-Lease Convergence (Zero-Sweep)**: Teach `toron-core` reservation conflict checks to dynamically resolve reason bead status from `.beads/issues.jsonl`. Conflicting leases whose reason bead is closed are treated as automatically expired in the same read, eliminating out-of-band sweep delays and grace-period stalls.
4. **Swarm Statechart Unification**: Model the multi-agent swarm lifecycle, P2P arbitration, and claim-watch as a formal hierarchical statechart using `elm-effect-xstate`.

---

## 2. Context & Problem Statement

### 2.1 Tri-Daemon Sprawl and Host Collisions
The stack currently requires operator supervision over three separate binaries:
- `flywheel` (fleet orchestrator, session door, durable loop)
- `toron` daemon on `:18080` (Nostr mailbox, SQLite outbox loop, reservation lease board, MCP server)
- `doorbell` daemon on `:18980` (wake debouncing, wake state storage, watchbeat)

Every stack installation (`flywheel stack install`, `scripts/stack.sh`) must compile, install, codesign, and verify three binaries, manage two LaunchAgent/systemd services, and handle host PID collisions (e.g. `refusing pid ... for flywheel-flywheel: command is not a flywheel doorbell host`).

### 2.2 The "Two Bells" Paradox (Plane D vs Lateral P2P Wakes)
Doorbell was spun out in ADR-0012 to provide an independent wake arbiter and debounce sink so that `send --wait` had independent wake receipts (`GS-037` / `GS-039`). However, in the modern multiplayer mesh:
- Lateral P2P directives (`flywheel peer intent`, `request-release`, `release-response`, `review-request`) write immediate `⟦p2p:<verb>⟧` wake lines directly to recipient panes via `toron-plane::backend`.
- Toron records durable cryptographic message delivery cursors (`DeliveryCursor`, `read_ts_us`, monotonic `wm`).
- Consequently, Doorbell Plane D on `:18980` and the P2P multiplayer wake pipeline on `toron-plane` now represent **duplicate, overlapping wake delivery routes**.

### 2.3 IPC Friction Over Co-Located Crates
`flywheel`'s `Cargo.toml` already links `toron-core`, `toron-plane`, and `doorbell-core` in-process. Despite this, runtime commands such as `flywheel peer` still shell out to child processes (`toron mail send`) or make loopback HTTP requests to `127.0.0.1:18080`. Spawning a subshell per message costs **40–80ms**, introducing human-perceptible friction during high-frequency peer coordination.

### 2.4 Multi-Store Convergence Lag
Work state lives in `.beads/*.jsonl` (`br`), file leases live in `index.sqlite` (`toron`), and supervisor loop state lives in `loops.json` (`flywheel`). When an agent closes a bead:
- The lease in Toron SQLite becomes stale, but remains on the board until an explicit `toron reserve sweep --closed-beads` command or until Flywheel's `claim_watch` runs multiple periodic grace ticks.
- This creates an unnecessary window of lock contention where an unassigned agent is blocked on a path whose work is already closed and committed to HEAD.

---

## 3. Decision Drivers

1. **Sub-20ms P2P Coordination**: Agents must negotiate file locks, handovers, and review requests laterally without captain bottlenecking or process-spawn overhead.
2. **Minimal Operational Footprint**: Reduce running daemons from 2 to 1, and installed stack binaries from 3 to 2 (`flywheel` + `toron`).
3. **Single Source of Wake Truth**: Eliminate the "two bells" paradox by routing all wakes through a unified event arbiter.
4. **Zero-Sweep Dynamic Leases**: File reservations for closed beads must clear atomically at the instant of bead closure.
5. **Strict Preservation of Effect Law**: Pure Elm tick law `(Model, Msg) -> (Model, Vec<Cmd>)`, journal honesty, and test sandboxing must remain inviolable.

---

## 4. Considered Options

### Option 1: Maintain Tri-Daemon Status Quo
- *Pros*: Zero migration effort; existing ADR-0012 tests pass.
- *Cons*: Retains port `:18980` collision hazards, 3-binary maintenance overhead, and duplicate wake injection mechanisms.

### Option 2: Absorb Doorbell into Flywheel CLI
- *Pros*: Eliminates Doorbell daemon.
- *Cons*: Breaks multi-workspace coverage. When Flywheel CLI terminates, background wake debouncing and watchbeat monitoring across other projects halt. Violates independent wake receipt rules.

### Option 3 (Selected): Consolidate Plane D into Toron Daemon, In-Process P2P Mesh, Dynamic Leases
- *Pros*:
  - Consolidates background services into **one daemon** (`toron serve` on `:18080`) covering all workspaces.
  - Eliminates port `:18980` and `com.doorbell.serve`.
  - Replaces CLI subshell calls with in-memory `MailSurface` calls (< 15ms latency).
  - Eliminates stale lease lockouts via dynamic bead-status resolution.
  - Maintains full backward compatibility with the CLI surface.
- *Cons*: Requires folding `doorbell-rt` into `toron` and updating stack installation scripts.

---

## 5. Architectural Decisions

```
+-----------------------------------------------------------------------------+
|                               FLYWHEEL                                      |
|  - Session Door (arrive / briefing / decide)                                |
|  - Workflow Composition (wave-swarm, coalition-form)                       |
|  - Elm Effect Swarm Statechart (elm-effect-xstate pure reducer)            |
|  - In-Process P2P Fast-Path (direct MailSurface::send + Rex/HC write)      |
+-------------------------------------+---------------------------------------+
                                      | in-process cargo link
                                      v
+-----------------------------------------------------------------------------+
|                          TORON DAEMON (:18080)                              |
|  - Identity & Cryptography (NIP-44, NIP-01, Secp256k1)                     |
|  - Durable Store (index.sqlite WAL, Outbox Loop, Monotonic Cursors)         |
|  - Dynamic Atomic Leases (Zero-sweep self-reconciling reservations)         |
|  - Plane D Wake Engine (Embedded Doorbell RT: In-memory debounce/coalesce) |
|  - Pane Plane Port (Rex primary, HC fallback via toron-plane)              |
|  - MCP Server Surface (41 tools, 25 resources)                              |
+-----------------------------------------------------------------------------+
```

### D1: Consolidate Plane D (Doorbell RT) into Toron Daemon (`:18080`)
1. The `crates/doorbell-rt` runtime library is embedded directly into `toron`'s daemon runtime (`toron serve`).
2. Toron daemon hosts the wake arbiter, wake-state tracking (`load_wake_state`), and sensor loops alongside its existing SQLite outbox and MCP servers.
3. Standalone port **`:18980`** and LaunchAgent unit `com.doorbell.serve` are officially deprecated and retired.
4. `flywheel stack install` and `scripts/stack.sh` build and install two binaries:
   - `flywheel`
   - `toron`
5. `flywheel stack status --check` verifies `toron` daemon health and embedded Plane D wake status.

### D2: In-Process P2P Multiplayer Fast-Path
1. `src/cmd/peer.rs` (`PeerCmd::{Intent, RequestRelease, ReleaseResponse, ReviewRequest}`) is refactored to call `toron_core::MailSurface::send` directly in-memory instead of executing child processes via `Command::new("toron")`.
2. Reactive wake line formatting (`⟦p2p:<verb>⟧ from <sender> for <path> (wm <hex>) — run: toron next`) delivers directly via `toron-plane::backend` to the target pane.
3. Envelope durability and cryptographic signing occur directly within the caller's thread before pane wake injection, preserving fail-closed auditability.

### D3: Atomic Bead-Lease Self-Reconciliation (Zero-Sweep)
1. `toron_core::mailbox::reservations::check_conflicts` and `acquire` are updated: when evaluating an existing lease whose `reason` matches a bead ID (`bd-*`), the engine dynamically inspects the local `.beads/issues.jsonl` (or cache).
2. If the reason bead has status `closed`, the conflicting lease is treated as **automatically expired** in the current transaction.
3. The next competing agent acquires the lock immediately without waiting for `toron reserve sweep` or `claim_watch` grace-tick expiration.

### D4: Hierarchical Swarm Statechart via `elm-effect-xstate`
1. The swarm lifecycle (Classify $\to$ Dispatch $\to$ Execute $\to$ Review $\to$ Verify) is unified into an `elm-effect-xstate` hierarchical state machine.
2. Events (`Msg::P2PDirective`, `Msg::StageDone`, `Msg::TrapDetected`) enter the pure reducer `(Model, Msg) -> (Model, Vec<Cmd>)`.
3. Commands remain pure values (`Cmd::WritePaneWake`, `Cmd::AcquireLease`, `Cmd::EvaluateJudgment`), interpreted off-tick by the runtime.

### D5: TypeSafe AI System One Advisory Integration (ADR-0016)
1. The advisory judgment packs added in `bd-typesafe-p2p-arbitrator-ncigb`:
   - `defaults/judgment-packs/p2p-conflict-resolution.v1.yaml` (Choice: `partition-by-function`, `sequence-a-then-b`, `sequence-b-then-a`, `escalate-to-captain`)
   - `defaults/judgment-packs/peer-review-severity.v1.yaml` (Score: 1..4, blocking $\ge 3$)
   are formalized as standard advisory evaluators during P2P lock contention and code review.
2. System One judgments remain strictly advisory evidence; when offline or low-confidence, fail-safe heuristics and captain escalation guarantee unblocked progression.

---

## 6. Consequences & Invariants

### 6.1 Positive Consequences
- **Halved Daemon Overhead**: Only one background daemon (`toron`) runs per host, eliminating cross-daemon port desynchronization.
- **8x Faster Peer Negotiation**: In-memory P2P message gift-wrapping and direct Rex injection reduce lateral coordination latency to **< 15ms**.
- **Zero-Latency Lock Reclaim**: Bead closure immediately frees file reservations, eliminating lock stalls during high-velocity multi-agent waves.
- **Simplified Ops**: Clean-proof builds, clippy checks, and stack management deal with 2 binaries instead of 3.

### 6.2 Negative Consequences & Mitigations
- *Migration Requirement*: Operators running existing `com.doorbell.serve` LaunchAgents must run `doorbell stop` and unload the unit.
  - *Mitigation*: `flywheel stack install` automatically checks for and unloads legacy `com.doorbell.serve` units during upgrade.
- *Dependency Coupling*: Toron daemon now embeds wake debouncing logic.
  - *Mitigation*: The one-way dependency law (`flywheel → toron-workflow → toron-core`) is strictly preserved; Doorbell RT is incorporated into Toron's plane layer without reverse dependencies into Flywheel.

### 6.3 Non-Goals
- Merging Flywheel and Toron into a single monorepo repository (they remain distinct sibling codebases with one-way cargo links).
- Replacing Nostr cryptographic envelopes with unverified in-memory RPC (all messages remain durably signed and recorded in SQLite).
- Bypassing the Elm tick law (reducers remain 100% pure).

---

## 7. Implementation Plan

```
Phase 1: In-Process P2P Fast-Path (Flywheel)
   │
   ├── Refactor `src/cmd/peer.rs` to call `toron_core::MailSurface::send` in-memory.
   ├── Wire direct pane wake delivery via `toron-plane::backend`.
   └── Unit & integration tests in `cmd::peer::tests`.

Phase 2: Atomic Bead-Lease Self-Reconciliation (Toron)
   │
   ├── Update `toron-core` reservations conflict check to resolve `.beads/` status dynamically.
   ├── Test immediate acquisition over closed reason beads without prior sweep.
   └── Verify `toron reserve acquire` unit tests.

Phase 3: Plane D Consolidation into Toron Daemon
   │
   ├── Embed `doorbell-rt` wake engine into `toron serve` Tokio runtime.
   ├── Update `scripts/stack.sh` and `flywheel stack install` for 2-binary footprint.
   ├── Sunset port `:18980` and deprecate `crates/doorbell-cli`.
   └── Verify `flywheel stack status --check` under unified daemon.

Phase 4: Swarm Statechart Unification (elm-effect-xstate)
   │
   ├── Model wave-swarm and claim-watch transitions in `elm-effect-xstate`.
   ├── Wire P2P directives and Chie memory trap events into reducer `Msg`.
   └── Add formal statechart verification suite.
```

### 7.1 Affected Paths
- `flywheel/src/cmd/peer.rs`: In-memory `MailSurface` dispatch.
- `flywheel/src/cmd/send.rs`: In-memory send path.
- `flywheel/src/judgment.rs`: P2P arbitration and review severity packs.
- `flywheel/scripts/stack.sh`: 2-binary stack management.
- `flywheel/AGENTS.md` & `~/.agents/AGENTS.md`: Update operational contract.
- `../toron/crates/toron-core/src/mailbox/reservations.rs`: Dynamic closed-bead check.
- `../toron/crates/toron-cli/src/daemon.rs`: Embedded Plane D wake engine.

---

## 8. Verification Criteria

- [x] **P2P Latency Benchmark**: `flywheel peer request-release` completes in $< 20\text{ms}$ when running in-process (verified in `bd-p2p-inprocess-fastpath-ljllj`).
- [x] **Zero-Sweep Lease Reclaim**:
  ```bash
  toron reserve acquire src/test.rs --reason bd-test-close --project flywheel --as AgentA
  br close bd-test-close --task-id bd-test-close --verdict-id test --commit-sha <sha>
  toron reserve acquire src/test.rs --reason bd-test-next --project flywheel --as AgentB
  # Succeeded immediately without running `toron reserve sweep` (verified in `bd-atomic-bead-lease-reconciliation-0zvt7`)
  ```
- [ ] **Stack Install Parity**:
  ```bash
  flywheel stack install
  flywheel stack status --check
  # Reports flywheel and toron healthy; 0 references to dead :18980 port.
  ```
- [ ] **Effect Law & Clean-Export Proof**:
  ```bash
  bash scripts/verify-clippy.sh
  bash scripts/clean-proof.sh --tests --keep
  # Passes cleanly with zero warnings and zero ambient leaks.
  ```

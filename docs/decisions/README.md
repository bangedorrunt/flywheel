# Architecture Decision Records (ADR)

This directory contains records of the major architectural decisions made for the `flywheel` project.

| ID | Title | Status |
| :--- | :--- | :--- |
| [0001](0001-event-driven-agent-collaboration-spine.md) | Event-Driven Agent Collaboration Spine (herdr events + agent-mail parity) | **Accepted** (2026-08-05; loop poll→events, pane↔mail bridge, mail primitives, tiered escalation, composable recipes; review-fix addendum 2026-08-05) |
| [0002](0002-pluggable-backend-abstraction.md) | Pluggable Backend Abstraction (MultiplexerBackend + MailBackend) | **Accepted** |
| [0003](0003-reintroduce-agent-mail-backend.md) | Reintroduce a Real agent-mail Backend (per-backend config, XDG, parity) | **Partially superseded** by toron ADR-0007 se2.3 (MailSurface trait unification) — config split, parity harness, per-backend routing survive |
| [0004](0004-flywheel-web-presence.md) | flywheel Web Presence — Orchestration-Layer Presentation | **Accepted** (2026-08-13; flywheel = section of toron.dev per toron ADR-0013 D5; layer-ownership table, Orchestration pillar, 3 diagrams E/W/CO, docs sections, spin-out path to flywheel.dev) |
| [0005](0005-always-on-collab-baseline-and-command-surface.md) | Always-On Collaboration Baseline + Composable Markdown Command Surface | **Accepted** (2026-08-20; amended 2026-10-25: `/loop-mode` is a command inside open, not a mode) |
| [0006](0006-parameterized-stage-payloads-and-wave-scoped-dispatch.md) | Parameterized Stage Payloads + Wave-Scoped Dispatch (the unattended wave swarm) | **Accepted** (2026-08-22; D1–D4; `verify-0006.sh` 9/9; graduation GREEN 2026-08-23 — composed wave-swarm closed fixture bead grad-fw-43424-9da via legal close, kept to catalog) |
| [0007](0007-catalog-slots-and-agent-native-cli.md) | Catalog slots + agent-native catalog CLI | **Accepted** (2026-09-05; D1 slots=harness+model, D2 six job profiles, D3 compiled-in squads, D4 catalog CLI+TUI; Wave 7 catalog-only; `verify-0007.sh` 14/14) |
| [0008](0008-workflow-compose-topology-agent-optimal-mental-model.md) | Workflow / Compose / Topology — Agent-Optimal Mental Model | **Accepted** (2026-09-05; amended 2026-10-25: modes are open and swarm; loop is not a mode) |
| [0009](0009-compound-spine-ce-vocabulary-and-learnings-corpus.md) | Compound Spine — ce- Vocabulary, Compounded wave-swarm (8 phases), Learnings Corpus | **Accepted** (2026-09-06; amended 2026-10-25: D6 loop-mode is the open-mode bead command) |
| [0010](0010-delivery-semantics-attention-triage-decision-lifecycle.md) | Adopt delivery semantics, attention triage, and a keyed decision lifecycle | **Accepted** (2026-09-07; captain; D0–D15; HarborPilot + KeenAuditor agreed on `adr-0010-peer-review`) |
| [0011](0011-session-conductor-crew-budget-captain-fcfs-join-loud.md) | Session conductor — crew budget N, captain FCFS, join-loud | **Accepted** (2026-09-08; amended 2026-10-25: mode question is open or swarm) |
| [0012](0012-doorbell-plane-independent-runtime.md) | Spin doorbell out as an independent plane (always-on wake runtime) | **Accepted** (2026-09-10; bd-asr2 Phase 0: doorbell-core extracted, `verify-0012.sh` green; Phases 1–4 in bd-asr2.1–.19) |
| [0013](0013-agent-profile-lifecycle-journaled-rebind.md) | Agent-profile lifecycle and journaled fallback rebind | **Accepted** (2026-09-12; GS-044; ADR-0032 Spec 7 honesty; Wave A + Wave B landed, Wave C open) |
| [0014](0014-jev-system-one-judgment-adapter.md) | Jev / System One as host-side judgment adapter | **Proposed** (2026-09-19; GS-055; HOLD merge; W1–W3 HOLD implement; companion toron ADR-0033 / beads ADR-0006) |
| [0015](0015-effect-oriented-adoption.md) | Effect-oriented discipline across the stack | **Accepted** (2026-09-19; wrap order superseded by ADR-0016 §5) |
| [0016](0016-functional-effect-kernel-redesign.md) | Adopt one typed functional effect kernel across flywheel | **Accepted** (2026-09-21; sequential implementation Phases 0–7) |
| [0017](0017-shared-foldcore-crate.md) | Shared fold crate lives in toron | **Rejected** (2026-10-11; never created; superseded by elm-effect-rs ADR-0002 + toron ADR-0040) |
| [0018](0018-elm-effect-is-the-core.md) | elm-effect-rs is the core of the stack | **Accepted** (2026-10-22; the leaf port landed in flywheel `bd-f572` and toron `bd-6iyy`; the comms kernel, the doorbell programs, the supervisor program and the breaker-to-policy moves are still owed) |

## Related toron ADRs

flywheel is tightly coupled to toron (toron carries; flywheel drives). Key cross-references:

- **toron ADR-0003** — the Nostr mailbox fabric flywheel consumes
- **toron ADR-0007 se2.3** — the unified `MailSurface` trait both backends implement (supersedes flywheel ADR-0003's `MailBackend`)
- **toron ADR-0013** — toron.dev website architecture; flywheel is a section (D5). *Moved 2026-08-14 to the [toron.dev repo](https://github.com/bangedorrunt/toron.dev), renumbered ADR-0001*
- **toron ADR-0015** — flywheel ↔ toron integration contract (one mail surface, one-way dep)
- **toron [ADR-0019](../../toron/docs/decisions/0019-bead-verified-execution-and-principle-playbooks.md)** — bead-verified execution, baked poteto principles, playbook workflows (proposed 2026-08-15). Wave 1 lives in flywheel (`ledger.rs`, loop reopen, briefs). Wave 4 is this repo's ADR-0001 A2 leftover.
- **toron [ADR-0020](../../toron/docs/decisions/0020-nostr-two-plane-cli-and-crew-steering.md)** — CLI-first Nostr rooms/jobs/presence/watch so crews steer without flywheel `send`. No new flywheel subcommands in wave 1.
- **toron [ADR-0024](../../toron/docs/decisions/0024-graph-first-repair-loops-bead-workflow.md)** — graph-canonical workflow engine, typed runtime failure, unrolled repair (Accepted in toron; VerifySink is the live compose/graph door — status corrected 2026-09-04, GS-016).
- **flywheel [ADR-0008](0008-workflow-compose-topology-agent-optimal-mental-model.md)** — workflow/compose/topology agent-optimal mental model (Accepted 2026-09-05; implemented 2026-09-05). Does not amend toron ADR-0027. Live `is_compose_token` = wave-swarm | repair-until-verified | beads-lifecycle.
- **toron [ADR-0025](../../toron/docs/decisions/0025-adopt-graph-dispatch-artifact-reduce-pin-supervision.md)** — overlay: lease reclaim, typed artifacts, pin supervision, plane routing table, ingress + launch idempotency. Extends this repo's ADR-0001 `LoopAction`. Direct spawn-send stays cheap.

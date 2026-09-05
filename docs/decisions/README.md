# Architecture Decision Records (ADR)

This directory contains records of the major architectural decisions made for the `flywheel` project.

| ID | Title | Status |
| :--- | :--- | :--- |
| [0001](0001-event-driven-agent-collaboration-spine.md) | Event-Driven Agent Collaboration Spine (herdr events + agent-mail parity) | **Accepted** (2026-08-05; loop poll→events, pane↔mail bridge, mail primitives, tiered escalation, composable recipes; review-fix addendum 2026-08-05) |
| [0002](0002-pluggable-backend-abstraction.md) | Pluggable Backend Abstraction (MultiplexerBackend + MailBackend) | **Accepted** |
| [0003](0003-reintroduce-agent-mail-backend.md) | Reintroduce a Real agent-mail Backend (per-backend config, XDG, parity) | **Partially superseded** by toron ADR-0007 se2.3 (MailSurface trait unification) — config split, parity harness, per-backend routing survive |
| [0004](0004-flywheel-web-presence.md) | flywheel Web Presence — Orchestration-Layer Presentation | **Accepted** (2026-08-13; flywheel = section of toron.dev per toron ADR-0013 D5; layer-ownership table, Orchestration pillar, 3 diagrams E/W/CO, docs sections, spin-out path to flywheel.dev) |
| [0005](0005-always-on-collab-baseline-and-command-surface.md) | Always-On Collaboration Baseline + Composable Markdown Command Surface | **Accepted** (2026-08-20; D5 captain ±loop; §9 SC-D1–D10; `verify-0005.sh`) |
| [0006](0006-parameterized-stage-payloads-and-wave-scoped-dispatch.md) | Parameterized Stage Payloads + Wave-Scoped Dispatch (the unattended wave swarm) | **Accepted** (2026-08-22; D1–D4; `verify-0006.sh` 9/9; graduation GREEN 2026-08-23 — composed wave-swarm closed fixture bead grad-fw-43424-9da via legal close, kept to catalog) |
| [0007](0007-catalog-slots-and-agent-native-cli.md) | Catalog slots + agent-native catalog CLI | **Accepted** (2026-09-05; D1 slots=harness+model, D2 six job profiles, D3 compiled-in squads, D4 catalog CLI+TUI; Wave 7 catalog-only; `verify-0007.sh` 14/14) |
| [0008](0008-workflow-compose-topology-agent-optimal-mental-model.md) | Workflow / Compose / Topology — Agent-Optimal Mental Model | **Accepted** (2026-09-05; GS-024; design set wave-swarm\|repair-until-verified\|beads-lifecycle; live `is_compose_token` two tokens only; beads-lifecycle implement pending; `verify-0008.sh`) |

## Related toron ADRs

flywheel is tightly coupled to toron (toron carries; flywheel drives). Key cross-references:

- **toron ADR-0003** — the Nostr mailbox fabric flywheel consumes
- **toron ADR-0007 se2.3** — the unified `MailSurface` trait both backends implement (supersedes flywheel ADR-0003's `MailBackend`)
- **toron ADR-0013** — toron.dev website architecture; flywheel is a section (D5). *Moved 2026-08-14 to the [toron.dev repo](https://github.com/bangedorrunt/toron.dev), renumbered ADR-0001*
- **toron ADR-0015** — flywheel ↔ toron integration contract (one mail surface, one-way dep)
- **toron [ADR-0019](../../toron/docs/decisions/0019-bead-verified-execution-and-principle-playbooks.md)** — bead-verified execution, baked poteto principles, playbook workflows (proposed 2026-08-15). Wave 1 lives in flywheel (`ledger.rs`, loop reopen, briefs). Wave 4 is this repo's ADR-0001 A2 leftover.
- **toron [ADR-0020](../../toron/docs/decisions/0020-nostr-two-plane-cli-and-crew-steering.md)** — CLI-first Nostr rooms/jobs/presence/watch so crews steer without flywheel `send`. No new flywheel subcommands in wave 1.
- **toron [ADR-0024](../../toron/docs/decisions/0024-graph-first-repair-loops-bead-workflow.md)** — graph-canonical workflow engine, typed runtime failure, unrolled repair (Accepted in toron; VerifySink is the live compose/graph door — status corrected 2026-09-04, GS-016).
- **flywheel [ADR-0008](0008-workflow-compose-topology-agent-optimal-mental-model.md)** — workflow/compose/topology agent-optimal mental model (Accepted 2026-09-05). Does not amend toron ADR-0027. Live `is_compose_token` = wave-swarm | repair-until-verified; beads-lifecycle design Accepted, implement pending.
- **toron [ADR-0025](../../toron/docs/decisions/0025-adopt-graph-dispatch-artifact-reduce-pin-supervision.md)** — overlay: lease reclaim, typed artifacts, pin supervision, plane routing table, ingress + launch idempotency. Extends this repo's ADR-0001 `LoopAction`. Direct spawn-send stays cheap.

<!-- governed-by: ADR-0010, ADR-0012, skill effect-oriented -->
---
status: accepted
date: 2026-09-19
decision-makers: Captain (bangedorrunt) — Accepted 2026-09-19 (explicit flip order; see §9)
consulted: effect-oriented skill (principles, rust.md, patterns.md); bd-f7oz audit (S1–S4, G1–G7, P1–P7); bd-ao2n audit (toron gaps 1–7)
informed: flywheel × toron maintainers
---

# ADR-0015: Effect-oriented discipline across the stack (Accepted)

| Field | Value |
|-------|-------|
| **Status** | **Accepted** — 2026-09-19 by explicit captain flip order (see §9). Implemented by bd-wn3m (P1–P7) + toron companion ADR-0034/G-lanes (bd-lzo6); fresh review bd-xdgv verdict NO BLOCKERS (1 unwired tracked as follow-up). |
| **Decision-makers** | Captain (bangedorrunt) |
| **Extends** | [ADR-0010](0010-delivery-semantics-attention-triage-decision-lifecycle.md) (honesty), [ADR-0012](0012-doorbell-plane-independent-runtime.md) (plane ownership), [ADR-0014](0014-jev-system-one-judgment-adapter.md) (judgment envelope: journal + escalate) |
| **Does not amend** | Tick Law purity; sticky-open topology; beads sole close; `decide` table leaf; MCP freeze removal (toron ADR-0033 owns it) |
| **Governs** (on acceptance) | Retry/timeout/failure/state/config/test discipline for flywheel; companion toron ADR for toron-side gaps |
| **Tracker** | bd-n86n lineage; toron gaps owned by toron companion ADR (crew B authors) |
| **Verification** | Full bin green, clippy gates green, hermetic default suite, per-surface tests per §7 |

**Decision in one sentence:** Adopt effect-oriented discipline — deferred effects, typed failures, injected authority, edge-attached resilience, substitution tests — as the house style, landing the audited proposals P1–P7 in flywheel now and the toron gaps under a companion ADR, while Tick Law, the journal, and beads authority stay exactly as they are.

---

## 1. Why this ADR exists

Two audits (bd-f7oz flywheel, bd-ao2n toron+beads) through the effect-oriented skill found the same shape everywhere: correct core discipline (Tick Law purity, Enforced/Recording journal, idempotency keys) with handwritten edges — fixed-sleep retries, unnamed 429s, ambient env reads, panics in catalog paths, sleeps-as-sync, heavy suites. Each edge is small; together they are the flakiness budget. This ADR converts the audit proposals into build order.

---

## 2. Principles (normative, stack-wide)

1. **Effects defer; edges execute.** Construction performs no I/O. Retries, timeouts, and fallbacks attach outside the operation, never inside it.
2. **Failures are typed.** New fallible paths return error enums; `panic!`/`unreachable!` never guards input-shaped data (malformed YAML, unknown kinds, bad responses). Existing panics migrate when touched.
3. **Authority injects.** No `env::var`, `current_dir`, `current_exe`, or wall-clock reads at call sites. Resolve once at `main()`, thread a struct down. Tests construct it directly; env-mutating tests are banned for new code.
4. **Waits are bounded and named.** Every remote call has a cap, a backoff, and a jitter, and only retries a typed retryable failure. Unbounded subprocesses (git, child judgment) get explicit timeouts. The one-line wrap order that used to sit in this bullet is retired: flywheel ADR-0016 §5 is the stack (fallback outside, breaker inside the retry, budget timeout around the chain, attempt timeout on the call). ADR-0017 is the shared crate that may later hold that stack. P2's "half-open probe" is not what `ConsecutiveBreaker` does today. That type resets to closed and clears the failure count when the cooldown elapses.
5. **Shared state is keyed, not hoped.** Concurrent claims converge on atomic store operations (UNIQUE + upsert, effect-key replay, epoch fencing), never on sleep windows.
6. **Tests substitute.** Clock, randomness, stores, and remotes are faked; failure sequences scripted (fail-twice-then-succeed, storm, hang). Default suite hermetic and fast; live tests flagged with commands.

---

## 3. Flywheel work (crew A implements all)

| ID | Change | Kills |
|----|--------|-------|
| P4 | `JevEnv`-style injected authority struct resolved once at `main()`; env readers take it; delete env mutation in tests | G4 |
| P1 | Shared `RetryPolicy` + `retry_with_backoff` (cap, exponential + jitter, classified-retryable predicate); migrate settle/dispatch/boot-prime loops | G1 |
| P2 | `JevError::RateLimited` on HTTP 429; host-held consecutive-failure breaker (threshold 5, cooldown 60s, half-open probe); open = fast escalate + audit | G2 |
| P3 | Pack cache keyed by (dir, id) with mtime invalidation | G3 |
| P5 | `CatalogError` enum (thiserror); lint/pick arms return Err; entrypoint maps to LoudMiss + nonzero exit | G5 |
| P6 | Poll-based rename probe (2s cap, 50ms step); keep documented 30s settle | G6 |
| P7 | Per-test time bounds where the harness allows; sleep-heavy tests behind a documented live filter; default suite <30s | G7 |

Order: P4 → P1 → P2 → P3 → P5 → P6 → P7 (each lands independently).

---

## 4. Toron work (companion ADR + crew B implements all)

Flywheel ADRs do not govern toron. Crew B authors the toron companion ADR first, then implements: typed `MailboxResult` edge (no signature churn per site), bounded git subprocess in `br close` path (reuse `vcs.rs` validation), atomic slot acquire (UNIQUE + upsert), `{code, retryable, hint}` MCP error envelope, shared `policy::{Timeout, RetryPolicy}` type, store-watch convergence for job claims, JSONL generation guard. Toron ADR owns kind/tool-count updates.

---

## 5. Explicitly not changing

Tick Law shape; `workspace::tick` purity; sticky-open; beads close ceremony; `decide` table; judgment envelope (ADR-0014 + bd-n86n); MCP parity names/shapes (toron ADR-0033 discipline).

---

## 6. Consequences

Positive: outage behavior becomes boring (capped, backed off, fast-failed); tests deterministic and fast; new surfaces inherit the discipline via skill + review. Costs: touch across retry sites (mechanical), breaker tuning debt, suite split maintenance. Risk: over-wrapping (breaker on every call) — reviewers enforce "remote edges only".

---

## 7. Acceptance

Met 2026-09-19: full bin green (1874 + 1885 runs), clippy gates green, P1–P7 + toron gaps landed with per-surface tests (judgment 50, toron-core 666), default suite hermetic. Fresh review bd-xdgv: NO BLOCKERS (G5 wiring tracked as follow-up bd-8pjr; 3 lows + 3 infos). Captain flip order 2026-09-19: "fix the catches, then fresh review adr implementation for any defect, bug, regression, unwired... when done, flip adr, then push commit."

---

## 8. Sources

- `~/.agents/skills/effect-oriented/{SKILL.md,references/*.md}`
- bd-f7oz findings (channel test-channel 9 chunks) + docs/learnings/effect-audit-flywheel-bd-f7oz.md
- bd-ao2n findings (channel test-channel 7 chunks)

---

## 9. Operator authorization record

P7 verbatim operator order 2026-09-19: "disband your current fleet. with all insight, create new adr, then steer 2 new crews to implement all." Implementation proceeds alongside Proposed status per that order; acceptance per §7 still required.

Acceptance record 2026-09-19: operator order "fix the catches, then fresh review adr implementation for any defect, bug, regression, unwired... when done, flip adr, then push commit." Catches fixed (bd-l4j3; bd-xdr0/bd-m0go verified green), fresh review bd-xdgv NO BLOCKERS, ADR flipped + pushed by this commit.

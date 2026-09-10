---
track: knowledge
module: src/doorbell.rs
problem_type: wake_analytics
tags: [doorbell, wake, analytics, budgets, rate-limit]
bead: bd-asr2.19
created: 2026-09-10
tier: aging
reinforced: 2026-09-10
---

# Wake analytics, first cut (ADR-0012 P4, 2026-09-10 build session)

Per-pin responsiveness and per-channel noise from the live proofs run while
landing bd-asr2.14–bd-asr2.18. Counters are session-observed, not sampled
estimates. Fed through the ce-compound gate per section 10 idea 12.

## Counters

| event | count | source |
|---|---|---|
| tokened emit attempts, admitted | 2 | live serve probe (both 502: no `ci` key locally — auth, budget, policy all passed) |
| tokened emit denied, bad token | 1 | 401 live |
| tokened emit denied, unknown service | 1 | 401 live (revocation = deletion, same path) |
| tokened emit denied, scope | 1 | 403 live |
| tokened emit denied, rate | 1 | 429 live (budget 2/min, third call inside window) |
| arrival-triple role binds | 4 | live digest before/after (4 pins, roster rows) |
| beads-poller read-only proofs | 2 | mtime+md5 stable across poll cycles, live ledger |
| cron sink flag parses | 1 | `config list` with flag on |
| cross-ws widen + dedupe + refusals | 4 | unit (real store paths) |
| external re-ring resolve + wake | 1 | unit (stub runner, real pending queue) |
| digest renders vs swarm size | 3 | zero-swarm digests all rendered (arrival, cron, emit sessions) |

## Per-pin responsiveness

No deaf pins observed this session: every bound pin (4/4) rendered in the
next digest after its wake. The 120s unanswered-pane rule never fired; the
only 502s were missing-key wire failures, not deafness.

## Per-channel noise

Noisiest channel was the emit probe itself (6 calls in <1s, 4 denies) — all
denies were the test matrix working as designed, not abuse. Revoked/unknown
tokens are indistinguishable from wrong tokens by status (both 401), which is
correct: callers cannot probe which service names exist.

## Proposals

- No quiet hours needed yet: deny rate is test traffic. Revisit when a real
  service identity rings >10/min sustained.
- No budget retunes: per-service `rate_per_min` default 6 was never the
  binding constraint in practice (probe used 2). Keep 6 until a service asks.
- Next signal to watch: 502 rate on the emit path — each 502 is an admitted
  ring that never reached a human (missing key). If services onboard without
  provisioning keys, 502s become silent drops. Alert when 502s exceed rings
  delivered over any 24h window.

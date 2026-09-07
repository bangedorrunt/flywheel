# Learnings corpus (ADR-0009)

One learning per file, git-tracked, `rg`-able. This is the compound-engineering **return
arrow**: `ce-brainstorm`/`wave-gate`/the brief's LEARNINGS field read it before work; the
`ce-compound` gate writes to it after verified work; `ce-sweep` maintains it.

## The gate (all three must hold — completion alone never qualifies)

1. **Non-obvious** — the reasoning is not recoverable from the final code, tests, types, or
   comments.
2. **Durable** — an invariant, root cause, rejected approach, or convention useful beyond the
   exact diff.
3. **Material** — forgetting it would plausibly cause recurrence or substantial rediscovery.

Counterfactual: if the doc vanished, would the next crew redo this investigation? If not,
`clean_skip`.

## Schema

```markdown
---
track: bug            # bug | knowledge — decides the sections
module: src/crew.rs   # open vocabulary, reuse what the corpus already uses
problem_type: integration_issue   # closed-ish: bug track uses these, knowledge track free
tags: [dispatch, boot-race]
bead: bd-wave-swarm-graduation-fix-98io   # provenance
created: 2026-09-06
tier: aging           # ADR-0010 D12: pinned | aging | perishable (default aging)
reinforced: 2026-09-06  # YYYY-MM-DD — last re-verification; gates the GATE projection
expires: 2026-10-01   # REQUIRED for tier: perishable — a checkable expiry
---
## Symptoms            # bug track: Symptoms / What Didn't Work / Solution /
## What didn't work    #         Why This Works / Prevention
## Solution
## Prevention
# knowledge track instead: Context / Guidance / Why This Matters / When to Apply
```

- High overlap with an existing entry ⇒ UPDATE it (stamp `last_updated` + re-stamp `reinforced` —
  ADR-0010 D12 reinforcement), never duplicate.
- Decay (ADR-0010 D12): `tier: pinned` never decays; `aging` leaves the GATE projection after
  30d unreinforced; `perishable` after 7d AND must name a checkable `expires:`. Leave the corpus
  is ce-sweep's ARCHIVE (`git mv docs/learnings/archive/`), never delete. `flywheel
  __gate-learnings` is the bounded GATE projection (top-N by reinforcement recency,
  `FLYWHEEL_GATE_BUDGET` ~4k tokens).
- A learning contradicting standing-orders/AGENTS.md guidance **outranks ordinary staleness** —
  resolve by which side current code follows; report a wrong guidance file, never edit it here
  (`ce-sweep` owns that).
- The learning ships in the SAME commit as the work that produced it.

## Maintenance

`flywheel workflow run ce-sweep --execute --inputs '{"project":"flywheel"}'` —
Keep / Update / Consolidate / Replace / Delete, evidence per verdict.

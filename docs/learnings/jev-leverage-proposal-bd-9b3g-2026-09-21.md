# Proposal: flywheel leverage from jev-mcp + toron surface (bd-9b3g)

Catalog-reliable, NL-coverage-poor: the verbs exist, operator intents don't reach them.

## 1 → Jev scores, the table decides

Route `decide` candidates through `jev_rank` (or atomic `jev_evaluate`
Choice packs). The routing table stays the authority; Jev is the scorer
behind deterministic floors (`max(0.8, auto_accept)`, concentration guard).
Never auto-dispatch on mock mode; thresholds tuned per repo.

## 2 → Jev gates the ship chain, advisory only

`jev_step` for steer continue/retry/stop using execution facts flywheel
already tracks; `jev_gate`/`jev_verify` as second opinion beside
`ce-evaluate` on phase-contract evidence. Host verdict always wins;
`coverage.complete=false` can never return `auto`.

## 3 → Close the NL gap on toron's CLI-only verbs

`wait --corr`, `receipt`, `role`, `ack-sla`, `channel/job/presence/watch
--live` are frozen CLI-only, so MCP-only agents and NL intents ("wait for
the ack", "who owns this path") have no path. Expose them as MCP tools or
first-class NL intents with the same fail-closed shapes (typed errors,
never fabricated judgments). Also fix the silent no-ops
(`ack_overdue_only`, `unread_only`) or delete them; silent flags are the
exact failure this stack claims to forbid.

## Non-goals

No Jev in the hot dispatch path until calibrated per repo; no exactly-once
claims (mailbox receipts are lifecycle, ADR-0032 journal still proposed);
no second network backend (jev-mcp omits fallback on purpose).

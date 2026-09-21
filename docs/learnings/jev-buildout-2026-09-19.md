# Learnings: Jev buildout steer (bd-n86n, bd-lpn6, bd-ncpt; 2026-09-19)

Two pi crews, all closes LEGAL, all gates green.

## What landed

- Crew A: enable-readiness (POST outside commit tx, latency in audit),
  miss ledger + resolution/needles queries, doorbell screen battery,
  ce-evaluate prescreen, coordination annotators, `judgment` CLI,
  judgment-packs guide. Full bin 1872/0, clippy gates green.
- Crew B: toron ADR-0033 (additive tools/kinds discipline replacing the
  freeze), `ask_judgment` MCP tool (41 total) via out-of-process
  `flywheel judgment-ask`, catalog regen, tool-authoring guide.
  Judgment 9 + guard 55 + mcp probe green.
- Captain applied all global/skill text (AGENTS.md, toron/flywheel/br
  skills, mcp-surface reference). Live daemon still serves 38;
  rebuild+restart is an ops step, not a code gap.

## Corrections worth keeping

1. **Chie is not the steer ledger.** Memory records for session tracking
   are wrong-tool usage (operator call). Beads + toron mail track the
   work; chie keeps durable lessons only.
2. **My cron diagnosis was wrong** (--confirm retired). Crew found the true
   root: XDG_STATE_HOME hermeticity. State the evidence, hold the theory
   loosely; crews correct loudly when briefed to.
3. **Guard forensics ladder** (now in skills): conflicts + list --path
   before source diving. The glob footgun (`**/` misses top-level) is
   encoded in AGENTS.md, both skills, and a toron acquire warning.
4. **Short-sha close records** (bd-ncpt: 7-char sha, empty reason) pass the
   tool but weaken the ledger trail. Gate notes carried full SHAs; still,
   brief crews to close with full sha + reason always.
5. **Untracked mid-session installs need provenance.** typesafe-ai +
   mcp-builder appeared from vendor sources; content good, author unclear
   until operator confirmed. Ask early, not at wrap-up.

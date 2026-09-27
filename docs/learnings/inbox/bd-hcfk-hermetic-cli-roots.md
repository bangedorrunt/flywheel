---
tier: perishable
status: inbox
created: 2026-09-27
expires: 2026-10-04
reinforced: 2026-09-27
provenance:
  bead: bd-hcfk
  commit: c9bba8483d300c352a5222f6fcb8d4117f338095
  lanes:
    corpus: rg docs/learnings/ — hermetic named only in passing (effect-audit gap list, jev-buildout XDG_STATE_HOME anecdote); no file states the triple-root rule
    wiki: chie qmd search low-relevance (docker noise; generic doorbell ADRs, no prompt-hop/pane-id page)
    memory: chie memory recall — stale 2026-09-07 doorbell-topology messages only, no bd-hcfk/bd-523f conclusions
  memory_record: e-e76cb0dca1cb76515c16
  receipt: .flywheel/receipts/bd-hcfk-c9bba8483d300c352a5222f6fcb8d4117f338095.unit-test-verified.txt
---

# Hermetic roots for CLI-spawn tests (bd-hcfk)

A control-flow test that spawns the real binary inherits the operator's
ambient config, so a user-tier `bug-fix.yaml` shadows the compiled-in
builtin and decides the verdict. Observed: `sc_bugfix_playbook` was the
last failing workspace target purely because the user tier held 40
corrupted copies, green again with zero code change after repair.

Rule: redirect all three ambient roots the binary reads at a temp tree —
`XDG_CONFIG_HOME` (catalog tier), `XDG_STATE_HOME` (loops/leases), `HOME`
(legacy macOS config dual-read plus every `dirs::home_dir()` fallback) —
via `HermeticRoots`, and pin the property with a test that poisons the
ambient tier and requires the identical argv to still exit 0.

Check: `XDG_CONFIG_HOME=<corrupted tree>` full `sc_bugfix_playbook`
suite green (4 passed, 0 failed) plus whole-workspace suite green;
RED observed pre-fix naming the poisoned ambient path.

A suite whose verdict flips on operator-global config is a test defect
independent of any bug it was written to catch.

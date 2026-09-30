# Read-only reliability audit: disposition (bd-9u53, 2026-09-29)

The audit ran 2026-09-28/29 and never produced an artifact. This is its
disposition, written at close so the ledger has something true to bind,
not a fresh audit wearing the old bead's name.

## What the audit was for

Trace workflow and daemon paths across flywheel, toron and beads; review
supervisor, peer, coalition, socket/API, wiring and performance risk;
report evidence and live-test prerequisites. No source edits.

## What it found, and where each finding landed

The findings that mattered were coalition and daemon outcome honesty:
formation must not activate on a failed, suspended or unknown workflow
result, and daemon request parsing and handler concurrency must stay
bounded and protocol-exact. Those are implemented under **bd-ivkr**,
which was taken over, re-verified today (doorbell-cli 103 green,
coalition 19 green) and closed LEGAL at `ffe0627`.

The reliability work that the audit's concern about delivery and
durability anticipated also landed separately, each with its own
verified close: bd-jqjh (silent drops counted, not swallowed), bd-68q2
(wake socket exhaustion distinguished from absence), bd-8nyh and bd-8x2o
(delivery cursors persisted across a loop exit), bd-slkn (SIGTERM saves
instead of killing), bd-pi4p (bindings read citations, not prose).

## Live-test prerequisites, which the audit asked for and which still hold

Everything claimable offline was claimed offline. What could NOT be
claimed without a real two-project environment, and is named here rather
than assumed: cross-project mail delivery by npub (bd-j5sk now refuses
an ambiguous name instead of sending into the void, but end-to-end
delivery under two live daemons is still unproven), and CI, which is dark
for a billing reason, not a code one (bd-whie).

## Status of the full suite at close

2092 bin tests green, `bash scripts/verify-clippy.sh` exit 0, and one
workspace test that needs a built toron CLI in the sibling repo, which is
environmental and pre-existing.

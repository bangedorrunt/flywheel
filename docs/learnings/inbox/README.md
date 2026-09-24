# docs/learnings/inbox/ — compound auto-promote inbox (bd-iqte)

Unreviewed learnings land HERE, never straight in `docs/learnings/`.
The human gate sits at **sweep time**, not write time: `ce-compound`
files and moves on; `ce-sweep` promotes or archives.

## Contract (every file in this dir)

- Frontmatter: `tier: perishable` + a checkable `expires:` date
  (default: 7 days after `created:`), plus `reinforced: <today>`.
- `provenance:` names the bead, the commit SHA, the read lanes consulted
  (`corpus` rg hits / `wiki` qmd hits / `memory` recall hits, or `none`),
  and whether `chie memory record` captured it.
- `status: inbox` until sweep triages it. Sweep verdicts:
  **promote** (move to `docs/learnings/`, re-tier aging/pinned,
  re-stamp `reinforced:`), or **archive** (decay, never delete).

Perishable carries the decay (encode-lessons-in-structure): an inbox
entry no one reinforces archives itself out of relevance on expiry.

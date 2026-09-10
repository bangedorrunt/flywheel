# ADR-0010 implementation status — re-study 2026-09-11

Vehicle: open (current pane), pin QuietAnvil. Re-study prompted by a coarse
sweep (2026-09-11) that classified ADR-0010 "entire ADR unimplemented" from its
unchecked §13 checkboxes. That verdict was wrong. Careful pass: read the full
body, pulled the bead graph (`_reviews/0010-bead-graph.md` +
`0010-bead-ids.json`), checked all 16 implementation beads, and probed each
surface in the tree/CLI today.

## Verdict

**ADR-0010 is implemented.** All sixteen decision beads (D0–D15) are closed,
and every major surface was re-verified live in the tree on 2026-09-11. What
remains is the ADR's own acceptance bookkeeping, not features:

1. **§13 wave-verify harnesses were never authored.** No
   `verify-0010a0.sh` / `verify-0010a.sh` / `b` / `c` / `d` exist in
   `docs/decisions/_scripts/` (verify-0012.sh exists as the pattern). All five
   wave checkboxes have been unchecked since acceptance (2026-09-07) even
   though the waves landed. Remaining obligation: author the five gates (or one
   sectioned script), run them, flip the boxes.
2. **D11 tail — beads ADR-0005 acceptance is unreconciled.**
   `~/workspace/beads/docs/decisions/0005-scout-ship-deliverable-and-captain-hold.md`
   carries frontmatter `status: accepted` but body "**Status:** Proposed
   (2026-09-06). Acceptance by the Captain of the beads repo is a separate
   act." The `--deliverable` / `--promotes` surfaces are live in `br` today;
   the acceptance act (and which status line is true) needs the beads
   captain's ruling.
3. **toron F1/F2/F5/F6 landed in source but were never filed as beads.**
   ADR-0010 §12 promised code beads labeled `adr-0010` + `live-use` in the
   toron tracker; none exist (title + label searches empty). The code is in:
   F1 captain auto-bind `crates/toron-core/src/mailbox/identity.rs:201,389`
   (test at :2387), F2 `last_active_ts_us` + recency predicate
   `identity.rs:14,104,121`, F5 ack shape `crates/toron-mcp/src/tools/mod.rs:262`,
   F6 typed contact-request rendering
   `crates/toron-core/src/mailbox/messaging.rs:3591`. Ledger gap only — file
   retro-labeled beads or amend §12.
4. **D13's hook fence is unverifiable from the repo by design.** The tracked
   surface is the prompt standing orders (`integrations/prompts/{open,loop,swarm}.md.tpl`);
   the PreToolUse deny is per-home untracked. One-time operator check that the
   deny is actually installed on the captain/steer primaries (pi/grok/opencode).

Skill Sync Contract (§12) is honored: flywheel skill §5 and
`flywheel --robot-capabilities` carry `helm`, `snapshot`, `doorbell afk`,
`crew control`; `--fire-and-forget` is on `send --help`.

## Per-decision evidence (all beads closed 2026-09-11)

| Dn | Bead | Tree/CLI evidence |
|----|------|-------------------|
| D0 | bd-aucr | `poll_mail_digests` producer deleted; only the guard assert remains (`src/durable.rs:11149-11152`) |
| D1 | bd-w98c | `--fire-and-forget` on `send --help`; ladder knobs in `workspace.rs`/`main.rs` |
| D2 | bd-8jlv | `WakeAck` ×8 in `src/workspace.rs` |
| D3 | bd-s8bo | verdict machinery in `workspace.rs`; presence trust in skill |
| D4 | bd-8j7m | `src/write_liveness.rs` exists; `FLYWHEEL_WRITE_WALK_*` knobs |
| D5 | bd-65he | `flywheel doorbell afk` live ("ADR-0010 D5" in help); `FLYWHEEL_DIGEST_SECS` |
| D6 | bd-f14d | `flywheel helm` prints OPEN DECISIONS / UNREAD STATUS / RECORD DIVERGENCE (observed live in session arrival output) |
| D7 | bd-w5fn | `flywheel crew control` live, verbs-only surface ("ADR-0010 D7" in help) |
| D8 | bd-d8e4 | `degraded` ×9 in toron `receipts.rs` |
| D9 | bd-l0fz | toron `send_tags.rs` `--resolve-bead` + `bead=` tag (":264", ":362") |
| D10 | bd-67qg | `src/snapshot.rs` exists; `flywheel snapshot` prints `flywheel-snapshot.v1`; `helm` live |
| D11 | bd-jtnn | beads ADR-0005 authored; `br create --deliverable/--promotes` live — acceptance pending (see verdict §2) |
| D12 | bd-fmya | `FLYWHEEL_GATE_BUDGET` in `src/stages.rs` |
| D13 | bd-eo39 | prompt standing orders tracked; hook untracked per-home (verdict §4) |
| D14 | bd-qpub | `FLYWHEEL_GUARD_GRACE_SECS` in `main.rs` |
| D15 | bd-tl65 | policy folded into stage templates/peer-aware tpl |

Method note for the next reader: the `adr-0010` label search returns empty in
the flywheel tracker — the Dn beads carry no such label; the authoritative map
is `_reviews/0010-bead-ids.json`.

// governed-by: ADR-0008, ADR-0010

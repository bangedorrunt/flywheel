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
remained was the ADR's own acceptance bookkeeping — see the update section at
the end for what this study then closed.

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
| D11 | bd-jtnn | beads ADR-0005 authored; `br create --deliverable/--promotes` live — acceptance pending (see below) |
| D12 | bd-fmya | `FLYWHEEL_GATE_BUDGET` in `src/stages.rs` |
| D13 | bd-eo39 | prompt standing orders tracked; hook untracked per-home (see below) |
| D14 | bd-qpub | `FLYWHEEL_GUARD_GRACE_SECS` in `main.rs` |
| D15 | bd-tl65 | policy folded into stage templates/peer-aware tpl |

Method note for the next reader: the `adr-0010` label search returns empty in
the flywheel tracker — the Dn beads carry no such label; the authoritative map
is `_reviews/0010-bead-ids.json`.

## What remained (found 2026-09-11)

1. §13 wave-verify harnesses never authored — no `verify-0010a0/a/b/c/d.sh`.
2. D11 tail — beads ADR-0005 acceptance unreconciled: frontmatter `status:
   accepted`, body "**Status:** Proposed (2026-09-06). Acceptance by the
   Captain of the beads repo is a separate act." Surfaces live in `br` today.
3. toron F1/F2/F5/F6 landed in source but never filed as the promised
   `adr-0010`+`live-use` beads (ledger gap): F1 captain auto-bind
   `identity.rs:201,389` (test :2387), F2 `last_active_ts_us`
   `identity.rs:14,104,121`, F5 ack shape `toron-mcp/src/tools/mod.rs:262`,
   F6 typed contact-request rendering `messaging.rs:3591`.
4. D13 hook fence unverifiable from the repo by design (per-home untracked).
5. §13 toron-addenda box: no formal toron ADR addendum records D8/D9 — code
   comments + closed beads only.

Skill Sync Contract (§12) honored: skill §5 + `--robot-capabilities` carry
`helm`, `snapshot`, `doorbell afk`, `crew control`; `--fire-and-forget` on
`send --help`.

## Update — gates authored and green (2026-09-11, later same day)

Item 1 closed: `verify-0010{a0,a,b,c,d}.sh` now exist in
`docs/decisions/_scripts/` (house style per verify-0012.sh; D5's gate runs the
D0 tripwire first) and ALL FIVE run PASS. §13 wave checkboxes + Skill Sync box
flipped; the toron-addenda box stays open (item 5 — operator call whether that
closes or an addendum ADR gets written).

Item 4 sharpened: the D13 hook fence is NOT present in this machine's visible
harness configs — `~/.claude/settings.json` PreToolUse carries only `atuin`
and `dcg` (Bash matcher, not a delegation deny), and the opencode/herdr config
dirs show no deny stems. Only the tracked prompt standing orders exist. The
fence the ADR calls structural is therefore not installed where I can see;
either install it on the captain/steer primaries or record that prompts-only
is the accepted posture.

// governed-by: ADR-0008, ADR-0010

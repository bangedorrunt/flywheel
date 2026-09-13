# Gate-name skew: br's policy list vs flywheel's legal-close matrix (bd-tfxv)

Status: **closed**. Found live on 2026-09-13 while closing `bd-d60u`; fixed 2026-09-14.

## What was reported

Closing `bd-d60u` (P3, blast normal, AC checkable, VERIFY
`bash docs/decisions/_scripts/verify-0012.sh`) recorded a `command-verified`
row, and br refused to authorize the close, naming the legal gates
`unit-test-verified | live-verified | worker-receipt | triage-verified`.

Two things were wrong at once, and only one of them was visible in that
refusal.

## Defect 1: flywheel could not parse two of br's seven gate names

br's `close_policy::ADR0019_VERDICT_GATE_NAMES` enumerates seven names:

```
command-verified  unit-test-verified  live-verified  reviewer-signed
worker-receipt    operator-directed   triage-verified
```

flywheel's `VerdictKind` knew five. `operator-directed` and
`triage-verified` fell through `from_gate_name` -> `None`, and
`ledger_check` treats an unknown gate as "not a verdict row" and skips it.
So a worker who followed br's own refusal message landed a proof the ledger
could never credit — a wasted ceremony in the P2+ non-runnable band, and
**no legal verdict at all** for a P0/P1 ordered close (row 2b exists only
for a named human attester).

Fixed in `1502093`: both kinds added, carrying br's own `legal_close` rows —
`TriageVerified` joins `WorkerReceipt` in the P>=2/Normal/non-runnable band,
`OperatorDirected` is the P0/P1/Normal human-attester row. `VerdictKind::ALL`
was reordered to match br's so future drift is visible side by side, and two
tests pin the agreement: one copies `ADR0019_VERDICT_GATE_NAMES` literally,
one runs `ledger_check`'s body (`from_gate_name` -> `legal_close`) for the
live `bd-d60u` shape.

## Defect 2: the close path applied a dispatch precondition

Making the kind parseable was necessary but not sufficient — the live check
still refused, now with a different message:

```
$ flywheel ledger check bd-tfxv
flywheel: bead bd-tfxv: missing/empty typed verify field — refuse to dispatch
```

`ledger check` loaded the bead with `bead_from_br_show`, whose empty-VERIFY
refusal is a *dispatch* precondition: nothing to run, so do not hand the bead
to a worker. A triage/dupe bead has no VERIFY by definition, and its legal
proof is a record review (`triage-verified`) — exactly the gate br names for
it. The dispatch precondition denied the close before any gate row was read.

Fixed in `abe3acc`: `bead_from_br_show` keeps requiring VERIFY (all dispatch
call sites unchanged) and `bead_from_br_show_for_close` admits an empty one
for the crediting path. Safe by construction — `verify_is_loop_runnable("")`
is false, so the bead lands in the non-runnable band, where the table admits
only record/independent proofs and never `CommandVerified`.

## What was deliberately *not* changed

`estimated_cheap` still demands a literal `timeout` token for `sh`/`bash`, so
a committed bash-script VERIFY stays in the non-runnable band. A committed
script is reviewable but not statically bounded — it may `sleep 3600` — and
the classifier cannot know. Relaxing flywheel here would have created a
*new* skew in the opposite direction (flywheel laxer than br); the
conservative reading stands, and an independent verifier (or a triage
record) is the honest proof for that shape.

## The authority

One authority for the gate-name policy: the ancestor table (ADR-0019 Spec 1,
carried by br). flywheel mirrors it — the descendant gained the two kinds
rather than br learning to name fewer. `VerdictKind::ALL` and br's
`VerdictKind::ALL` now share an element order so any future divergence shows
up as a side-by-side diff, and `every_br_gate_name_parses_to_a_kind` fails
the moment br names a gate flywheel cannot adjudicate.

## Evidence

| Check | Result |
|---|---|
| `cargo test --bin flywheel verdict` | 56 pass, 0 fail |
| `bash scripts/verify-clippy.sh` | fmt + clippy `-D warnings` + check clean |
| `flywheel ledger check bd-tfxv` (live) | `LEGAL (sha abe3acc6…)`, exit 0 |
| before the fix | row unparseable (`from_gate_name` -> `None`) **and** bead unloadable ("refuse to dispatch") |

# A bead id belongs to exactly one commit

## The rule

A bead's legal-close binding is resolved by **searching commit messages for its
id**. So the newest commit that merely *mentions* `bd-x` becomes `bd-x`'s
binding — and the gate row that proved the close, written against an older sha,
stops matching.

The close then reads as unprovable:

```
ledger check bd-v7uu: no legal verdict bound to sha cb61d05
  gate unit-test-verified: note binds sha fc8461a, not the revision cb61d05
  gate unit-test-verified: note binds sha 62e6b4f, not the revision cb61d05
```

The bead's own `commit_sha` field still said `62e6b4f`, and its status still said
`closed`. Nothing in `br show` was wrong. Only the *check* disagreed — which is the
point of the check, and also why the disagreement is invisible until you run it.

## How it bit

I closed `bd-v7uu` at `62e6b4f`, then cited `bd-v7uu` in three more commits:
`89fab88` (the LoudMiss dedup fix), and `cb61d05` (the wake consumer's retired
flag). Both were genuinely part of the same bead's work, which is exactly why the
citation felt honest. The binding followed the last one and the close went
unbound.

CopperWren hit the same thing independently on `bd-3vr9`, where a later commit
(`e4adaf5`, unrelated work) mentioned the id in its message *prose*. Neither of us
wrote that citation intending to move a binding. Both of us then hand-repaired the
bead, which is the tell: **two hand repairs in one day means the mechanism, not
the instances.**

## Why an honest citation still breaks it

The citation is not the problem — a commit that carries real work for a bead
*should* name it. The problem is that the ledger resolves identity by search
rather than by an explicit record. So the fix is not "stop citing beads"; it is
one of:

- record the binding at close time in a place a later commit cannot overwrite, or
- refuse a close whose proving row binds a sha that is not the newest citation, or
- make the ledger check fall back to *any* proving row for the bead rather than
  demanding the newest.

The first is a beads-repo change. `bd-close-without-gate-row-sfz9` (filed by
CopperWren) is the adjacent defect: a legal close can leave no gate row at all,
which makes this trap unrecoverable-by-accident.

## The repair, when it happens anyway

Reopen, re-gate, re-close **at the resolved sha**:

```
br update <bead> --status open
br update <bead> --status in_progress
br gate report <bead> --gate <name> --provider <p> --status pass --to closed \
  --note "sha=<full sha> receipt=<what proves it>"
br close <bead> --commit-sha <that same sha> --reason "<what changed, and why>"
```

Note the policy detail that cost me a round trip: `closed -> in_progress` is NOT
permitted (valid next status from `closed` is `open` only), and the close sha must
itself cite the bead — so the repair needs a commit that mentions it, which is the
same trap once more. Make that commit carry real content. An empty commit whose
only job is to cite a bead is a green lie, and `ledger check` will call it LEGAL
because the check reads the message, not the tree.

## The general form

Two of tonight's findings are this same shape wearing different clothes:

- a health field that fires on a healthy path (`stale_build` on wall-clock age),
- an audit row that re-fires because its own memory lives in the store it prunes.

In all three, the signal is derived from something adjacent to the fact it claims
to report — an age instead of a staleness, a pruned log instead of a tombstone, a
message search instead of a recorded binding. When a check and the thing it
checks disagree, the check is usually reading the wrong thing.

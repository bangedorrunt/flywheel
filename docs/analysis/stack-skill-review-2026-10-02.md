# Stack skill review: flywheel x toron x chiebukuro x br

Date: 2026-10-02. Reviewer: Buffy. Lens: `~/.agents/skills/skill-judge/`
(D1-D8, 120 points) and `~/.agents/skills/effect-oriented/` (15 principles).
Scope: the six skills that own the stack, judged fresh rather than re-read
from the previous pass.

## What counts as a stack skill

The stack's agent-facing surface is six skills, not the ~80 directories under
`~/.agents/skills/`. The rest are general-purpose or vendored and are out of
scope here.

| Skill | Lines | Canonical home | Guardable in-repo? |
|---|---|---|---|
| flywheel | 375 | `~/.agents/skills/flywheel` | no (same rule as toron) |
| toron | 324 | `~/.agents/skills/toron` | no |
| br | 389 | `~/.agents/skills/br` | partly (`.claude/skills/br` in-repo) |
| chiebukuro | 108 | `~/.agents/skills/chiebukuro` | yes (bd-yr2 added the guard) |
| effect-oriented | 202 | `~/.agents/skills/effect-oriented` | n/a (the lens itself) |
| effect-kernel | 48 | `flywheel/.agents/skills/effect-kernel` | yes (in-repo) |

## Grades

Scores are this pass. Where a grade moved, the reason is a specific edit.

| Skill | D1 | D2 | D3 | D4 | D5 | D6 | D7 | D8 | Total | Grade |
|---|---|---|---|---|---|---|---|---|---|---|
| effect-oriented | 20 | 14 | 15 | 14 | 14 | 14 | 9 | 14 | 114 | A |
| br | 18 | 13 | 14 | 14 | 13 | 13 | 8 | 14 | 107 | B |
| chiebukuro | 17 | 12 | 13 | 14 | 12 | 12 | 9 | 13 | 102 | B |
| flywheel | 17 | 13 | 13 | 14 | 14 | 12 | 8 | 13 | 104 | B |
| toron | 17 | 13 | 13 | 14 | 14 | 12 | 8 | 14 | 107 | B |
| effect-kernel | 16 | 13 | 12 | 13 | 10 | 12 | 8 | 12 | 96 | B |

Every one of the four ops skills moved up this pass. None reached A, and the
reason is the same in all four: D1 knowledge delta is capped by tables of
command shapes that a reader could regenerate from `--help`, while the part
that is genuinely expert (the measured gotchas) is present but not
prioritised.

## Findings

### S1, material: the toron skill stated three different tool counts

`crates/toron-mcp` registers **41** tools: `catalog::PARITY_TOOL_COUNT` is 38
and `catalog::ADDITIVE_TOOLS` holds 3 (`release_file_reservations_by_reason`,
`list_file_reservations`, `ask_judgment`). The skill said:

| Site | Said | Truth |
|---|---|---|
| `SKILL.md:5` (frontmatter) | 41 (38+3) | correct |
| `SKILL.md:48` | 41 (38+3) | correct |
| `references/mcp-surface.md:1` | 40 (38+2) | stale |
| `references/workflow-engine.md:163` | 39 | stale |
| `references/daemon-ops.md:129` | 39 | stale |

Fixed. The two correct sites are why this survived: an agent reading only the
frontmatter got the right number.

### S2, material: `ask_judgment` was undocumented

The third additive tool was absent from `mcp-surface.md` entirely — not in the
header count, not in the additive list, and no cluster row. The cluster table
summed to 40. An agent choosing a tool had no way to learn the tool exists,
which matters because it is the only MCP path to flywheel's judgment math and
the only one that is idempotent on `effect_key`.

Fixed: cluster row added (Judgment, 1, `judgment.rs`), plus a section
documenting the required args, the offline/system-one split, the seeded packs,
the 30s default and 300s ceiling, and the fact that toron returns and journals
but does not publish.

### S3: 16 malformed spots left by the citation strip

Commit `5711f64` removed `ADR-NNNN` and `beads#N` prose ids from the skills.
Where the id was the whole list item, it removed the label and left the husk:

- **9 orphaned items** reading `-: \`docs/decisions/00NN-....md\`` across
  `mcp-surface.md`, `daemon-ops.md`, `workflow-engine.md`.
- **7 dangling `—)` husks**, e.g. `### File reservations (intent vs held —)`
  in `mcp-surface.md` and `client across flywheel + toron —).` in
  `daemon-ops.md:126`.

The orphans were also dead weight: a path into the toron repo is not actionable
from `~/.agents`, which is rsynced to machines that have no checkout. Removed
rather than relabelled. All husks repaired. This is the strip's own regression,
found by looking rather than by a gate.

### S4: the chiebukuro skill pointed at two skills that were not installed

`skills/chiebukuro/SKILL.md` instructs "Load `chiebukuro-gist` before editing
pipeline, bridge, session, daemon, or concurrency code" and "Load
`mcp-server-audit` before editing any MCP surface". Neither was installed, so
both instructions were unfollowable. Its hermes `related_skills` named three
more that do not exist anywhere (`chiebukuro-compiled`, `context-mode`,
`paperclip-search`).

Fixed: both skills installed from the chiebukuro repo (verified byte-identical
by `diff -r`), and `related_skills` repointed at four that exist.

### S5: nothing tied skill prose to the code that defines it

chiebukuro's count rotted while `tool_count_is_24` passed, because the test
guarded the router and not the prose. bd-yr2 closed that for chiebukuro by
reading `skills/chiebukuro/SKILL.md` from an in-repo test.

toron cannot be fixed that way: its `AGENTS.md:149` declares
`~/.agents/skills/toron/SKILL.md` the "single canonical copy" that grok reads
with no mirror. The prose is deliberately outside the repo, so no hermetic
repo test can reach it, and a test that read `~/...` would be the ambient-state
anti-pattern in a new dress.

Fixed with `~/.agents/scripts/check-skill-counts.sh`, which derives each count
from source and asserts the prose states it. It cross-checks the breakdown
against the router independently, and SKIPs loudly when a repo is absent
rather than passing silently.

Two derivations, because the first version was confidently wrong: counting
`name = "` within six lines of `#[tool(` reported **24** knowledge tools
against a true 17 (a wrapped description spills into the next block), and the
`ADDITIVE_TOOLS` extraction counted its own declaration line and closing `];`,
reporting **5** additives against a true 3. The attribute header
`^    #\[tool($` is exact. Both bugs were found by running it, not by reading
it.

### S6: the prior effect-oriented review understated the stack

`effect-oriented-stack-review-2026-10-01.md` still lists F1/T1/T2/C1 as open.
All four are closed:

| Finding | Status | Evidence |
|---|---|---|
| F1 flywheel `swarm_loop` authority | closed | `durable.rs:8433` resolves once at the host edge; the guard at ~12380 asserts the body takes `ctx.get_dependency` and contains neither `resolve()` nor a clock read, building its forbidden strings with `concat()` so an adjacent comment cannot satisfy the scan |
| T1 toron `run_parallel` cancellation | closed | documented fail-after-collect (`control_flow.rs:1607`, `:1664`): every copy is a journaled durable child, so a sibling failure must not cancel it |
| T2 toron ADR-0034 status | closed | frontmatter now `status: accepted` |
| C1 chiebukuro bridge policy | closed | `RetryStep` + `jittered_delay` + `step_after_failure` in `src/bridge/llm.rs` |

The flywheel audit verdict stays **conditional** and that is still accurate:
`swarm_loop` is migrated, but other durable leaves still sample wall time, so
strict end-to-end authority injection is not claimed. The review doc now says
so instead of pointing at a closed bead.

## Effect-oriented status of the implementation

The paradigm's durable half is in good shape across all three services, and the
one structural weakness repeats in exactly two places: **policy as constants at
the edge instead of values handed down**.

- flywheel: migrated `swarm_loop` to host-injected authority. Remaining work is
  the other durable leaves, which the audit states rather than hides.
- toron: `policy.rs` is the counterexample worth copying — `Timeout` and
  `RetryPolicy` are values, `delay_for`/`should_retry` are pure, and the retry
  helpers are thin executors.
- chiebukuro: the bridge now takes its retry boundary as a value; the same
  shape would apply to its 300s/10s timeouts.

Journal discipline is the strongest repeated pattern in the stack: every honesty
claim is a named state plus a test (unknown tables, replay guards, lease
counters), never prose.

## What I did not do

- **toron `docs/guides/*.mdx`** still carry ADR and bead citations (~50 across
  the guides). Left out of scope as user-facing docs rather than agent
  instructions, as decided earlier. Worth a pass if the site publishes them.
- **The 13 `nostr-*` skills** in the toron repo are reference material, not
  stack ops. Not judged.
- **A hermetic in-repo guard for toron** is architecturally unavailable while
  the single-canonical-copy rule stands. The operator fence is the honest
  substitute; if toron ever wants the strong version, the skill has to move
  into the repo and the grok mirror rule has to change with it.
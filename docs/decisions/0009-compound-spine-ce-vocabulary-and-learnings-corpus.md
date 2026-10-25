# ADR-0009 — Compound Spine: ce- Vocabulary, the Compounded wave-swarm, and the Learnings Corpus

| Field | Value |
|-------|-------|
| **Status** | Accepted (2026-09-06; captain, after the EveryInc/compound-engineering-plugin study session). **Amended 2026-10-25:** D6 "loop-mode" is the bead command under open mode, not a third mode. Swarm stays the crew mode. |
| **Date** | 2026-09-06 |
| **Decision Maker** | Captain (bangedorrunt) |
| **Relates to** | [ADR-0006](0006-parameterized-stage-payloads-and-wave-scoped-dispatch.md) (stage payloads + wave swarm), [ADR-0008](0008-workflow-compose-topology-agent-optimal-mental-model.md) (compose tokens), toron [ADR-0022](../../toron/docs/decisions/0022-phase-native-orchestration.md) §3.1/§4 (phase vocabulary — amended by this ADR's addendum there), toron ADR-0027 (one-workspace tick), beads ADR-0001 (typed fields), [EveryInc/compound-engineering-plugin](https://github.com/EveryInc/compound-engineering-plugin) (MIT — concepts adopted, no code) |
| **Governs** | `src/stages.rs` (stage library, PHASES, alias retirement), `src/builtin_workflows.rs` (built-in renames, wave-swarm composition, ce-compound/ce-sweep), `src/crew.rs` + `src/decide.rs` (compose/routing), `src/main.rs` (robot capabilities, help), `docs/learnings/` (project corpus), `integrations/beads-body.md.tpl`, standing orders |
| **Verification** | `docs/decisions/_scripts/verify-0009.sh` (see §8) |

---

## 1. TL;DR

A study of Every's compound-engineering plugin (33 skills) against our stack found we already
have CE's *discipline* half (brief contracts, gates, bounded repair, receipts, captain signoff —
stronger than CE in durable mail + the beads two-step close) but lack its *compounding* half:
the **learning return arrow** (nothing reads past lessons at dispatch; swarm lessons were
hand-curated into AGENTS.md/skills days later) and a **true adversarial review** (wave-VERIFY
audits receipts and close-reason tails, never the artifact — the hole the torondev
"garbage stub catalog claimed as real" lane fell through).

**Decision — six moves, all landed in one change:**

1. **D1 — ce- stage vocabulary.** The compound-engineering loop stages carry the `ce-` prefix:
   `ce-brainstorm` (new), `ce-plan` (was `decompose-beads`/p4), `ce-plan-review` (was
   `plan-review`/p5), `ce-execute` (was `systematic-execute`/p6), `ce-simplify` (new),
   `ce-review` (was `code-review`/p2), `ce-compound` (new). **The p-code aliases are RETIRED**
   (clean cutover — `send --stage p6` now errors naming the fix; no shims, per project policy).
2. **D2 — the learnings corpus.** Per-project `docs/learnings/` (git-tracked): bug-track vs
   knowledge-track docs with frontmatter, one learning per file, `rg`-able — no vector store.
   The GATE grounding-read + the brief's new LEARNINGS field are the return arrow: run one
   teaches it, run two reads it.
3. **D3 — the compounded wave-swarm.** The token expands to EIGHT steps:
   GATE → DISPATCH → EXECUTE → OBSERVE → **REVIEW** → VERIFY → **COMPOUND** → SIGNOFF.
   REVIEW (`ce-review`) audits each closed bead's diff against its brief BEFORE VERIFY audits
   receipts; COMPOUND (`ce-compound`, `on_error: continue`, after VERIFY per captain decision)
   runs the learning gate. VERIFY gains the **residual gate**: every finding fixed in the diff
   or filed as a bead id — the bead IS CE's "durable sink".
3b. **Failure path** (captain decision): a failed VERIFY never reaches COMPOUND in the linear
   YAML, so OBSERVE's escalations carry a `lesson:` line — failure lessons enter the corpus at
   the next compound run instead of being lost.
4. **D4 — honesty vocabulary.** Terminal states (`done | clean_skip | blocked | failed`) in the
   D4 report contract (a clean skip is a SUCCESS; a missing report is unknown, never failed);
   `settled:` labels for captain-briefing decisions (review routes preference-findings to
   report-only `settled_conflict`); idempotent re-entry (run the bead's VERIFY at HEAD first —
   already green means already done); independence doctrine (two lenses in one pane are two
   perspectives, not two witnesses).
5. **D5 — workflows.** Builtins renamed where they ARE CE concepts: `ce-brainstorm` (was
   `brainstorm`), `ce-plan` (was `plan`), `ce-plan-review` (was `plan-review`); slash short
   forms (`/plan`, `/plan-review`, `/brainstorm`) keep their names and retarget. NEW: `ce-compound`
   (standalone capture from any topology) and `ce-sweep` (corpus maintenance — Keep / Update /
   Consolidate / Replace / Delete; a learning contradicting standing-orders/AGENTS.md guidance
   outranks ordinary staleness, resolved by which side the code follows).
6. **D6 — topology reach.** swarm-mode gets D3 for free (the token). **loop-mode**: the
   beads-body brief carries the grounding read + the post-close compound gate (a qualifying
   learning ships in the same commit as the work). **open-mode**: the standing instruction +
   `flywheel workflow run ce-compound --execute` one-shot; `decide` routes "compound the
   learning" / "capture what we learned" to it.

Toron side (amended in toron's ADR-0022 addendum): `Phase` enum gains `compound` — the closed
six-value vocabulary deserializes exactly as before; no R-P lint gate for compound (deferred
until a measured need — its output is an artifact on disk, not a self-report).

## 2. What CE calls these (vocabulary crosswalk)

| CE concept | Flywheel primitive after this ADR |
| :--- | :--- |
| Learning / docs/solutions/ + ce-compound | `docs/learnings/` + `ce-compound` stage/workflow |
| The return arrow (plans read learnings) | GATE grounding read + brief LEARNINGS field |
| ce-code-review personas / detection conditions / confidence anchors / independence | `ce-review` stage (evidence-quoted findings, autofix classes, independence rule) |
| Residual ("durable sink before done") | VERIFY residual gate → `br create` bead id |
| Clean skip vs crash | `terminal:` states in the report contract |
| ce-simplify | `ce-simplify` stage + "simplify as you go" in `ce-execute` |
| ce-compound-refresh / ce-sweep | `ce-sweep` workflow (cron-able) |
| Outcome spine | already ours: the brief (GOAL/…/FORBIDDEN/REPORT/STANDING) — stricter than CE's |
| Detached job / headless / model tiers / bounded repair | already ours, stronger (Resonate loop, byte-identical non-TTY, catalog slots, repair-until-verified) |
| Transport commits (host commits, workers never) | **rejected** — would invert `br close --commit-sha`; we keep crew-commits + move review BEFORE verify |
| CODING_STANDARDS.md | **rejected as a new file** — project tier `.flywheel/` + standing orders are the criteria home |

## 3. Non-goals

* No vector store / embedding retrieval — `rg` over frontmatter is the whole retrieval story.
* No change to beads schema (residuals are plain `br create` beads; learnings provenance is a
  frontmatter field, not a typed bead field).
* No `Stage.refs` (CE's phase-loaded kernels) — deferred; the 20k single-body cap stays, with
  the authoring rule that outcome-spine content leads the body.
* No new principles-registry names in this ADR (candidates `score-artifacts-not-essays`,
  `state-the-condition-not-the-cases` recorded for a future registry ADR).
* No cross-project corpus — project scope only (captain decision); a shared store can layer
  later as a second grounding root.

## 4. Consequences

* Good: every topology now compounds — swarm via the token, loop via the brief, open via the
  one-shot; review happens on the diff before the ledger is trusted; p-codes (a readability
  complaint older than this repo's first swarm) are gone.
* Bad: `send --stage p*` breaks loudly for any prompt still carrying p-codes (the sweep in this
  change covers src/, tests/, docs/, skill — residuals will error naming the fix, which is the
  policy).
* Bad: REVIEW adds a reviewer pass per wave (cost); bounded by the existing
  `FLYWHEEL_COMPOSE_MAX_SECONDS` and the single-lens choice (no fan-out) — the R-P3 n≥2 panel
  is the escalation path via `interrogate`/`phase-review` when a captain wants witnesses.
* Neutral: `ce-plan` names both a stage and a workflow by design — one concept, one token;
  compose disambiguates (`--parts ce-plan` = stage part, `workflow:ce-plan` = workflow part).

## 5. Principles mapping

| Principle | Decision it changed |
| :--- | :--- |
| `encode-lessons-in-structure` | the entire ADR: session folklore becomes a corpus, a stage, and a token step |
| `prove-it-works` | REVIEW audits the artifact; VERIFY's residual gate closes CE's "session-only finding" hole |
| `guard-the-context-window` | grounding reads return paths, not bodies; learnings ride briefs as constraints |
| `subtract-before-you-add` | p-code alias machinery deleted, not deprecated; no third criteria file |
| `never-block-on-the-human` | COMPOUND never gates the run (`on_error: continue` by construction) |

## 6. Topology wiring (the "modes" answer)

* **swarm-mode** (`/swarm-mode` → `compose --parts wave-swarm --run`): the eight-step token —
  no manual chaining; REVIEW and COMPOUND are inside the token by captain decision.
* **loop-mode** (`loop run <p> --work`): the beads-body template's lifecycle carries the
  grounding read (step 5) and the compound gate before close (step 8) — every legal close
  evaluates the learning gate once.
* **open-mode**: standing instruction (AGENTS.md + skill): after verified non-trivial work,
  run the ce-compound protocol — `flywheel workflow run ce-compound --execute` works with no
  crew; `decide` routes the capture phrases to it.

## 7. Implementation

Bead `bd-4el4` (this change). Files: `src/stages.rs`, `src/builtin_workflows.rs`, `src/crew.rs`,
`src/decide.rs`, `src/main.rs`, `src/config.rs`, `src/workflow_catalog.rs`, `src/headless.rs`,
`src/principles.rs`, toron `crates/toron-workflow/src/schema.rs` (+ADR-0022 addendum),
`docs/learnings/` (README + seed), `.flywheel/standing-orders.md`, `integrations/beads-body.md.tpl`,
AGENTS.md / README.md / docs / skill §5 / CHANGELOG.

## 8. Verification

`docs/decisions/_scripts/verify-0009.sh`:

- [ ] `stage_p_codes_retired_ce_names_resolve` — p-codes error; ce-execute resolves
- [ ] `compiled_in_exposes_ce_loop` + `ce_loop_seed_phases_tagged` — 7 ce- stages; compound phase valid both sides
- [ ] `wave_swarm_compose_token_expands_eight_steps_and_lints_clean` — 8 steps, REVIEW@5/ce-review, COMPOUND@7/ce-compound/phase compound, lints clean
- [ ] `wave_dispatch_briefs_ce_execute_with_learnings` — DISPATCH briefs ce-execute + learnings param
- [ ] toron-workflow `phase_enum_closed_vocabulary_roundtrip_and_reject` — six values, unknown still fails parse
- [ ] `docs/learnings/` exists with README + ≥1 seeded learning
- [ ] Skill Sync Contract: robot_capabilities `stages`, AGENTS.md, SKILL.md §5, CHANGELOG updated

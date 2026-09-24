# Upstream compound-engineering parity review + chiebukuro compound proposal (bd-a1st)

Date: 2026-09-24. Sources: EveryInc/compound-engineering-plugin @ live
3.28.2 (cloned 2026-09-24) vs `docs/upstream-parity.toml` pin 3.26.2;
ADR-0009; `src/stages.rs`; `src/builtin_workflows.rs` (ce-compound,
ce-sweep, wave-swarm); `docs/learnings/README.md`.

## 1. Position

Upstream moved 3.26.2 → **3.28.2** (33 → **36 skills**). The manifest pin
is two minors stale — re-pin and diff before adopting anything below.
ADR-0009's core finding still holds: local discipline (beads two-step
close, receipts, residual gate, reinforcement/decay tiers) is stronger
than upstream on proof; upstream is stronger on capture machinery.

## 2. Parity table

| Upstream capability | Local status | Verdict |
|---|---|---|
| ce- lifecycle vocabulary | compiled into stages/workflows | adopted, no action |
| Evidence-quoted review + residuals | ce-review + VERIFY residual gate | adopted, no action |
| Learning return arrow (LEARNINGS field, gate grounding) | briefs + gate projection | adopted, no action |
| Session-history mining (scripts + session-historian) | HEAD commit + steer-channel lessons + receipts only | **GAP G1** |
| Vocabulary capture (CONCEPTS.md) | open-vocabulary tags only | **GAP G2, recommend decline** |
| Claim validation (validate-doc-claims/frontmatter + semantic validator) | receipts + residual gate cover proof, not prose claims | **GAP G3, small** |
| Lightweight/full depths + non-interactive terminal signals | one housekeeper prompt, free-form reply | **GAP G4** |
| Auto-memory scan at compound time (Phase 0.5) | no memory read in the gate | **GAP G5 → chiebukuro proposal §4** |
| Learnings-researcher review persona | LEARNINGS grounds the brief at GATE (earlier than review) | covered alternatively, no action |
| Worth audit (delete accurate-but-redundant) | archive-only, never delete | deliberate divergence, no action |
| Bake-off cost routing | partial (per manifest) | unchanged, no action |

## 3. Revisions recommended (small, backward-compatible)

- **R1 — ce-compound workflow: `depth` input + terminal signal.**
  Add `depth: lightweight|full` (default full) and require the reply
  JSON to end on `Documentation complete` or `Documentation skipped +
  reason`. Mirrors the upstream non-interactive contract; current
  callers pass nothing and get today's behavior.
- **R2 — ce-compound stage prompt: session-history probe.** Add one
  instruction: grep the steer channel + `.flywheel/receipts/` for the
  bead/wave id before drafting (the local equivalent of the
  session-historian probe; our session sources are thinner, so keep it
  a grep, not a subagent fleet).
- **R3 — ce-compound prompt: claim self-check.** One line: every
  file:line/SHA claim must resolve in the tree or be marked historical
  (the manual fallback upstream itself prescribes when its script
  cannot run — we adopt the fallback, not the script).
- **R4 — manifest re-pin.** Pin 3.28.2, record the 33→36 skill delta,
  re-run the offline validation. No code rides this step.
- **Declined — CONCEPTS.md.** Tags + corpus search already disambiguate;
  a second glossary is a sync burden with no reader. Revisit if tag
  collision causes a real miss.
- **Declined — worth-delete.** Archive-only stays; deletion is
  unrecoverable context loss for a marginal tidiness gain.

## 4. Chiebukuro integration proposal (complement, never replacement)

`docs/learnings/` stays the repo-local durable sink (it is upstream's
"repo-local durable solutions" done right: git-tracked, rg-able, same
commit as the work). Chiebukuro adds the two planes the corpus cannot
do:

- **Write path.** After the file lands, the compound run records the
  session conclusion to the memory plane (`chie_memory_record`) and,
  for cross-project durable insights only, promotes via
  `chie_memory_promote` into the wiki review queue. Memory is
  ephemeral-by-design (dreamer-consolidated); the file is the record,
  memory is the index. Never route the return arrow through memory
  alone — session-scoped loss would silently break run-two-reads-it.
- **Read path (closes G5).** Grounding reads become three lanes with
  authority labels: `rg` over the corpus (repo truth), `chie_qmd_search`
  (curated wiki), `chie_memory_recall` (prior session decisions).
  Memory hits are advisory and labeled; they never override corpus or
  code. This mirrors upstream's auto-memory scan + learnings-researcher
  without their subagent fleet.
- **Refresh path.** `ce-sweep` already owns accuracy/worth for files;
  promoted wiki pages fall under the wiki review queue instead — do not
  duplicate the queue in ce-sweep, just link it.
- **First step (small diff).** Extend the ce-compound workflow task
  text: two read calls + one record call, each degrading to current
  behavior when chiebukuro is absent (offline-fallback principle, same
  as the Jev tables). No new workflow, no new stage. Estimated: prompt
  text only, verified by the existing ce-compound shape tests.

## 5. Open questions (for the captain, not decided here)

1. Should memory-plane conclusions ever AUTO-promote to the corpus,
   or stay human-gated through the wiki queue?
2. Is the 4k-token GATE projection budget shared with wiki/memory
   hits, or does each lane get its own cap?
3. Re-pin to 3.28.2 now, or wait for the next upstream release and
   review the delta once?

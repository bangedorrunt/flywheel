# ADR-0006 — Parameterized Stage Payloads + Wave-Scoped Dispatch (the unattended wave swarm)

| Field | Value |
|-------|-------|
| **Status** | Accepted (2026-08-22; captain, post-session on the beads ADR-0002/0001 swarm) |
| **Date** | 2026-08-22 |
| **Decision Maker** | Captain (bangedorrunt) |
| **Relates to** | ADR-0005 (prompt surface + doorbell), toron ADR-0019 (playbook workflows, principles Spec 5/6), beads [ADR-0001](../../beads/docs/decisions/0001-make-beads-the-fail-closed-work-ledger.md) (typed `wave`/`principles` fields), `flywheel decide` default routing |
| **Governs** | `flywheel send --stage` (param substitution), `flywheel loop run --work --wave-label`, `flywheel workflow compose` (builtin `wave-swarm` recipe + verify stage), `flywheel ledger receipt` (principles-aware), stage templates under `integrations/prompts/*.tpl` |
| **Verification** | `docs/decisions/_scripts/verify-0006.sh` (see §7) |

---

## 1. TL;DR

The 2026-08-22 wave-gated swarm on the beads repo (ADR-0002 W1→W4 + ADR-0001 wave-1 graph,
34→22 bead program) ran end-to-end on **captain hand-steering**: five bespoke lane briefs, a
hand-rolled channel watcher, manual receipt adjudication, and one manually-defused auto-dispatch
hazard. Every piece of coordination machinery (spawn, mail, channels, reservations, build slots)
was flywheel's; every piece of *decision* machinery was the captain's, because the composables
could not carry the task contract.

**Decision:** four additions, each closing one measured gap:

1. **D1 — Parameterized stage payloads.** `send --stage p1 --param bead=<id> --param scope=<globs>
   --param exit=<bar> --param channel=<slug>`. Templates gain `{{var}}` slots; missing required
   param is an error; zero params behaves exactly as today.
2. **D2 — Wave-scoped dispatch.** `loop run --work --wave-label wave-N` intersects `br ready`
   with the label and asserts no open lower wave. Default `--work` behavior unchanged.
3. **D3 — `wave-swarm` composed workflow (builtin).** GATE → DISPATCH → OBSERVE → VERIFY →
   CAPTAIN-SIGNOFF on the existing `compose --parts` grammar; DISPATCH emits D1 payloads;
   VERIFY runs the receipt at committed HEAD and checks close-reason VERIFY tails **and
   principles citations**; graduates via the existing `--run --keep` green-run path.
4. **D4 — Report conventions as payload params.** Long crew reports go to a `report_file`
   param path; the channel gets a short summary. Kills the 768-char truncation failure mode
   observed twice in the session.

The captain stays at the gates (adjudication, wave sign-off, cross-lane rulings). Everything
between gates becomes a composed, bounded, observable workflow.

## 2. Measured drift (2026-08-22 session, evidence not vibes)

| Gap | Evidence |
| :--- | :--- |
| `decide` never routes wave-gated graphs to compose | `flywheel decide "<objective>" --json` → `{"primitive":"steer"}` while a wave-labeled bead graph existed; the compose path requires manual invocation |
| Stages cannot carry a task contract | `send --stage` takes only the stage name; the captain wrote 5 bespoke briefs (~1.5k chars each) to carry bead id, file ownership, exit bar, hand-over rules |
| Auto-dispatch grabs out-of-scope work | `br ready` on the beads repo returns non-wave backlog (P0 git-authority regression, CASS rehearsal family) that `loop run --work` would dispatch to wave crews mid-flight |
| Channel posts truncate | two crew reports cut mid-word at ~768 chars; full content survived only in pane scrollback and had to be re-requested |
| Receipts + principles checks are manual | every close waited for a captain-orchestrated verifier run; principles citations in close reasons were unenforced until a mid-session captain directive |

## 3. Decisions

### D1 — Parameterized stage payloads

`flywheel send --stage <name> [--param k=v ...]`:

* Stage templates (`integrations/prompts/*.tpl`) gain `{{bead}}`, `{{scope}}`, `{{exit}}`,
  `{{channel}}`, `{{pin}}`, `{{report_file}}` slots.
* Each stage declares required params; a missing required param is a usage error naming the slot.
* Param values are substituted verbatim; no shell interpolation (they are prompt text, not argv).
* Zero `--param` → byte-identical prompt to today (back-compat; `--raw` unchanged).
* Skill Sync Contract applies (Commands enum, robot_capabilities, AGENTS.md, SKILL.md §5).

### D2 — Wave-scoped dispatch

`flywheel loop run --work --wave-label wave-N`:

* Ready set = `br ready --json` ∩ `labels contains wave-N`, AND no open/in_progress bead with
  `wave-M`, M < N (label-based hold, matching today's loop semantics).
* Label parsing is explicitly **transitional**: when beads ADR-0001 wave-1 ships the typed
  `wave` field in ready JSON, the filter reads the field and the label path becomes fallback.
* Default `--work` (no flag) is unchanged — existing repos with unlabeled beads keep behavior.

### D3 — `wave-swarm` composed workflow

Builtin recipe on the `compose --parts` grammar:

```
GATE      wave-label filter (D2) + preflight
DISPATCH  for each ready bead: send --stage p1 --param bead=… scope=… exit=… (D1)
OBSERVE   steer-channel watch (kind-9 receipts, claims, blocked)
VERIFY    per close claim: receipt at committed HEAD; close-reason VERIFY tail;
          principles citations present and decision-naming (D4 ties in)
SIGNOFF   captain-only: wave gate close + next-wave dispatch (manual, by design)
```

* Bounded by `FLYWHEEL_COMPOSE_MAX_SECONDS` (default 3600) like all composed runs.
* Graduation: `--run --keep wave-swarm` on a fixture project, per existing compose policy.
* OBSERVE does not adjudicate — escalations (blocked >10min, conflicts, hand-over requests)
  wake the captain. Judgment stays human (see Non-goals).

### D4 — Report conventions as payload params

* Stage payloads include `report_file` (e.g. `/tmp/<bead>-report-<epoch>.md`).
* Crew contract inside the template: full report → file; channel → ≤700-char summary + path.
* Rationale: two observed truncations; scrollback is not durable storage.

## 4. Principles mapping (the decisions each principle changed)

| Principle | Decision it changed |
| :--- | :--- |
| `encode-lessons-in-structure` | the whole ADR: session folklore (hand-written briefs, watcher loops) becomes typed mechanisms |
| `build-the-lever` | one param/substitution engine serves every future stage, workflow, and repo |
| `prove-it-works` | D3 graduates only via a green `--run --keep` on a fixture, not by assertion |
| `sequence-verifiable-units` | waves remain the verifiable unit; the workflow gates on them instead of trusting dispatch order |
| `never-block-on-the-human` | between gates the loop dispatches/observes; the captain is woken only at sign-offs and escalations |
| `guard-the-context-window` | D4: files for full reports, short channel summaries |
| `separate-before-serializing-shared-state` | DISPATCH still respects toron file reservations; the workflow never edits around them |
| `subtract-before-you-add` | no new dispatch daemon, no new mail plane — everything composes existing surfaces (send, compose, loop, ledger, channels) |

## 5. Non-goals

* No auto-adjudication of crew judgment (scout misreadings, fix-vs-hand-over forks, regression
  suspicion). Those stay captain territory — that is the observed value of the session.
* No change to `decide`'s default routing table in this ADR (routing wave-graphs to compose is a
  follow-up once `wave-swarm` graduates).
* No bead/tracker schema changes here — typed `wave`/`principles` are beads-repo ADR-0001
  wave-1 scope; this ADR consumes them when they land.
* No toron surface changes.

## 6. Consequences

* Good: the next wave swarm runs unattended between gates; briefs become data, not prose.
* Good: auto-dispatch becomes safe on repos with mixed backlog (the flag is the sorting tray).
* Bad: one more substitution surface to test (template/schema drift is a new lint).
* Bad: label-based wave hold is stringly until beads schema 18 lands; flagged transitional.
* Neutral: `--stage` without params is forever identical to today.

## 7. Verification

`docs/decisions/_scripts/verify-0006.sh`:

- [ ] `flywheel send --stage p1 --param bead=x` substitutes `{{bead}}` (unit test on template render)
- [ ] `flywheel send --stage p1` with a required param missing exits non-zero naming the slot
- [ ] zero-param render is byte-identical to the pre-ADR prompt (golden)
- [ ] wave filter unit test: labeled/unlabeled/mixed fixture bead sets produce the exact ready set
- [ ] `--wave-label` asserts lower-wave hold (fixture: wave-2 open blocks wave-3 dispatch)
- [ ] `flywheel workflow compose --parts wave-swarm --dry-run` lints green
- [ ] `flywheel workflow compose --run --keep wave-swarm` green-run on fixture project
- [ ] VERIFY stage rejects a close reason lacking VERIFY tail or principles citations (fixture)
- [ ] Skill Sync Contract: command enum + robot_capabilities + AGENTS.md + SKILL.md §5 updated

## 8. Implementation beads

Filed in this repo's tracker (`br list --labels adr-0006`): `stage-params` (D1),
`wave-scoped-dispatch` (D2), `wave-swarm-recipe` (D3, depends on D1+D2),
`principles-aware-receipts` (VERIFY-stage citation checks, extends `ledger receipt`),
`adr-0006-docs-sync` (Skill Sync Contract + verify script + README row; depends on all).

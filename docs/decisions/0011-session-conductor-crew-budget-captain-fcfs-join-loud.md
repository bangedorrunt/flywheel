<!-- governed-by: ADR-0005, ADR-0008, ADR-0010, toron ADR-0011, toron ADR-0018, toron ADR-0027, toron ADR-0029 -->
---
status: accepted
date: 2026-09-08
decision-makers: Captain (bangedorrunt)
consulted: LucidFalcon vision probe 2026-09-08 (live decide/crew-preview/helm/doorbell/snapshot); flywheel ADR-0010 peer review
informed: flywheel × toron × beads maintainers; harness agents (grok/pi/omp/opencode/codex)
---

# ADR-0011: Session conductor — crew budget N, captain FCFS, join-loud

| Field | Value |
|-------|-------|
| **Status** | **Accepted** (2026-09-08; captain word on four cuts). **Amended 2026-10-25:** the mode question is `open` or `swarm`. Ready beads stay the `loop-mode run` command and classify as `open`. A stored session word `loop` aligns to `open`. Crew budget N, captain FCFS, and join-loud are unchanged. |
| **Date** | 2026-09-08 |
| **Decision-makers** | Captain (bangedorrunt) |
| **Extends** | [ADR-0005](0005-always-on-collab-baseline-and-command-surface.md) (topologies open/loop/swarm; doorbell Layer 0), [ADR-0008](0008-workflow-compose-topology-agent-optimal-mental-model.md) (topology ≠ workflow), [ADR-0010](0010-delivery-semantics-attention-triage-decision-lifecycle.md) (helm/snapshot/doorbell reflexes), [toron ADR-0011](../../toron/docs/decisions/0011-coalition-orchestration.md) (intent/reserve/join store), [toron ADR-0018](../../toron/docs/decisions/0018-agent-identity-naming-grammar.md) (captain alias), [toron ADR-0027](../../toron/docs/decisions/0027-one-workspace-tick-control-plane.md) (one tick), [toron ADR-0029](../../toron/docs/decisions/0029-rot-supervision-handoff-pacing-and-config-trust-boundary.md) (repo config may only narrow — `crew.max` is operator-only) |
| **Does not amend** | frozen MCP tool surface; kind registry; one-way deps; SIGNOFF remains captain-only for irreversible preference (auto-pass only when REVIEW residual is empty and OPEN DECISIONS is empty — Wave D) |
| **Governs** | `src/crew.rs` (unique-role infer + `pack_crew`); compose/steer/workflow `--crew` as total N; `crew.max` cap; session briefing contract (Wave B); captain FCFS (Wave C); join-loud (Wave C); doorbell-on-arrive (Wave C); captain-as-protocol (Wave D) |
| **Tracker** | beads. Never pull requests. |
| **Verification** | `docs/decisions/_scripts/verify-0011.sh` + per-wave tests below |

---

## 1. TL;DR

A random harness with an objective must **arrive → brief → two questions (mode, N) → commit → run-to-done**. A second harness on the same project **joins by default and says so loud**. Crew size is a **runtime total N**, not a YAML-fixed occurrence count. **N is capped** (`crew.max`, default **5**, operator-only; 4–5 is the live budget). Captain is a **first-come-first-serve slot** with stale eviction and an explicit override.

Four captain cuts (2026-09-08):

1. Write this ADR, then implement Wave A inside it.
2. N is **total panes**. Workflows do not freeze crew size. Cap 4–5 now, raise later via operator config.
3. Captain = FCFS live slot, no stale holder; user may override.
4. Join by default, **announce loud** so the human sees it.

**Wave A (this cut):** stop the balloon. Unique-role infer + pack into N + `--crew N` + cap. Briefing edits must mutate the plan. Loud pack line on every dispatch.

**Waves B–D:** specified here, code follow-up. Do not invent them green.

---

## 2. Context — what the binaries did on 2026-09-08

Live probes (toron project, LucidFalcon):

- `decide "swarm with 2 crews"` → **steer**. `"swarm"` is not a needle. N dropped.
- `decide "wave-swarm 3 crews"` → token hit, argv has **no `--crew`**.
- `flywheel crew preview wave-swarm` → **7** panes (planner×2, reviewer×2, +3). Occurrence-count of `@role` steps.
- `crew preview bug-fix` → **8**.
- Compose has **no `--crew`**. Only `--allow-crew-expand`, which confirms the balloon.
- `steer_brief` TTY lets you type a new count, then **discards it** (`edits_change_the_preview_not_the_context`).
- `doorbell status toron` → empty (no dispatcher ever started).
- `helm` UNREAD / reservations: **hardcoded empty** in `snapshot.rs` (calm is a lie).
- `CoalitionStore::join` exists, `#[allow(dead_code)]`.
- Spawn on a live flywheel swarm **refuses** (or `--retire-orphans` kills it).

Root causes: decide is a keyword table (not a briefing); crew auto counts step cardinality; topology is not a decide axis; join is unwired; supervision is optional infrastructure.

Quality gates (playbook lint, VERIFY, ce-review, fail-closed close) stay. The leak is the door.

---

## 3. Decision set

### D1 — Topology ⊥ workflow (speech already Accepted; now a briefing axis)

`/open` and `/swarm` are the modes. `/loop` is the bead command inside open mode (amended 2026-10-25). `decide` / `briefing` MUST emit a `topology` field. They MUST NOT collapse topology into a workflow primitive. A one-file typo is open. Ready beads with no new graph run `loop-mode run` and the mode is open. Multi-lane is swarm. Unmatched still names its topology in the JSON.

Wave B lands the JSON. Wave A does not change decide needles except where needed to honor `--crew` on compose argv.

### D2 — Crew budget N (Wave A)

**N is total panes**, not coder-N, not occurrence-count.

1. **Infer unique roles** from `@profile` refs (and `call_workflow` recursion). Two `@planner` steps ⇒ **one** planner pane, not two. YAML `crew: { coder: 2 }` is a **role inventory**, not a spawn lock — counts in YAML are ignored at pack time.
2. **Pack** unique roles into N:
   - Keep-priority: `coder, reviewer, planner, scout, oracle, housekeeper`, then others alphabetically.
   - First `min(N, unique)` roles get one pane.
   - If `N > unique`, extra panes go to `coder` if present, else the first kept role.
   - If `N < unique`, overflow roles **alias** onto the first kept role. Dispatch rewrites `@overflow` → `@host` before invoke so "no crew pane" cannot fire.
3. **Default N** = `min(unique, crew.max)`.
4. **Cap** `crew.max` default **5**. Operator layer only (`~/.config/flywheel/`, env `FLYWHEEL_CREW_MAX`, flag `--crew-max`). Repo `.flywheel/` MUST NOT raise it (0029 trust boundary). A request `N > max` **fails closed**, naming the cap and the raise path.
5. **CLI:** `--crew N` on `workflow compose` and the existing `--crew` on `steer` / `workflow run` parse a bare integer as total N. `auto` = default pack. `role:n,role:n` is an explicit map whose **sum is N** (must be `<= max`); missing workflow roles alias onto the map's first role. Squad names still resolve as today, then pack to `sum(squad)` capped.
6. **Loud:** every pack prints one line (stderr + JSON `packed`): `packing <wf> into N panes (cap=max, unique=U): coder, reviewer, … (housekeeper → coder)`. Robots see it. Humans see it. No silent 7.
7. **`--allow-crew-expand`:** no longer the confirm for N>1. Packed-under-cap is the default. The flag is kept as a no-op alias (warn once) so old argv does not hard-bail; it does not raise the cap.
8. **Briefing:** section 1 asks **N** (default shown, cap named), not per-role counts. Confirmed N mutates the plan used for spawn. Preview-only edits are illegal.

### D3 — Session briefing (Wave B)

New noun `flywheel briefing "<objective>" [--json] [--commit]`. Observes helm/snapshot/ready/doorbell/coalitions/presence (decide Snapshot flags become automatic). Emits `flywheel-briefing.v1`:

- `world` (joinable, dispatcher, degraded, ready_beads, foreign_workspaces)
- `proposal` (topology, workflow, token, stages, packed crew, add/remove vs unique, why, argv including `--crew N`)
- `alternatives` (loop / open / repair)
- `questions` (topology, N) with defaults

`--commit` runs argv. `--yes` commits defaults. Harness rule: print proposal, ask the two questions, commit. AGENTS.md WORKFLOWS block collapses to this door. Slash `/swarm` `/loop` `/open` start with arrive+briefing; `/swarm` MUST NOT pin wave-swarm before briefing sees joinable / ready-beads / N.

`decide --json` remains the classifier **leaf** inside briefing. Do not invent a second selector.

### D4 — Captain FCFS (Wave C)

The `captain` alias is a **slot**, not a personality.

- **Claim:** first arrived live pin on the project that is not stale takes the slot (`toron contact captain --pin`), **loud**.
- **Stale:** no presence in the TTL window AND no doorbell `last_wake` within the same window AND no herdr pane still named. Stale holder is cleared (`--clear`) then FCFS retries. Never leave a 9-day-dead pin as captain.
- **Override:** `toron contact captain --pin <x>` always wins. `--clear` vacates for the next FCFS claim.
- **Open-mode:** this pane claims if the slot is empty/stale; it does not guess a roster pin (existing ladder).
- Spawn-time role bind stays; captain noun stays reserved. Bind failure is **fail-closed** for the captain slot (cannot start a swarm with no captain address). Other role binds stay warn+continue until a follow-up tightens them.

### D5 — Join by default, say it loud (Wave C)

If a live fleet exists on this project (dispatcher gen-file, or live herdr panes in the project workspace, or an active coalition):

- Default proposal topology is **join**, not a second `compose --run`.
- **Loud banner** (stderr + helm + kind-9 one-liner): project, live pin count, held paths, coalition id, "JOINING — not spawning". The human must see it without asking. Not a blocking y/N (never-block-on-the-human) unless files HARD-conflict.
- Wire `CoalitionStore::join`. Spawn `--join` / compose `--reuse` the live workspace. Open-mode arrive attaches peer-aware to the existing dispatcher; it does not fork one.
- Declare: presence `working`, coalition intent, reserve before edit, mint `corr`.
- A second `/swarm-mode` on a live fleet is join, not a parallel wave-swarm.

### D6 — Arrive always (Wave C)

Session start (any topology, including open): register/reuse pin → `helm` (truthful) → presence publish → doorbell start-or-attach → FCFS captain claim if needed → briefing. Doorbell watch child is `toron watch --live` (today's 1s snapshot is a defect vs ADR-0028). Helm must not hardcode mail/reservations empty.

### D7 — Captain-as-protocol (Wave D)

The tick is the supervisor. The human pane is the OPEN DECISIONS fold. In-band "this grok pane polls helm" is the silent-captain failure and is **forbidden** as the design. Rot scorer (0029 Wave 1), WaitCorr drain on Sweep as well as WatchNdjson, nudge cap 3, blocked crews wedge to helm, SIGNOFF auto-pass iff REVIEW residual empty AND no OPEN DECISIONS.

---

## 4. Options (crew size)

| Option | What | Why not / why |
| :--- | :--- | :--- |
| A. YAML-fixed `crew: {coder:3,…}` | Today's explicit maps | Captain: size must be flexible. Budget changes. |
| B. Unique×1, no cap | wave-swarm = 5 always | Still over a 4-pane budget; no "2 crews". |
| C. coder:N, supporting stay 1 | "2 crews" = 2 coders + staff | Captain: N is **total**. |
| **D (chosen). Unique roles packed into total N, cap `crew.max`** | Flexible, budget-honest, aliases when N < unique | Chosen. |

Drivers: `never-block-on-the-human` (defaults, loud not y/N), `encode-lessons-in-structure` (occurrence-count becomes unrepresentable), `experience-first` (harness asks N once), `boundary-discipline` (cap is operator-only).

---

## 5. Implementation plan

### Wave A (this cut — flywheel)

**Affected paths:**

- `src/crew.rs` — unique infer; `pack_crew`; `apply_role_aliases`; `DEFAULT_CREW_MAX=5`; tests
- `src/config.rs` — `crew.max` (operator config / `FLYWHEEL_CREW_MAX`)
- `src/main.rs` — compose `--crew` / `--crew-max`; parse integer as N; loud pack; skip expand hard-bail under cap; `crew preview` shows packed default
- `src/steer_brief.rs` — ask N; `BriefingOutcome::Go { budget }`; callers pack
- `src/pane.rs` — alias-safe lookup (rewrite-first, lookup remains profile-based)
- flywheel skill `SKILL.md` compose/crew speech
- `docs/decisions/README.md` + `docs/decisions/_scripts/verify-0011.sh`

**Patterns:** `plan_crew_with` stays the infer seam; pack is a second pure function so dry-run can show unique **and** packed. `resolve_crew_fanouts` style rewrite for aliases.

**Avoid:** new MCP tools; new kinds; raising cap from repo config; occurrence-count `+= 1`; briefing preview-only edits; treating `--allow-crew-expand` as a cap raise.

**Tests (Wave A):**

- unique: two `@reviewer` steps ⇒ reviewer:1
- wave-swarm unique = 5 roles, default pack N=5, total panes 5 (not 7)
- bug-fix unique ≤ 3, default pack total ≤ 3 (not 8)
- pack N=4 of 5 roles: 4 members, one alias, aliases rewrite dispatch profile
- pack N=7 with max=5: error names cap
- pack N=2 of 3: extra aliases; extra budget N=4 unique=2 ⇒ coder:3 or worker+2
- `--crew 4` parse on compose
- briefing typing `4` yields `Go { budget: 4 }` (not discarded)
- repair explicit YAML still round-trips; spawn pack of unique=2 default N=2 stays 1+1

### Wave B (follow-up)

`src/briefing.rs` + clap `briefing`; observe snapshot (stop hardcoded empty); decide consumes live facts; slash templates + AGENTS.md door.

### Wave C (follow-up)

`CoalitionStore::join` CLI; spawn `--join`; FCFS captain claim + stale eviction; arrive banner; doorbell `--live`; helm truth.

### Wave D (follow-up)

0029 rot Wave 1; WaitCorr drain; nudge cap; SIGNOFF auto-pass predicate.

---

## 6. Verification (acceptance)

Wave A:

- [x] `verify-0011.sh` green
- [x] unique-role infer (two `@reviewer` ⇒ 1) + pack default ≤ cap
- [x] `pack_crew` N>max fails closed naming `crew.max`
- [x] compose `--crew` parses total N
- [x] briefing `Go { budget }` applied (not preview-only)
- [x] two `@planner` steps do not spawn two planner panes
- [x] skill mentions packed N + cap 5

Wave B:

- [x] `flywheel-briefing.v1` + clap `briefing` (`--json`/`--commit`/`--yes`/`--crew`)
- [x] snapshot mail/reservations observed (degrade only on miss)
- [x] `decide --json` emits `topology`; `--project` fills Snapshot from helm
- [x] slash `/swarm` `/loop` `/open` start with arrive+briefing; `/swarm` does not pin wave-swarm first

Wave C:

- [x] `coalition join` wires `CoalitionStore::join`
- [x] `spawn --join` arrives (JOINING banner, no second swarm)
- [x] FCFS captain claim + stale eviction + override
- [x] `flywheel arrive` loud banner + helm + doorbell start
- [x] doorbell watch child is `toron watch --live`

Wave D:

- [x] rot Wave 1 scorer on `LoopMsg::Rot` → Respawn/Compact
- [x] WaitCorr drains on Sweep as well as WatchNdjson
- [x] nudge cap 3 (`interrupt_nudge`; 4th+ mail-only)
- [x] SIGNOFF auto-pass iff REVIEW residual empty AND OPEN DECISIONS empty

---

## 7. Consequences

**Positive**

- "2 crews" / "4 crews" is a number the binary honors.
- wave-swarm/bug-fix stop silently minting 7–8 panes.
- Cap is a budget the operator owns; raising it later is a config change, not a YAML rewrite.
- Join-loud and FCFS captain are specified so Wave C cannot invent a second swarm or a stale captain.

**Negative / costs**

- YAML `crew: { coder: 2 }` no longer means "spawn 2 coders" unless `--crew` map form says so. Document in skill.
- N < unique multiplexes roles onto one pane (wrong preamble for the overflow stage). Named: overflow runs on the host profile. Prefer N ≥ quality-critical roles (coder+reviewer = 2).
- `--allow-crew-expand` speech in old skills will lie until Wave A skill sync.

**Honesty**

- Waves A–D shipped in this record. Helm mail/reservations degrade (never hard-fail) when toron is down. Rot Wave 1 scores on `LoopMsg::Rot` (bytes/repetition/silence); Advise is a no-op until a caller mails the bead thread. SIGNOFF auto-pass observes snapshot decisions + review-residual lanes, not a model judgment.

---

## 8. Scenario walk-throughs

**S1 — wave-swarm, no `--crew`.** Unique 5, max 5, N=5. Spawn planner, coder, scout, reviewer, housekeeper ×1. Loud: `packing wave-swarm into 5 panes (cap=5, unique=5): coder, reviewer, planner, scout, housekeeper`. Pass: 5 panes. Fail: 7.

**S2 — `--crew 4` on wave-swarm.** Keep coder, reviewer, planner, scout. Alias housekeeper → coder. Compound stage rewrites to `@coder`. Pass: 4 panes, compound still dispatches. Fail: "no crew pane for @housekeeper".

**S3 — `--crew 8` with max=5.** Fail closed: `crew budget 8 exceeds crew.max=5 (set FLYWHEEL_CREW_MAX or ~/.config/flywheel [crew].max)`.

**S4 — TTY briefing types 4.** Spawn uses 4. Pass: `Go { budget: 4 }` applied. Fail: preview-only.

**S5 (Wave C) — second grok, fleet live.** Arrive prints JOINING banner with pin count + held paths; coalition join; no second compose. User sees the banner. Fail: silent parallel swarm.

**S6 (Wave C) — stale captain.** Alias points at a pin with no presence and no pane. Arrive clears, claims FCFS, prints `captain slot claimed: <pin> (previous holder stale)`. Override `--pin` still wins.

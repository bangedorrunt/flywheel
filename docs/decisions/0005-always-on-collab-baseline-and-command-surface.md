# ADR-0005 — Always-On Collaboration Baseline + Composable Markdown Command Surface

| Field | Value |
|-------|-------|
| **Status** | Proposed (captain discussion, 2026-08-19) |
| **Date** | 2026-08-19 |
| **Decision Maker** | Captain (bangedorrunt) |
| **Relates to** | ADR-0001 (S.9.3 peer-triggered pattern), ADR-0003 (mail backend), toron ADR-0018 (identity/naming, identity mode), toron ADR-0020 (two-plane CLI), bd-pattern-peer-triggered |
| **Governs** | `integrations/peer-aware.md.tpl`, `integrations/prompts/*.tpl`, `flywheel integration` generator (`src/main.rs` `cmd_integration`/`prompts_dir`/`render_prompt`), spawn defaults (`cmd_spawn`), `flywheel cron` surface |
| **Verification** | per §7 checkboxes |

---

## 1. TL;DR

The captain's doubt, verified: **slash commands are fire-and-forget prompt
expansions on every harness we target, so composable concerns (awareness,
identity, conflict protocol) must never live behind them.** Decision: split
flywheel's collaboration surface into two layers — **Layer 0: an always-on
baseline** (identity prime, mail poll, reservation watch, guard attribution
on EVERY flywheel-spawned pane + a `flywheel cron`/`toron watch` captain
digest; never command-triggered, opt-out only at spawn), and **Layer 1: a
markdown × frontmatter command surface with arguments** generated per
harness by the EXISTING `flywheel integration` generator (orchestration
modes pick a shape; a new standalone `/peer-aware` retro-primes a running
pane). Per-harness programmatic extensions are **deferred** — one template
source with N thin frontmatter adapters beats N extension APIs to maintain.
The command layer stays as the fallback for unforeseeable events — the
baseline is infrastructure, and infrastructure fails (witnessed 2026-08-19:
guard hatch broken, two working-tree clobbers by an unprimed pane).

## 2. Background & Motivation

### 2.1 The trigger (captain's question, 2026-08-19)

"If crews always collab over toron mail by default, why do we need the
peer-aware slash command template? If I trigger /swarm or /loop, I can't
combine them with /peer-aware." — verified below: correct for every
harness's markdown-command mechanism.

### 2.2 Evidence: harness command mechanics (researched 2026-08-19)

| Harness | Command mechanism | Args? | State/chain? | Fire-and-forget? |
|---|---|---|---|---|
| opencode | `~/.config/opencode/command/*.md`, frontmatter (`description`, `agent`, `model`, `template`, `subtask`) — one expanded user message per invocation | `$ARGUMENTS` (unused by flywheel's commands today) | none | **yes** |
| pi | prompt templates `~/.pi/agent/prompts/*.md` (`pi.dev/docs/latest/prompt-templates`): frontmatter (`description`, `argument-hint`) + `$1`/`$@`/`${1:-default}` positional args | **yes, rich** | none (pure expansion) | **yes** |
| pi/omp (programmatic) | `pi.registerCommand(name, {handler(args, ctx)})` — full TS handler: args, UI, session, state | yes | **yes** | no — but per-harness API to maintain |
| omp | pi fork: both mechanisms above (`~/.omp/agent/prompts`) | as pi | as pi | markdown: yes |
| pa (prime-agent) | `~/.prime/agent/commands` markdown; extension system exists | TBD adapter | ext: yes | markdown: yes |
| grok | `~/.grok/prompts` markdown | TBD adapter | none | yes |

Verified locally: flywheel's generator already maps all five dirs
(`prompts_dir`, main.rs:14356) and emits `swarm.md`/`loop.md` into opencode
today; pi's extension surface is real (`~/.pi/agent/extensions/
herdr-agent-state.ts` — the H1 lifecycle reporter).

**Consequence:** `/swarm + /peer-aware` is inexpressible as one trigger on
any markdown surface. Composition currently happens only as PROSE frozen
inside playbook text ("/swarm says: spawn --profile, peer-aware default").
Orchestration mode × awareness × identity mode are orthogonal dimensions
collapsed into monolithic, non-parameterized playbooks.

### 2.3 What the baseline must cover (from the 2026-08-19 session)

Two unprimed-pane clobbers (my in-flight edits reverted twice by a peer with
no prime and no reservations) and a broken guard escape hatch are the
existence proof that awareness cannot depend on the captain remembering a
command. Required always-on: identity prime (pin + guard attribution), mail
poll loop (plane S kinds + plane P since_ts), reservation-awareness at
EDIT time (watch kind 30031, not just the commit-time guard), presence
publish, and a captain digest that does not need `loop run` to be alive.

## 3. Decision

### D1 — Layer 0: always-on baseline (infrastructure, never command-triggered)

Every flywheel-spawned pane gets, at spawn time (launch args, ADR-0001
S.9.3 mechanism, unchanged):

1. **Identity prime**: "You are <pin>; mail identity; guard will block
   unattributed/unreserved commits." — ONE line minimum, present even with
   `--no-peer-aware` (single-shot panes still need guard attribution).
2. **Full peer prime** (default-on, `--no-peer-aware` opts out of the LOOP,
   never of the identity line): poll cadence, plane S/P, conflict ladder,
   presence, budgets — today's `peer-aware.md.tpl` body.
3. **Reservation watch**: the template's watch line adds kind **30031**
   (file reservations) to `toron watch --once --kinds 9,7,43001,20001,30031`
   → edit-time awareness of who holds what.
4. **Wait-vs-collab decision table** in the template (presence + lease
   state → defer / request-release / take over / share) so every agent
   decides identically without asking the captain.

Captain side: **`flywheel cron` awareness digest** (new builtin workflow,
e.g. `awareness-digest`): periodic snapshot of presence + active
reservations + expired request-release timers + guard denials → one kind-9
channel post + captain DM on conflict only. `toron watch` feeds it; no
`loop run` required. The loop remains WORK dispatch, not awareness.

### D2 — Layer 1: markdown command surface with arguments (fallback + shape selection)

The command layer survives deliberately — the baseline is NOT 100%
trustworthy (2026-08-19 proves it), and unforeseeable events need a manual
trigger. Changes to the generator, not to harnesses:

1. **Orchestration commands become parameterized**: `swarm.md`/`loop.md`
   (and steer) gain `argument-hint` frontmatter + `$ARGUMENTS` passthrough
   (pi) / `$ARGUMENTS` (opencode), e.g. `/swarm peer-aware=off crews=3`.
   Arguments map onto existing flywheel CLI flags — the command text
   resolves them to a `flywheel` invocation; no new semantics live in prose.
2. **Standalone `/peer-aware` command**: retro-primes an ALREADY-RUNNING
   pane — instructs the agent to adopt the identity line + poll loop +
   reservation watch NOW (recovery path for unprimed panes and manual
   spawns; the exact remedy for this session's clobber incident class).
3. **One source, N adapters**: all commands render from
   `integrations/prompts/*.tpl` through per-kind `frontmatter-<kind>.tpl`
   (exists) into the five `prompts_dir` targets. A new harness = one
   frontmatter adapter + one dir entry — no per-harness code.

### D3 — Extensions deferred (explicit non-default)

Programmatic `registerCommand` extensions (pi/omp/pa) are NOT built now.
Rationale: markdown × frontmatter with args covers parameterization across
ALL harnesses with one template source; extensions mean one API/doc surface
per harness to learn, track, and maintain (captain's stated holdback).
**Revisit trigger**: a concrete need for state, arg validation, or UI that
markdown provably cannot express (e.g. interactive argument pickers) — then
`flywheel extension <kind>` becomes a new generator mode, and D2 templates
remain the cross-harness floor.

### D4 — Identity mode (context, unchanged)

`IdentityMode` stays toron-side per-project routing policy (ADR-0018 D3:
`Open` = any registered name; `Swarm` = canonical `flywheel-<project>-
<role><seq>` or npub only). Flywheel projects SHOULD run `swarm` mode (the
pin-public-plane wave's `set_identity_mode` wiring); this ADR depends on
it, does not change it.

## 4. Options Considered

| Option | Verdict | Why |
|---|---|---|
| A. Baseline-only (drop command layer) | rejected | baseline fails too (guard hatch, unprimed panes); unforeseeable events need a manual fallback — captain's call |
| B. Baseline + markdown args (A + parameterized commands + /peer-aware) | **chosen** | composes by argument on every harness; one source of truth; extends the existing generator |
| C. Per-harness extension commands | deferred | max power, N APIs to maintain; pi/omp/pa only — opencode structurally can't; revisit on proven need |
| D. Orchestrator-everything (awareness only via loop/swarm) | rejected | requires a running loop; 2026-08-19 incident happened precisely with no loop alive |

## 5. Non-Goals

- No new mail transport, plane, or protocol (toron ADR-0020 covers the CLI surface).
- No change to the loop/swarm playbook CONTENT (wave gating, bead discipline).
- No interactive UI work (that is the extension revisit trigger, not this ADR).
- No per-harness behavioral forks — commands differ by frontmatter/dir only.

## 6. Implementation Plan

- **Affected paths**: `integrations/peer-aware.md.tpl`, `integrations/prompts/{swarm,loop,steer,peer-aware}.md.tpl`, `src/main.rs` (`cmd_integration`, `render_prompt`, `PROMPT_WORKFLOWS`/`ORCHESTRATION_COMMANDS`, `peer_prompt_template`, `cmd_spawn` minimal-prime branch), `src/cron.rs` (digest workflow registration), `src/builtin_workflows.rs` (`awareness-digest`).
- **Step 1 — template split**: factor `peer-aware.md.tpl` into `IDENTITY_LINE` (always injected, even `--no-peer-aware`) + `POLL_LOOP` (default-on block). Add kind 30031 to the watch line + the wait-vs-collab table.
- **Step 2 — /peer-aware command**: new `peer-aware.md.tpl` rendering the retro-prime instruction ("adopt identity + poll loop + reservation watch now; publish presence") into all five kinds via `flywheel integration`.
- **Step 3 — parameterized orchestration commands**: `argument-hint` + `$ARGUMENTS` (validated mapping to flywheel CLI flags) in swarm/loop/steer templates; generator tests assert args present per kind.
- **Step 4 — awareness digest**: builtin workflow `awareness-digest` (presence + reservations + expired request-release + guard denials → kind-9 post + conditional captain DM); register a default `flywheel cron` entry.
- **Step 5 — spawn minimal prime**: `cmd_spawn` injects IDENTITY_LINE for every pane; unit test: `--no-peer-aware` pane still carries the identity line.
- **Patterns**: templates stay the single source; per-kind differences live ONLY in `frontmatter-<kind>.tpl`. governed-by comments on touched files.

## 7. Verification (checkboxes)

- [ ] `flywheel integration <kind>` for all 5 kinds emits `peer-aware.md` + argument-bearing `swarm.md`/`loop.md` into the kind's `prompts_dir` (generator test, no live harness needed)
- [ ] `/peer-aware` retro-prime: a live unprimed pane, prompted with the command output, publishes presence + reserves before its next edit (manual live check, one pane)
- [ ] `--no-peer-aware` spawn still injects the identity line (unit test on `cmd_spawn` arg construction)
- [ ] peer template watch line includes kind 30031 (template invariant test)
- [ ] `awareness-digest` cron posts a digest without `loop run` alive (cron run + channel read)
- [ ] `/swarm peer-aware=off` renders the playbook with the flag mapped to `--no-peer-aware` spawns (render test)

## 8. Open Questions (for the discussion)

1. Budget defaults for the retro-primed loop (inherit 5/30/3, or shorter for recovery contexts)?
2. Should the digest run per-project or one cross-project digest (cron entry shape)?
3. `pa`/`grok` argument adapters: confirm their markdown arg syntax (TBD in §2.2) before Step 3 lands for those kinds.
4. Should `/peer-aware` also accept `--budget` overrides in args (maps to the template's three knobs)?

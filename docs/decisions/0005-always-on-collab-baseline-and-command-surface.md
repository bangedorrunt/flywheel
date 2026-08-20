# ADR-0005 — Always-On Collaboration Baseline + Composable Markdown Command Surface

| Field | Value |
|-------|-------|
| **Status** | Accepted (2026-08-20; G + §4.6 addenda + detached lifetime) |
| **Date** | 2026-08-19 |
| **Decision Maker** | Captain (bangedorrunt) |
| **Relates to** | ADR-0001 (S.9.3 peer-triggered), ADR-0003 (mail backend), toron ADR-0015 (flywheel drives, toron carries), toron ADR-0018 (identity), toron ADR-0020 (two-plane CLI; plane W = bus push), bd-pattern-peer-triggered |
| **Governs** | `integrations/peer-aware.md.tpl`, `integrations/prompts/*.tpl`, `flywheel integration` generator (`src/main.rs` `cmd_integration`/`prompts_dir`/`render_prompt`), spawn defaults (`cmd_spawn`), one workspace doorbell dispatcher (flywheel-owned), `flywheel cron` surface |
| **Verification** | `docs/decisions/_scripts/verify-0005.sh` + §7 + §9 |

---

## 1. TL;DR

Slash commands are fire-and-forget prompt expansions. Composable
concerns (awareness, identity, conflict protocol) must never live
behind them. The first draft of this ADR put those concerns in the
**spawn template as a 5-minute self-poll**. That is the same class of
bug: mail is pull, there is no push into an LLM's context, and a
blank pane that never runs the poll is deaf.

**Decision (amended):** two layers.

**Layer 0 — always-on baseline (infrastructure, never command-triggered).**
Identity prime in the prompt (pin is otherwise invisible). One
**workspace doorbell dispatcher** (flywheel-owned, not N per-pin
processes) holds `toron watch` plus a pin-scoped inbox `--since`
sweep and injects one `[doorbell]` line via `herdr agent.prompt`.
The agent **pulls** the body. The template is the **handler**, not
the timer. `--no-peer-aware` opts the pane out of the dispatcher +
handler, never of the identity line. A per-project `awareness-digest`
cron needs no `loop run`.

**Layer 1 — markdown × frontmatter commands with arguments** via the
existing `flywheel integration` generator. `/peer-aware` retro-primes
a running pane (register with the dispatcher + identity + handler).
Per-harness programmatic extensions stay deferred.

Labor: **mail = state and protocol; dispatcher = wake; template =
behavior; loop/swarm = dispatch.** Peer-collab works with no loop
because the dispatcher, not the captain loop, is on the peer path.

**Captain triggers (binding):** `/swarm` and `/loop` remain Layer 1
commands. They do **not** own wake. Spawn (or `/swarm`'s spawn step)
starts the dispatcher. `/loop` is bead dispatch (`loop run --work`:
claim, wave-gate, VERIFY). Skip the loop when the job is peer collab
only. Run the loop when beads need an owner. Both modes share Layer 0.

---

## 2. Background & Motivation

### 2.1 The trigger (captain's question, 2026-08-19)

"If crews always collab over toron mail by default, why do we need the
peer-aware slash command template? If I trigger /swarm or /loop, I can't
combine them with /peer-aware." — verified below: correct for every
harness's markdown-command mechanism.

### 2.2 Evidence: harness command mechanics (researched 2026-08-19)

| Harness | Command mechanism | Args? | State/chain? | Fire-and-forget? |
|---|---|---|---|---|
| opencode | `~/.config/opencode/commands/*.md` (plural — singular `command/` is dead), frontmatter (`description`, `agent`, `model`, `template`, `subtask`) — one expanded user message per invocation | `$ARGUMENTS` (unused by flywheel's commands today) | none | **yes** |
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

Two unprimed-pane clobbers (in-flight edits reverted twice by a peer
with no prime and no reservations) and a broken guard escape hatch
prove awareness cannot depend on the captain remembering a command.

A second proof, same day: **toron mail is pull.** Identity registered,
transport wired, and guard installed are invisible to a freshly
spawned pane. The template is the only client driver today: it
boot-primes the pin, the two planes, the self-poll, the conflict
ladder, presence, and the budget. Delete the template and
mail-by-default is infrastructure nobody speaks. Plane W (`toron
watch`) is already push on the **bus**. Bus-push is not LLM-push.
`herdr agent.prompt` is the only injection into a model.

Required always-on: identity prime (pin + guard attribution), a
**process** that rings the pane, reservation awareness at EDIT time
(kind 30031), presence publish, and a captain digest that does not
need `loop run`.

---

## 3. Decision

### D1 — Layer 0: always-on baseline (infrastructure, never command-triggered)

Every flywheel-spawned pane gets, at spawn time (launch args, ADR-0001
S.9.3 mechanism, plus one flywheel-owned dispatcher for the workspace):

1. **Identity prime**: "You are <pin>; mail identity; guard will block
   unattributed/unreserved commits." — ONE line minimum, present even
   with `--no-peer-aware` (single-shot panes still need guard
   attribution). A process cannot invent a name the model has never
   heard.

2. **Workspace doorbell dispatcher** (default-on; `--no-peer-aware`
   unregisters that pane from the dispatcher, never drops the identity
   line). One flywheel process per workspace, not N `toron watch`
   children. Ground: `toron watch` opens `demo_mailbox()` (plane.rs);
   N sidecars = N SQLite handles on the daemon root — the split-brain
   class ADR-0015 / bd-pin-public-plane-s-easy.14 just closed. Swarm
   cap is 5 crews (`swarm.md.tpl`). `InProcessBus` persist is sqlite,
   so one watcher that `collect_trusted`s the same XDG root sees
   daemon posts.

   **Plane S:** one `toron watch` (no `--once`) on kinds
   `9,7,43001,30031`. Flywheel reads NDJSON and fans out to the pin
   that matches `p` / assignee / holder.

   **Plane P:** the same dispatcher loop calls daemon `fetch_inbox`
   `--since` per registered pin (existing MCP tool, one mailbox).
   `trigger_tx` stays the workflow host's gift-wrap edge (in-process
   in `toron serve`). This ADR does **not** add a daemon→herdr socket.

   On a ring it calls `herdr agent.prompt` with **one line**:
   `[doorbell] kind 9. Pull: toron channel read h=<channel>`
   (or `toron job list` / `toron mail inbox --since`). Kind-9 `p` is
   npub hex; the dispatcher also matches `@pin` in content. The agent
   then pulls. Never push a gift-wrap body into context.

   **Ring:** `@pin` on kind 9; job 43001 for that pin; request-release
   to that pin; `importance` high|urgent on plane P; kind 30031 that
   conflicts with this pin's paths.

   **Do not ring:** room chatter, presence 20001, routine acks.

   **Budget:** the existing 5 / 30 / 3 knobs
   (`peer_poll_interval_minutes`, `peer_poll_budget_minutes`,
   `peer_max_prompts` in `SpawnOpts`) move onto the dispatcher, per
   pin. A process can enforce them; a prompt cannot. Retro-prime
   **inherits 5/30/3** (one set of knobs; a shorter recovery budget
   would kill peer-wake while the pane is still working).

   **Lifetime:** `cmd_spawn` is one-shot. The dispatcher is a
   **detached child** (pid file
   `~/.local/state/flywheel/doorbell/<workspace>.pid`). `flywheel
   kill` tears it down with the swarm. Spawn **fails** if it cannot
   start or attach. **Lazy self-heal:** a later spawn that finds a
   stale lock relaunches and re-registers pins from the spawn
   registry. Residual gap (crash → next spawn) is the handler
   safety-net pull, not a supervising loop and not a launchd unit.

   **Supervision signal:** the awareness-digest's first line is
   `dispatcher: up|down <since>`. A down dispatcher is a
   conflict-shaped event (deaf panes).

   A pane is registered with the dispatcher when its pin is bound.
   `/peer-aware` takes the same workspace lock: attach, never fork
   a second dispatcher.

   **Working pane:** queue the doorbell (same rule as interrupt:
   injecting into a working agent needs `--force`). Drain on
   `pane.agent_status_changed` working→idle|done (ADR-0001 event
   spine — subscribe). Poll `agent_list` **only** when subscribe is
   down. After inject, the pin has **120s** to show heard (presence
   20001 or kind-9/7 from its npub). Else digest
   `doorbell: unanswered pin=…` (deaf pane, same class as dispatcher
   down). Coalesce while queued:
   one `[doorbell]` line per event-class (channel / job /
   request-release / reservation-conflict / plane-P urgent), never
   one per ring.

   Plane-P sweep cadence: the same ~1s dispatcher loop, pins
   batched in one pass. A later sub-second plane-P ADR replaces
   the sweep implementation, not this structure.

3. **Handler template** (not a timer): planes S/P, conflict ladder
   (request-release → respond → escalate), wait-vs-collab table
   (presence + lease → defer / request-release / take over / share),
   "on `[doorbell]`, pull, then act." An optional 5-minute pull stays
   as a **safety net**, not the path.

4. Captain side: **`flywheel cron` awareness digest** (builtin
   `awareness-digest`), **per-project** YAML at
   `<project>/.flywheel/crons/` (ADR-0012 tier: per-project > global >
   built-in; `CronScope::Project`). Reservations, pins, and steer
   channels are project-scoped; a host-wide digest would mix graphs.
   Snapshot: presence + active reservations + expired request-release
   + guard denials → one kind-9 post + captain DM on conflict only.
   No `loop run` required. The loop remains WORK dispatch, not awareness.

**Labor split (binding):** mail = state and protocol; dispatcher = wake;
template = behavior; loop/swarm = dispatch. The dispatcher is the
peer-to-peer wake path. The captain loop is optional after spawn.

The daemon does **not** call herdr (toron ADR-0015: toron carries,
flywheel drives).

### D2 — Layer 1: markdown command surface with arguments (fallback + shape selection)

The command layer survives deliberately — the baseline is NOT 100%
trustworthy (2026-08-19 proves it), and unforeseeable events need a
manual trigger. Changes to the generator, not to harnesses:

1. **Orchestration commands become parameterized**: `swarm.md`/`loop.md`
   (and steer) gain `argument-hint` frontmatter + `$ARGUMENTS` passthrough
   (pi) / `$ARGUMENTS` (opencode), e.g. `/swarm peer-aware=off crews=3`.
   Arguments map onto existing flywheel CLI flags — the command text
   resolves them to a `flywheel` invocation; no new semantics live in prose.
2. **Standalone `/peer-aware` command**: retro-primes an ALREADY-RUNNING
   pane — register it with the workspace dispatcher, inject the identity
   line, load the handler. Accepts `--budget` overrides mapped to the
   three dispatcher knobs (default inherit 5/30/3). Recovery path for
   unprimed panes and manual spawns; the exact remedy for this session's
   clobber incident class. It does **not** ask the model to start polling.
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

### D5 — Captain with and without a loop (binding)

The captain has two live modes. They compose; neither is a fallback
hack.

| Mode | How the captain starts it | Loop? | What it is for |
|---|---|---|---|
| **Collab, no loop** | `flywheel spawn` (Layer 0). Optional `/swarm` for classify/lanes/brief. | **No.** | Peer wake is the dispatcher. Digest is cron. Captain `send` is optional steer. |
| **Bead dispatch** | `/loop` → `flywheel loop run <p> --work`, or `/swarm` that includes the loop step. | **Yes.** | Claim, wave-gate, VERIFY, reopen. Loop does **not** ring panes. |

`/swarm` playbook: spawn starts the doorbell. `loop run` is **optional**
and only for bead work. Saying "peer collab needs `/loop`" is a bug
after this ADR.

`/loop` playbook: still one pane, one objective, verified close. Inbox
wake is the dispatcher, not "poll every 5 min."

MCP-only sessions (no herdr pane) stay pull. Push is a pane feature.

---

## 4. Options Considered

| Option | Verdict | Why |
|---|---|---|
| A. Baseline-only (drop command layer) | rejected | baseline fails too (guard hatch, unprimed panes); unforeseeable events need a manual fallback |
| B. Baseline + markdown args (poll-in-template + `/peer-aware`) | **superseded by G** | first-draft chosen; still fire-and-forget — a model that skips the 5-min poll is deaf |
| C. Per-harness extension commands | deferred | max power, N APIs; pi/omp/pa only; revisit on proven need |
| D. Orchestrator-everything (awareness only via loop/swarm) | rejected | requires a running loop; 2026-08-19 incident had no loop |
| E. Inbox-push (dump `fetch_inbox` into context) | rejected | burns the window; leaks plane P |
| F. Daemon calls `herdr agent.prompt` | rejected | blurs toron ADR-0015; flywheel owns panes |
| G. Identity line + one workspace doorbell dispatcher + handler template + D2 commands | **chosen** | process cannot forget; one sqlite handle; pull stays the body; no loop required; no new MCP tools |

---

## 4.5 Debate (2026-08-19) — first draft → G

The first draft's Layer 0 was "always-on" only as **more prompt**. That
repeats the slash-command failure: awareness lives in text the model
may never execute. Plane W already pushes on the bus. The missing
wire is watch → `herdr agent.prompt`. Keep D2. Change D1's driver
from poll-in-template to sidecar-then-pull.

```diff
--- a/docs/decisions/0005-always-on-collab-baseline-and-command-surface.md
+++ b/docs/decisions/0005-always-on-collab-baseline-and-command-surface.md
@@ TL;DR
-Layer 0: an always-on baseline (identity prime, mail poll, reservation
-watch, guard attribution on EVERY flywheel-spawned pane + a flywheel
-cron / toron watch captain digest)
+Layer 0: identity prime in the prompt + a doorbell sidecar outside the
+LLM (toron watch → one-line herdr agent.prompt) + handler template.
+Agent pulls the body. Captain digest unchanged. Loop is dispatch.

@@ D1 item 2 (was "full peer prime = today's poll loop")
-2. Full peer prime (default-on, --no-peer-aware opts out of the LOOP,
-   never of the identity line): poll cadence, plane S/P, conflict
-   ladder, presence, budgets — today's peer-aware.md.tpl body.
-3. Reservation watch: add kind 30031 to toron watch --once --kinds
-   9,7,43001,20001,30031
+2. Doorbell sidecar (default-on; --no-peer-aware opts out of sidecar +
+   handler, never identity): process holds
+   toron watch --as <pin> --kinds 9,7,43001,30031 + plane-P edge.
+   Ring → herdr agent.prompt one [doorbell] line. Queue if working.
+   Budget 5/30/3 enforced by the sidecar. Kind 30031 is a ring rule,
+   not a prompt instruction.
+3. Handler template: planes, ladder, wait-vs-collab, "on [doorbell]
+   pull then act." 5-min pull is a safety net, not the path.

@@ D2 item 2 (/peer-aware)
-instructs the agent to adopt the identity line + poll loop +
-reservation watch NOW
+start (or attach) the doorbell sidecar, inject the identity line,
+load the handler. Does not ask the model to start polling.

@@ Implementation Step 1
-factor peer-aware.md.tpl into IDENTITY_LINE + POLL_LOOP
+factor peer-aware.md.tpl into IDENTITY_LINE + HANDLER
+(doorbell, ladder, wait-vs-collab). POLL_LOOP becomes optional
+safety-net, not the injected driver.
+New Step 1b: cmd_spawn starts the per-pin sidecar; spawn fails if
+it is down. Test: kind-9 / request-release → agent.prompt without
+the agent having polled.

@@ Non-goals
+No inbox-push. No new MCP tools. No daemon→herdr call.
```

Why G beats B: encode-lessons-in-structure. A dispatcher either runs or
spawn fails. A template poll is a suggestion. Why G beats F: pane
lifecycle stays flywheel. Why G beats E: gift-wrap stays pull.

Same-day close of §8 flipped the first G sketch (per-pin sidecar) to
**one workspace dispatcher**: N `toron watch` children would each call
`demo_mailbox()` on the daemon root.

---

## 4.6 Debate round 2 (2026-08-19) — G stands; three addenda proposed (CoralFinch)

Opening concession: the doorbell amendment wins round 1 outright — a
prompt-poll is a suggestion, a dispatcher is a mechanism
(encode-lessons-in-structure). Evidence re-verified:
`plane.rs` `demo_mailbox()` per CLI invocation + 100ms
`collect_trusted`; interrupt-working-needs-`--force` (5b08079); grok
prompts do not interpolate. The three diffs below are addenda, not
reversals — each names a gap G leaves unspecified where silent
failure returns.

```diff
@@ D1 item 2 — after "Spawn fails if the dispatcher is not up."
+   **Supervision (lazy self-heal):** any spawn retro-launches the
+   dispatcher when the workspace lock (pid file) is stale — crash →
+   next spawn boundary relaunches + re-registers pins from the spawn
+   registry. The awareness-digest's FIRST line reports dispatcher
+   liveness (`dispatcher: up|down <since>`) — a down dispatcher means
+   deaf panes, which is a conflict-shaped event, the digest's exact
+   job. Rejected: launchd/systemd units (host-specific — against the
+   harness-agnostic stance) and a supervising loop (re-introduces
+   "awareness needs a loop"). Residual gap between crash and next
+   spawn is covered by the handler's safety-net poll.
@@ D1 "Working pane" bullet
-   **Working pane:** queue the doorbell (same rule as interrupt:
-   injecting into a working agent needs `--force`). Drain on idle.
+   **Working pane:** queue the doorbell (same rule as interrupt:
+   injecting into a working agent needs `--force`). Drain signal =
+   `pane_agent_status_changed` working→idle on the ADR-0001 event
+   spine — the dispatcher SUBSCRIBES (it already reads the bus); it
+   never polls herdr for status. Coalesce while queued: one
+   `[doorbell]` line per event-class (channel / job / request-release
+   / reservation-conflict / plane-P urgent), never one per ring.
@@ D1 Plane P — after "calls daemon `fetch_inbox` `--since` per pin"
+   Cadence: the same ~1s dispatcher loop, pins batched in one pass
+   (fine at 5 crews). The later sub-second plane-P ADR replaces the
+   sweep implementation, not this structure.
@@ D2 item 2 (/peer-aware)
    retro-primes an ALREADY-RUNNING pane — register it with the
    workspace dispatcher, ...
+   Attach takes the same workspace lock as spawn: a second
+   retro-prime attaches to the live dispatcher, never forks a second
+   one (the .14 split-brain guard applies to recovery too).
@@ §7 Verification
+- [ ] dispatcher killed mid-flight → next spawn relaunches it (stale-lock path), digest line flips to `down`, no second dispatcher appears
+- [ ] queued doorbell drains on `pane_agent_status_changed` working→idle, coalesced per event-class (unit test on the queue)
```

Why these close real holes: an unsupervised dispatcher re-creates the
deaf-pane failure mode silently (G's own motivation); an unnamed idle
signal means every implementer invents one (polling herdr — the thing
the event spine exists to prevent); unspecified cadence + unguarded
attach are where the N-process split-brain sneaks back in during
recovery, the highest-risk path (/peer-aware exists precisely for
broken states).

---

## 5. Non-Goals

- No new mail transport, plane, or protocol (toron ADR-0020 covers the CLI surface).
- No new MCP tools (toron ADR-0007 C3 / ADR-0020).
- No inbox-push (bodies stay pull; plane P stays sealed).
- No daemon → herdr `agent.prompt` (toron ADR-0015).
- No change to the loop/swarm playbook CONTENT (wave gating, bead discipline).
- No interactive UI work (that is the extension revisit trigger, not this ADR).
- No per-harness behavioral forks — commands differ by frontmatter/dir only.

---

## 6. Implementation Plan

- **Affected paths**: `integrations/peer-aware.md.tpl`, `integrations/prompts/{swarm,loop,steer,peer-aware}.md.tpl`, `integrations/prompts/frontmatter-{opencode,pi,omp,grok,pa}.md.tpl`, `src/main.rs` (`cmd_integration`, `render_prompt`, `PROMPT_WORKFLOWS`/`ORCHESTRATION_COMMANDS`, `peer_prompt_template`, `cmd_spawn` dispatcher register), `src/pane.rs` (queue-if-working), a new doorbell dispatcher module (workspace-scoped), `src/cron.rs` + `<project>/.flywheel/crons/` (digest), `src/builtin_workflows.rs` (`awareness-digest`).
- **Step 1 — template split**: factor `peer-aware.md.tpl` into `IDENTITY_LINE` (always injected, even `--no-peer-aware`) + `HANDLER` (doorbell contract, planes, ladder, wait-vs-collab). Keep a short optional pull as safety net. Kind 30031 is a dispatcher ring rule and a handler line, not a 5-min chore.
- **Step 1b — doorbell dispatcher**: one detached flywheel-owned process per workspace (pid file; `flywheel kill` tears it down; spawn heals a stale lock). Plane S: one `toron watch` on `9,7,43001,30031`. Plane P: `fetch_inbox --since` per pin, ~1s batched loop (daemon MCP). Spawn fails if the dispatcher cannot start or attach. `--no-peer-aware` does not register the pane. Working-pane doorbells queue; drain on `pane.agent_status_changed` working→idle|done; coalesce per event-class. Rate-limit 5/30/3 per pin. Test: post kind 9 / request-release → `agent.prompt` fires without the agent having polled. Test: two pins, one watch child. Test: kill dispatcher → next spawn relaunches, no second process.
- **Step 2 — /peer-aware command**: new `peer-aware.md.tpl` rendering the retro-prime ("register with dispatcher; you are {pin}; on [doorbell] pull") into all five kinds via `flywheel integration`. Args include `--budget` → the three knobs.
- **Step 3 — parameterized orchestration commands**: per-kind adapters (closed in §8): opencode `$ARGUMENTS`; pi/omp `argument-hint` + `$1`/`$@`; grok `argument-hint` plus trailing-text convention (no interpolation in `~/.grok/prompts` today); pa same as pi when the dir exists. Generator tests assert the adapter for each kind, not a fake `$ARGUMENTS` on grok.
- **Step 4 — awareness digest**: builtin workflow `awareness-digest`; default cron YAML is **per-project** (`<project>/.flywheel/crons/`, `CronScope::Project`).
- **Step 5 — spawn minimal prime**: `cmd_spawn` injects IDENTITY_LINE for every pane; unit test: `--no-peer-aware` pane still carries the identity line and is **not** registered with the dispatcher.
- **Patterns**: templates stay the single source for **behavior**; the dispatcher is the single source for **wake**. Per-kind command differences live ONLY in `frontmatter-<kind>.tpl`. `// governed-by: ADR-0005` on touched files.

---

## 7. Verification (checkboxes)

Gate: `docs/decisions/_scripts/verify-0005.sh`. Live SC-D1 is
`sc_d1_live_kind9_channel_post_rings` (real `toron channel post --mention`
→ real `toron watch` NDJSON with npub `p` → `__doorbell --once`).
`herdr agent.prompt` is the same inject path the live dispatcher uses.

- [x] `docs/decisions/_scripts/verify-0005.sh` (D5 + §9 + templates + `doorbell` + `sc_doorbell_adr0005`)
- [x] Ring/queue core: `cargo test --bin flywheel doorbell::` (SC-D1/D2/D3)
- [x] Playbook contract: `cargo test --test sc_doorbell_adr0005` (SC-D1..D10 + D5)
- [x] herdr-mock: kind-9 `@pin` → `[doorbell]` inject, no `loop run` (`sc_d1_kind9_at_pin_becomes_doorbell_prompt`)
- [x] Live wire: real channel post `--mention` → watch NDJSON (npub `p`) → `[doorbell]` (`sc_d1_live_kind9_channel_post_rings`)
- [x] Heard-or-deaf: 120s with no presence/kind-9 from the pin → `doorbell: unanswered pin=…` (`HearTracker` + digest / `flywheel status` `unanswered`)
- [x] `flywheel integration` renders `/peer-aware` + argument-bearing `/swarm` `/loop` for all 5 kinds (`render_all_orchestration_commands_per_kind` + `integration_writes_orchestration_commands`)
- [x] `/peer-aware` attaches, never forks (`sc_d4_peer_aware_command_attaches`; dispatcher lock is `LockAction::Attach`)
- [x] `--no-peer-aware` spawn still injects `identity_prime` and does **not** start the dispatcher (`spawn_no_peer_aware_keeps_identity_skips_dispatcher` + `sc_d7_spawn_identity_without_dispatcher`)
- [x] One workspace = one `toron watch` child (`sc_d1_live_dispatcher_uses_apply_watch_line`)
- [x] Dispatcher watch kinds include 30031
- [x] Plane P: `toron mail inbox --since` per pin on the 1s tick (`doorbell_sweep_inbox` + `parse_inbox_item`)
- [x] Working pane: queue until working→idle|done; coalesce per class; herdr poll only if subscribe is down
- [x] Budget 5/30/3 enforced by `PinBudget` / `take_allowed`
- [x] Stale lock reloads; digest first line is `dispatcher: up|down` (`sc_d5_status_line_up_and_down` + `flywheel status` `dispatcher` field + `__doorbell --report`)
- [x] `flywheel kill` calls `doorbell::stop` (pid file gone, child SIGTERM)
- [x] `awareness-digest` is `CronScope::Project` (override at `<project>/.flywheel/crons/`); no `loop run`
- [x] `/swarm peer-aware=off` maps to `--no-peer-aware` (`swarm_peer_aware_off_maps_to_flag`)
- [x] Handler template is not a 5-min idle timer (`sc_d1_peer_template_is_handler_not_timer`)
- [x] grok `frontmatter-grok.md.tpl` does **not** claim `$ARGUMENTS` (`grok_frontmatter_does_not_claim_dollar_arguments`)

---

## 8. Open questions — closed (researched 2026-08-19)

Evidence: `toron-cli/src/plane.rs` `run_plane_command` → `demo_mailbox()`;
watch loop is `collect_trusted` every 100ms; `SpawnOpts` defaults 5/30/3
(`main.rs`); swarm cap 5 (`swarm.md.tpl`); cron tiers
`per-project > global > built-in` (`cron.rs` ADR-0012);
`trigger_tx` is in-process `UnwrappedMail` for `WorkflowHost` only
(`server.rs`); grok `~/.grok/prompts` frontmatter has no arg
interpolation (skills take trailing text; `frontmatter-grok.md.tpl`
is `prompt_mode` only); pi/omp already have `argument-hint`.

| # | Decision | Why |
|---|---|---|
| 1 | **One dispatcher per workspace**, not N per-pin sidecars | N `toron watch` = N `demo_mailbox()` on the daemon root (the .14 split-brain). One watch child + fan-out. Isolation is per-pin **queue/budget**, not per-pin process. |
| 2 | Retro-prime **inherits 5/30/3** | `SpawnOpts` already owns the knobs. A shorter recovery budget would silence peer-wake mid-task. Override is Layer 1 (`--budget`), not a second default. |
| 3 | Digest is **per-project** (`<project>/.flywheel/crons/`, `CronScope::Project`) | Pins, reservations, and steer channels are project-scoped. Host-wide mix is a lie. Matches ADR-0012 precedence. |
| 4 | Adapters: **opencode `$ARGUMENTS`**; **pi/omp `argument-hint` + `$1`/`$@`**; **grok `argument-hint` + trailing-text convention, no fake `$ARGUMENTS`**; **pa = pi** when the dir exists | Grok prompts are not skills. Inventing interpolation would fail closed. Pi/omp already interpolate. |
| 5 | **`/peer-aware` accepts `--budget`** | Maps to the three dispatcher knobs. Default inherit. Recovery that wants `max_peer_prompts=1` can say so. |
| 6 | Plane P = **dispatcher `fetch_inbox --since` per pin** on the same loop. `trigger_tx` stays workflows. **No daemon→herdr socket in this ADR** | Existing MCP tool, one mailbox, ADR-0015 intact. Sub-second P-wake is a later ADR if 1s sweep is too slow (not needed at 5 crews). |

---

## 9. Scenarios (live gate)

Each row is a named test. CLI substrate (no flywheel) stays toron
SC-27 / SC-32. Flywheel contract is `tests/sc_doorbell_adr0005.rs` +
`src/doorbell.rs` + `docs/decisions/_scripts/verify-0005.sh`.

| ID | Story | Expect | Gate |
|---|---|---|---|
| SC-D1 | No loop: LilacChapel posts kind 9 `@BlueRidge` | BlueRidge gets one `[doorbell]` line; pulls; no `loop run` | `sc_d1_live_kind9_channel_post_rings` (real watch NDJSON) |
| SC-D2 | File conflict: request-release → respond → transfer | Ring holder on plane P; grant is transfer; guard blocks unheld `git add` | `parse_inbox_item` + toron reserve/guard tests |
| SC-D3 | Working pane | Queue; drain on `pane.agent_status_changed` working→idle\|done; one line per class | `DoorbellQueue` + `should_inject` + `apply_status_change` |
| SC-D4 | `/peer-aware` on an unprimed pane | Attach to live dispatcher (same lock); identity line; no second process | `sc_d4_peer_aware_command_attaches` + `LockAction::Attach` |
| SC-D5 | Dispatcher crash | Digest `dispatcher: down`; next spawn heals stale pid; no fork | `dispatcher_status_line` + `inspect_lock` Reloaded |
| SC-D6 | Captain without loop | `awareness-digest` cron, no `loop run` | `sc_d6_digest_needs_no_loop` |
| SC-D7 | `--no-peer-aware` | Identity line present; pane not registered | `identity_prime` + `sc_d7_spawn_identity_without_dispatcher` |
| SC-D8 | MCP-only session | Stay pull; no herdr prompt | non-goal; no doorbell without a pane |
| SC-D9 | `/swarm` without `/loop` | Spawn + doorbell; peer collab works; loop optional | `swarm.md.tpl` must not require `loop run` for peer wake |
| SC-D10 | `/loop` | Bead claim/VERIFY/wave-gate; wake is still the dispatcher | `loop.md.tpl` must not teach 5-min inbox poll as the driver |

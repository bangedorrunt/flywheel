# Peer-aware agent template

The dispatcher rings `[doorbell]`. This template is the handler: identity,
coordination planes, conflict handling, and pull-then-act. It is not a timer
and does not create additional orchestration.

The handler never outranks a captain dispatch. It only wakes the agent and
explains how to coordinate; it does not create scope, assign implementation
work, or authorize bead closure.

## Template (injected into the agent's session on spawn by default; `--no-peer-aware` opts out of the handler, never of the identity line)

```
OPERATING CONTRACT

The captain's current dispatch is authoritative. This peer-aware text is only
an always-on wake and coordination protocol; it is not a task and must not
replace the captain's instructions.

Priority:
1. Continue the current captain-assigned task.
2. Handle urgent captain messages, reservation conflicts, and request-release
   events.
3. Handle ordinary peer work only when it does not conflict with the captain's
   task.
4. Ignore unrelated room chatter.

If no captain task exists, wait for a captain dispatch or `[doorbell]`; do not
invent implementation work.

GS-039 P0-2 OPEN-MODE / OPERATOR-RAW EXCEPTION: when the current pane is in
open-mode, or when flywheel injects an operator/`--raw` objective marked
AUTHORITATIVE (including create-file / edit / report tasks), that text
OUTRANKS this peer-aware contract and outranks any stale diagnose/captain
TASK-ID still in scrollback. Execute it. Do not classify it as unrelated
room chatter. Do not idle awaiting a new captain dispatch.

PUBLIC ADDRESS: {mail_identity}
INTERNAL PANE HANDLE: {name}

Use PUBLIC ADDRESS for Toron, mail, presence, reservations, guards, and
`flywheel --to`. Never use the internal pane handle for those operations.

GS-039 P1 AGENT-LOCAL MAIL: prefer `flywheel mail … --as <PUBLIC ADDRESS>`
from this pane (CLI pin plane). Raw `toron mail --as <pin>` needs the
project-local key minted at spawn; if you see "unknown agent" / "no local
key", re-run via flywheel mail or `flywheel mail register --name <pin>` —
do not invent a slug identity.

Captain task at boot: none assigned in this peer-awareness message.

Two coordination planes:
- Plane S (shared, kind 9 rooms + jobs + presence): crew-visible work and
  peer wake. Room: `toron channel create|join|post|read`. Job:
  `toron job list|claim|result`. Presence: `toron presence publish --as
  {mail_identity} --state idle|working|blocked|done`.
- Plane P (private, NIP-17 gift-wrap): secrets and 1:1 only. `toron mail
  send|inbox|reply`. Pull with `toron mail inbox --since last`
  (since_ts high-water; cursor `since_ts`/`created_ts`, never unread).
  The daemon persists the incremental cursor; read-then-bump is atomic and
crash-safe.
  `unread_only` remains a documented no-op.

Membership is the only gate: `channel read`
works without membership, `post` does not -- and a non-member post FAILS
LOUDLY (nonzero exit, no event id). STEP 0, before any doorbell work:
join your lane channels -- `toron channel join <channel> --project
<project> --as {mail_identity}` (idempotent). A bounce names this exact
remedy.

File conflict ladder: soft conflict (want a held path) -> `toron reserve
request-release --target <holder-pin> --ask release|defer:<secs>` (plane P)
-> holder answers `toron reserve respond --decision
agreed|denied|deferred:<secs>` -> escalate after timeout on the steer
channel (kind 9), captain last. Never DM for a sweep; the guard is the
control. A grant is a lease transfer, never commit-on-behalf.

On `[doorbell]` (the dispatcher injects this line; do not start a watch
timer):
  0. JOIN first (above) if you have not already -- posting without
     membership is a refused write. Better: JOIN and publish
     `presence --state idle` at BOOT, not at first doorbell -- an arrived
     crew is discoverable by roster and presence from minute one.
  1. FIRST, show you heard (120s or you are unanswered): `toron presence
     publish --as {mail_identity} --state working` OR one kind-9 line.
  2. Pull: `toron channel read` / `toron job list` / `toron mail inbox
     --since last` (plane P only).
  3. For each URGENT or doorbell-named item:
       - reply/ack if plane P requires it
       - do the work, or escalate via kind 9 / captain mail
  4. acknowledge_message on each ack_required message you have acted on

Verify-after-send: after any load-bearing post, confirm
delivery ONCE -- `toron channel post ... --wait-ack 5` reads back the
post's own event id and exits nonzero if it never lands. One read-back,
never a poll loop. Delivery is claimed only after it is observed.

Delivery visibility: "did X report?" is ONE query --
`toron channel read <channel> --project <p> --as {mail_identity}
--author <pin>`: delivered_posts == 0 means X has NOT reported.

Safety-net (not the driver): if no doorbell arrives for
{poll_interval_minutes} minutes of idle AND you are waiting on a peer,
one `inbox --since` / `channel read` is allowed. The dispatcher is the
path; this covers crash-to-next-spawn only.

Continue your primary task. Exit when the primary task is done OR your
context budget is reached.

ON COMPLETION (every crew, not just bead crews): mail the
orchestrator/captain on the TASK thread -- the TASK-ID line at the top of
your dispatch IS the thread anchor :
`flywheel mail send --thread-id <task-id> --subject '[<task-id>] done'`
with a BODY carrying the report fields:
status, commit sha, verdict kind, what you ran + the result tail,
deviations, PRINCIPLES cited. Bead-closed + commit are the durable
signal; this mail is courtesy. Same for 'starting' when you claim a task.

Bounded budget: max {poll_budget_minutes} minutes of
peer-wake AND max {max_peer_prompts} peer-prompts per task (enforced by
the dispatcher). On budget exhaustion, stop taking doorbells. Do NOT
spawn or close panes -- only the orchestrator owns lifecycle. You may
prompt existing peers, never create them.
```

## Invariants

- **Peer-triggered is default-on** (`flywheel spawn --profile <role>`).
  `--no-peer-aware` opts out of dispatcher registration + this handler;
  the identity line still injects.
- **Budget is mandatory** when enabled: `poll_interval_minutes` (default 5,
  safety-net only), `poll_budget_minutes` (default 30), `max_peer_prompts`
  (default 3). Enforced by the dispatcher, not by hoping the model polls.
- **Safety**: a peer-aware agent may `herdr agent prompt` an existing peer
  but must NEVER `pane split`/`agent start`/`close`.
- **`--since last` incremental fetch** keeps the plane-P pull cheap and
  crash-proof (the cursor lives server-side, not in model memory).
  `unread_only` is a no-op and is never taught as a filter.
- **File-first deliverables** (bd-tdjaz): any finding over ~20 lines is
  written to a durable file FIRST; the channel post carries a summary +
  the file path (prefer toron `attachment_paths`). File-first is also the
  silent-failure recovery path -- redeliver from the file, never from
  scrollback.
- **Address pins by exact identity, never by roster name-similarity.**
  Roster entries can be stale session artifacts; an unknown pin gets a
  `whois` + recency check before it is addressed. The `captain` alias, when
  bound, is the only title-based address.
- **Plane S is the room, plane P is the whisper.**
- **`[doorbell]` is the wake.** A 5-minute self-watch loop is not the
  driver.

# Peer-aware agent template

Arrive then brief before any spawn: `flywheel arrive <project> --as <pin>` then `flywheel briefing "<objective>" --json`. Do not start a second swarm when the world is joinable.

The dispatcher rings `[doorbell]`. This template is the handler: identity, contract, and pull-then-act. It is not a timer and creates no orchestration.

## Template (injected into the agent's session on spawn by default; `--no-peer-aware` opts out of the handler, never of the identity line)

```
OPERATING CONTRACT

The captain's current dispatch is authoritative. This text is an always-on wake and coordination protocol, never a task: continue the captain's work, handle urgent captain messages and reservation conflicts, take ordinary peer work only when it does not conflict, ignore room chatter. If no captain task exists, do not invent implementation work; wait for dispatch or `[doorbell]`.

OPEN-MODE EXCEPTION: an operator/`--raw` objective marked AUTHORITATIVE outranks this contract and any stale TASK-ID in scrollback. Execute it.

CONTROL-PLANE MARKER: prompts prefixed with U+2063 + `FLYWHEEL_OP:` are lifecycle events (relaunch, drain), never implementation work. Re-anchor and re-ack the doorbell. Captain stage payloads stay UNMARKED: unmarked text is work.

PUBLIC ADDRESS: {mail_identity}
INTERNAL PANE HANDLE: {name}

Use PUBLIC ADDRESS for toron, mail, presence, reservations, guards, and `flywheel --to`. Never the pane handle.

BOOT: join lane channels first (`toron channel join <channel> --project <project> --as {mail_identity}`, idempotent; a non-member post FAILS LOUDLY). Publish `toron presence publish --as {mail_identity} --state idle`.

On `[doorbell]`: 1. show you heard within 120s or you are unanswered: presence `working` or one kind-9 line. 2. Pull: `toron channel read` / `toron job list` / `toron mail inbox --since last` (since_ts high-water, never unread). 3. Act on URGENT and doorbell-named items (reply/ack plane P as required), escalate the rest via kind 9 or captain mail. 4. Acknowledge each ack-required message acted on. A duplicate ring is the same delivery, never second work.

ACK-DUTY (the delivery contract): the toron message IS the delivery, the `[doorbell]` line is only the wake. On a ring: `fetch_inbox` (or `toron mail inbox --since last`), process in rumor order, act, then `acknowledge_message` / `mark_message_read`. The ack is the handling fact -- an unacked corr re-rings on the ladder (one ring per grace window, then Dead->respawn / Unknown->digest).

ID DISCIPLINE: the TASK-ID is routing metadata only, never the work order and never a `br show` argument. The work order is the BEAD id (bd-###) named in a BEAD line of your dispatch; with no BEAD line bound, do not act, ask the captain.

ASK-USER AUTHORITY (worker never self-answer): a finding that asks the user a question is never answered by the worker that raised it. Reconstruct accepted intent from the brief (GOAL / SCOPE / ACCEPTANCE). RESTORATION -- an unambiguous restoration of accepted behavior, including hard but in-scope corrections -- may be auto-decided: Publish the corr (journaled) and proceed. EXPANSION (new guarantee, threat model, architecture, destructive, security): STOP, Publish the corr, park on WaitCorr, and escalate with five elements: (1) original criterion, (2) proposed expansion, (3) smallest compliant alternative, (4) accept-vs-decline consequences, (5) recommendation. Expansions stay human; the open call parks in OPEN DECISIONS until an answer/--verdict clears it. Worker-side auto-close of an ask-user finding is a LoudMiss, never a close.

Verify-after-send: after any load-bearing post, one read-back (`--wait-ack 5`); delivery is claimed only after it is observed. One read-back, never a poll loop.

WAKE VERBS (kind-9 is a wake log, not current state): post sparse verbs only -- `blocked: <what> [key=...]` / `paused: <which-human> <why>` / `needs-decision [key=...] <question>` / `resolved [key=...] <outcome>`. Never percent-done or activity prose ("working on X", "update:"). Current state = presence verdict + `br show`; the room carries wakes, not status.

RED-SURFACE PING: if your work leaves the tree non-compiling, post one project-mail line (`<pin>: <path> red at <error> -- fixing by <time>`) within one poll cycle; re-ping while it stays red.

Safety-net (not the driver): if no doorbell arrives for {poll_interval_minutes} minutes of idle AND you wait on a peer, one `inbox --since` / `channel read` is allowed. Crash-to-next-spawn cover only.

ON COMPLETION: mail the orchestrator on the TASK thread (`--thread-id <task-id>`, subject `[<task-id>] done`): status, commit sha, verdict, commands run plus result tail, deviations, PRINCIPLES cited.

Budget: max {poll_budget_minutes} minutes of peer-wake and {max_peer_prompts} prompts per task, dispatcher-enforced. Never spawn, close, or split panes; prompt existing peers only.
```

## Invariants

- **Peer-triggered is default-on** (`flywheel spawn --profile <role>`). `--no-peer-aware` opts out of dispatcher registration + this handler, never of the identity line.
- **`[doorbell]` is the wake.** A self-watch loop is not the driver.

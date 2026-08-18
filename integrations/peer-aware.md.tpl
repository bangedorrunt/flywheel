# Peer-aware agent template (bd-pattern-peer-triggered)

The self-poll loop lives INSIDE the spawned agent (ADR-0001 S.9.3). This
template is the agent-side instruction: while working its primary task, the
agent periodically checks for peer work and acts on it -- waking peers,
replying, or escalating. No orchestrator in the peer-to-peer path.

## Template (injected into the agent's session on spawn by default; `--no-peer-aware` opts out)

```
You are {name}. Primary task: {task}.

Your mail identity is {mail_identity} -- the pin peers and the orchestrator
address you by (use it for `--as` / `--to` / reserve / guard / presence).
Your herdr name is the internal pane handle; never type it as `--as` or
`--to`.

Two planes (ADR-0020):
- Plane S (shared, kind 9 rooms + jobs + presence): crew-visible work and
  peer wake. Room: `toron channel create|join|post|read`. Job:
  `toron job list|claim|result`. Wake: `toron watch --once
  --kinds 9,7,43001,20001`. Presence: `toron presence publish --as
  {mail_identity} --state idle|working|blocked|done`.
- Plane P (private, NIP-17 gift-wrap): secrets and 1:1 only. `toron mail
  send|inbox|reply`. `unread_only` is a documented no-op -- poll by
  `since_ts` high-water.

File conflict ladder: soft conflict (want a held path) -> `toron reserve
request-release --target <holder-pin> --ask release|defer:<secs>` (plane P)
-> holder answers `toron reserve respond --decision
agreed|denied|deferred:<secs>` -> escalate after timeout on the steer
channel (kind 9), captain last. Never DM for a sweep; the guard is the
control.

Peer-awareness loop (run ON TOP of your primary task):
  While working, on completing a step OR every {poll_interval_minutes} minutes
  of idle (whichever comes first):
    1. `toron watch --once --kinds 9,7,43001,20001` for crew-visible work;
       `toron mail inbox --since <last_poll>` for private 1:1 (plane P only).
    2. For each URGENT message from a peer:
         - reply(P, ack) acknowledging you received it
         - if P needs help you can give: herdr agent prompt P '<help>' --wait
         - else: escalate via toron mail to the orchestrator/human
    3. acknowledge_message on each ack_required message you have acted on
    4. update <last_poll> to the latest message timestamp you processed
    5. publish presence for the state you are in (working/done), so peers
       and the orchestrator see liveness without polling mail.

  Continue your primary task. Exit when the primary task is done OR your
  context budget is reached.

  ON COMPLETION (every crew, not just bead crews): mail the
  orchestrator/captain on the TASK thread (thread_id=<task-id>) with
  subject '[<task-id>] done' and a BODY carrying the report fields:
  status, commit sha, verdict kind, what you ran + the result tail,
  deviations, PRINCIPLES cited. The captain has no push channel; bead-closed + commit are the
  durable signal; this mail is the courtesy that tells them without
  polling. Same for 'starting' when you claim a task.

Bounded budget (non-optional): max {poll_budget_minutes} minutes alive AND
max {max_peer_prompts} peer-prompts per task. On budget exhaustion, exit
cleanly (never crash). Do NOT spawn or close panes -- only the orchestrator
owns lifecycle. You may prompt existing peers, never create them.
```

## Invariants

- **Peer-triggered is default-on** (`flywheel spawn --profile <role>`); most
  tasks want orchestrator direction. `--no-peer-aware` opts out for
  single-shot panes.
- **Budget is mandatory** when enabled: `poll_interval_minutes` (default 5),
  `poll_budget_minutes` (default 30), `max_peer_prompts` (default 3). Bounds
  the cascade risk (A triggers B triggers A...) + the runaway-poll risk.
- **Safety**: a peer-aware agent may `herdr agent prompt` an existing peer but
  must NEVER `pane split`/`agent start`/`close` -- the orchestrator owns
  lifecycle. Enforced by the template text.
- **`since_ts` incremental fetch** keeps the plane-P poll cheap (only new
  messages). `unread_only` is a no-op and is never taught as a filter.
- **Plane S is the room, plane P is the whisper** (boundary-discipline):
  crew-visible work and peer wake go through channel/watch/job; gift-wrap
  stays for secrets and 1:1.
- This is the HIGHEST-VARIANCE pattern (agents deciding to involve peers can
  cascade) -- the bounded budget is the control.

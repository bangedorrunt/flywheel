# Peer-aware agent template (bd-pattern-peer-triggered)

The self-poll loop lives INSIDE the spawned agent (ADR-0001 §9.3). This
template is the agent-side instruction: while working its primary task, the
agent periodically checks its own agent-mail inbox and acts on urgent messages
from peers — waking them, replying, or escalating. No orchestrator in the
peer-to-peer path.

## Template (injected into the agent's session via `--peer-aware`)

```
You are {name}. Primary task: {task}.

Peer-awareness loop (run ON TOP of your primary task):
  While working, on completing a step OR every {poll_interval_minutes} minutes
  of idle (whichever comes first):
    1. fetch_inbox(self, unread_only=true, since_ts=<last_poll>) → new messages
    2. For each URGENT message from a peer:
         - reply(P, ack) acknowledging you received it
         - if P needs help you can give: herdr agent prompt P '<help>' --wait
         - else: escalate via agent-mail to the orchestrator/human
    3. acknowledge_message on each ack_required message you have acted on
    4. update <last_poll> to the latest message timestamp you processed

  Continue your primary task. Exit when the primary task is done OR your
  context budget is reached.

Bounded budget (non-optional): max {poll_budget_minutes} minutes alive AND
max {max_peer_prompts} peer-prompts per task. On budget exhaustion, exit
cleanly (never crash). Do NOT spawn or close panes — only the orchestrator
owns lifecycle. You may prompt existing peers, never create them.
```

## Invariants

- **Peer-triggered is opt-in** (`flywheel spawn --peer-aware`); most tasks
  want orchestrator direction. Default OFF.
- **Budget is mandatory** when enabled: `poll_interval_minutes` (default 5),
  `poll_budget_minutes` (default 30), `max_peer_prompts` (default 3). Bounds
  the cascade risk (A triggers B triggers A...) + the runaway-poll risk.
- **Safety**: a peer-aware agent may `herdr agent prompt` an existing peer but
  must NEVER `pane split`/`agent start`/`close` — the orchestrator owns
  lifecycle. Enforced by the template text.
- **`since_ts` incremental fetch** keeps the poll cheap (only new messages),
  per bd-mail-ack-sla-tracking.
- This is the HIGHEST-VARIANCE pattern (agents deciding to involve peers can
  cascade) — the bounded budget is the control.

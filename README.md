# flywheel

Multi-agent swarm orchestrator on the **herdr** backend — an NTM-parity daily loop
for opencode / pi / grok. Spawn swarms, dispatch the daily-driver stages
(P1→P8), monitor states, checkpoint progress. Tracker state lives in **br/bv**,
coordination in **agent-mail**, orchestration in herdr.

## Install

```bash
cargo install --path . --force
# or: cp target/release/flywheel ~/.local/bin/flywheel
```

Config: `~/.config/flywheel/config.toml` (see `config.example.toml`).
Requires a running herdr server (herdr.sock) — `flywheel deps` checks everything.

## The daily loop

```bash
flywheel spawn notetoself --oc=1 --pi=1 --gk=1   # 3-agent swarm
flywheel loop notetoself --queue p1,p2,p4,p5,p6,p8   # durable orchestrator
```

`flywheel loop` is the monitoring orchestrator: it polls the swarm every N
seconds (default 300, `--interval`), feeds the queued instructions to
idle/finished agents, reports blockers (`--on-blocked continue|stop|mail`),
and exits 0 when the queue drains and the swarm settles. Idle agents with an
empty queue get an optional `--nudge`.

Two work sources:
- `--queue p1,p2,p4,p5,p6,p8` — static stages/messages (the daily driver)
- `--work` — **bead-driven**: each poll sources work from `br ready --json` and
  feeds idle agents "claim and execute bead bd-N" prompts until the plan is
  exhausted (dependencies unblock new beads naturally). Use this after a
  `/swarm` launch.

Work-mode dynamics:
- **interrupt recovery** — beads left `in_progress` by an interrupted/network-
  dropped agent are re-prompted with a resume instruction before fresh beads;
  the loop stays alive while any bead is stranded
- **context respawn** — before delegating to an idle agent, the loop checks its
  recent output for the `CONTEXT_LIMIT` marker (the bead prompt tells agents to
  emit it past ~80% context); if found, the agent's pane is replaced by a fresh
  instance under the same name and the queue item is handed to it

### Durability (Resonate)

The loop runs as a **durable Resonate workflow** (requires a Resonate server,
default `http://localhost:8001` — installed as the launchd service
`com.flywheel.resonate`; sqlite state survives reboots).
Every poll, send, and sleep is a checkpoint: kill the loop process mid-run and
`flywheel loop <project>` with the same flags **resumes the same invocation**
from its last checkpoint. State is queryable via `flywheel loop-status
<project>`. `--local` runs the plain sync loop instead; `--once` does a single
poll pass (cron-friendly).

The same sequence as a one-shot workflow:
`flywheel workflows run daily notetoself`
(stages in `~/.config/flywheel/workflows.toml`).

## Command map

| Group | Commands |
|---|---|
| Swarm lifecycle | `init spawn add list status kill attach zoom` |
| Operator loop | `send interrupt view watch work loop loop-status` |
| Assignment | `assign` (agent-mail bead dispatch) |
| Coordination | `mail inbox/send/locks/unlock/conflicts/agents` |
| Durability | `checkpoint timeline audit history changes resume analytics` |
| Assets | `recipes workflows pipeline template ensemble` |
| Safety | `safety guards policy` |
| Ops | `deps config upgrade tutorial serve` |
| Automation | `--robot-snapshot --robot-status --robot-capabilities` (+ `--json` everywhere) |

## Durable backend

The loop workflow lives in `src/durable.rs`: `swarm_loop` (workflow) +
`poll_swarm`/`send_prompt`/`notify_blocked` (leaves that shell out to the
flywheel binary's hidden `__poll`/`__send`/`__mail-blocked` subcommands).
Built against `resonate-sdk` 0.6.0. Note: 0.6.0 does not support version-based
re-invocation, so fresh runs get fresh invocation ids
(`swarm-loop:<project>:<ts>`), persisted in `~/.local/state/flywheel/loops.json`.

## Backend protocol

herdr socket protocol v18: one JSON-RPC request per connection (newline-delimited),
`events.subscribe` keeps a long-lived stream. See `src/herdr.rs`.

## Exit codes

0 ok · 1 herdr/server error · 2 usage error. stdout = data, stderr = messages,
`--json` for machine output (color auto-disabled when piped / `NO_COLOR`).

## Roadmap (v2)

- checkpoint → respawn fidelity (store spawn spec), timeline rendering polish
- policy approval workflow (prompt-based approvals, not just deny-guards)
- `serve` SSE on real herdr events (currently state-poll stream)

Feedback: open an issue at https://github.com/bangedorrunt/flywheel/issues

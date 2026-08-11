# flywheel

Multi-agent swarm orchestrator with **pluggable backends** (default: **herdr**
multiplexer + **toron** coordination) — an NTM-parity daily loop for
opencode / pi / grok. Spawn swarms, dispatch the daily-driver stages (P1→P8),
monitor states, checkpoint progress. Tracker: **br/bv**.

## Install

```bash
cargo install --path . --force
# or: cp target/release/flywheel ~/.local/bin/flywheel
```

Config: `~/.config/flywheel/config.toml` (see `config.example.toml`).
Copy [`.env.example`](.env.example) → `.env` for local backend overrides
(`.env` is gitignored). Default stack needs a running herdr server
(herdr.sock) — `flywheel deps` checks everything.

## Backends

flywheel selects a **multiplexer** (pane/agent lifecycle) and a **mail**
fabric via env / `.env` / config (ADR-0002). Defaults: `herdr` + `toron`.
Mail backends (ADR-0003): `toron` = the toron facade (`[mail.toron]`, absent
→ embedded in-process); `agentmail` = the real mcp_agent_mail server
(`[mail.agentmail]`, default `http://127.0.0.1:8765`). Stubs: `superlogical` /
`tmux` (mux).

```bash
flywheel backend list --json    # what is compiled in + active
flywheel backend status         # config source + health
# FLYWHEEL_BACKEND=herdr FLYWHEEL_MAIL=toron
# FLYWHEEL_MAIL=agentmail       # real agent-mail server ([mail.agentmail])
```

Precedence: flag > env > `.env` > config.toml > default. Full matrix and
migration path (R0→R2): `AGENTS.md` § Backends, `docs/decisions/0002-*.md`.

## The daily loop

```bash
flywheel spawn notetoself --oc=1 --pi=1 --gk=1   # 3-agent swarm
flywheel loop run notetoself --queue p1,p2,p4,p5,p6,p8   # durable orchestrator
```

`flywheel loop run` is the monitoring orchestrator: it polls the swarm every N
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

### Agent state files

Context exhaustion is a soft signal the agent decides (~80% session usage).
Terminal reads can silently miss content for full-screen agents (alternate
screen), so the loop also trusts an advisory **agent state file** written by
the agent itself:

```
<project>/.flywheel/agent-state/<agent-name>.json
{ "context_pct": 82, "ts": "2026-08-03T03:00:00Z" }
```

- `context_pct` — 0..=100; the agent writes the file when its session context
  reaches ~80%, before replying `CONTEXT_LIMIT`
- `ts` — RFC3339 write time (same shape as the loop's own timestamps)
- advisory: the agent never blocks its own work on the write
- freshness — a file older than **15 minutes** is treated as stale (previous
  session) and must never trigger a respawn
- tidy-up — `flywheel kill` removes the project's `.flywheel/` state
  directory; `.flywheel/` is git-ignored

Project-relative (not `~/.local/state`): agents write from inside the repo
without shell tricks.

### Durability (Resonate)

The loop runs as a **durable Resonate workflow** (requires a Resonate server,
default `http://localhost:8001` — installed as the launchd service
`com.flywheel.resonate`; sqlite state survives reboots).
Every poll, send, and sleep is a checkpoint: kill the loop process mid-run and
`flywheel loop run <project>` with the same flags **resumes the same
invocation** from its last checkpoint (the bare legacy form
`flywheel loop <project>` still works). State is queryable via
`flywheel loop status <project>`; `flywheel loop list` shows the live-loop
pid registry and `flywheel loop prune` cleans dead entries. `--local` runs the
plain sync loop instead; `--once` does a single poll pass (cron-friendly).

#### Poll cadence (durable workflow)

The loop has two cadences, both in seconds:

- `--interval N` (default 300) — the **settled** cadence, used when nobody is
  working. This is the pre-adaptive meaning and stays backward compatible.
- `--interval-active N` (default 30, floor 5 — values below clamp with a
  warning) — the **active** cadence, used while any agent is
  working/blocked/unknown **or right after a dispatch**. The blocked-alert
  latency bound (`--on-blocked mail`) is ≤ one active interval; a lower
  `--interval-active` trades socket load for faster reaction per project.
- The sync loop (`--local`/`--once`) polls flat at `--interval` — the active
  cadence is durable-workflow-only.
- `--timeout-ms` interplay: the durable workflow has no wall clock, so the
  timeout becomes an ITERATION cap (`max_iterations`). With a faster active
  cadence the wall-clock deadline is reached sooner — intended; tune
  `--timeout-ms` accordingly.

The same sequence as a one-shot workflow:
`flywheel workflows run daily notetoself`
(stages in `~/.config/flywheel/workflows.toml`).

## Command map

| Group | Commands |
|---|---|
| Swarm lifecycle | `init spawn add list status kill attach zoom` |
| Operator loop | `send interrupt view watch work loop run/status/list/prune` |
| Assignment | `assign` (agent-mail bead dispatch) |
| Coordination | `mail inbox/send/locks/unlock/conflicts/agents` + `contact` + `product` |
| Patterns | `debate`, `plan-review`, `xproj` (cross-project fan-out) |
| Orchestration (ADR-0005/0010/0011/0012) | `workflow {new,lint,run,list,show,history,crew,remove}` · `cron {lint,run,list,show,pause,resume,status,history,start,remove}` (standalone YAMLs) · `coalition {form,status,list,intent,reserve,negotiate-*,detect,auto-coalesce,disband}` · `squad` · `profile` · `model` · `stage` · `runs` |
| Durability | `checkpoint timeline audit history changes resume analytics` |
| Legacy assets | `recipes workflows pipeline template ensemble` (absorbed into the unified vocabulary above) |
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

### Blocking on pane output (`__wait-output`)

`flywheel __wait-output <project> <agent-or-pane> <pattern> [--regex] [--timeout-ms N]`
blocks until the target's terminal output matches the pattern (literal
substring by default; `--regex` for a herdr-side regex) or the timeout elapses.
The target is an agent name (resolved via the project's agent list) or a raw
pane id (`w1:p1`) that lives in the swarm's workspace. stdout carries
`{"matched", "matched_line", "pane_id"}`; exit 0 on match, 1 on timeout or
unknown target.

Recipe — block on a test run:

```sh
herdr pane run w1:p1 'cargo test'          # start the watcher in a pane
flywheel __wait-output proj w1:p1 --regex 'passed|failed' --timeout-ms 120000
# …script continues the moment the run prints a verdict
```

Caveat: `__wait-output` waits on raw pane OUTPUT, not agent lifecycle. Use
`herdr agent wait` when you need lifecycle semantics (idle/working/blocked);
use `__wait-output` when a specific line appearing is the signal.

### Beads subagent integration

Bead/issue work in a swarm is delegated to a generated `beads` SUBAGENT per
tool (not an AGENTS.md section). `flywheel integration <kind>` renders the
subagent definition from `integrations/{opencode,pi,grok}-beads.md.tpl` (shared
body + per-kind frontmatter), version-stamped with the flywheel build:

```sh
flywheel integration opencode --out ~/.config/opencode/agents/beads.md
flywheel integration pi         --out ~/.pi/agent/agents/beads.md
flywheel integration grok       --out ~/.grok/agents/beads.md
```

Edit the templates, rebuild, re-run — the subagent files stay current with the
workflow (lifecycle, agent-mail coordination, context contract). The tool
AGENTS.md files carry only a pointer to the subagent.

## Backend protocol (herdr default)

When `FLYWHEEL_BACKEND=herdr`: socket protocol v19, one JSON-RPC request per
connection (newline-delimited); `events.subscribe` keeps a long-lived stream.
See `src/herdr.rs` / `src/backend/herdr.rs`.

## Collaboration (event-driven)

The durable loop is **event-wake primary** (`__poll_events` / `events.subscribe`)
with **`__poll` heartbeat** as the correctness floor (no event replay). See
[ADR-0001](docs/decisions/0001-event-driven-agent-collaboration-spine.md) and
`docs/decisions/_scripts/verify-0001.sh`.

| Pattern | Command | Contract |
|---------|---------|----------|
| Debate | `flywheel debate "<proposition>" --rounds 3` | A/B + judge, thread `debate-<ts>` |
| Plan→review→implement | `flywheel plan-review <project> "<feature>"` | APPROVE/CHANGES/VERIFIED/REJECT; paper-first |
| Cross-project | `flywheel xproj send …` | product + contact; subject `[xproj]`; `--to-project` is routing context (delivery is name-based after `xproj request --to-project` handshake) |
| Peer-triggered | spawn with peer-aware prompt | peer mail wakes idle peer |
| **Workflow run** (ADR-0010) | `flywheel workflow run <name> --execute --mode headless|pane` | declarative multi-step procedure; role-conditioned + model-injected dispatch; `runs/` logged when headless |
| **Coalition** (ADR-0011) | `flywheel coalition form <goal> --crews <squad>` | steer+collab layers; intent-based coordination (declare → soft-conflict → propose → negotiate); captain-formed + auto-coalesce |
| **Cron** (ADR-0012) | `flywheel cron start` (drives cron YAMLs) | standalone scheduled primitive distinct from workflow; body composes workflows; Resonate `ctx.sleep` durable-sleep |

Full recipe catalog: [docs/collab/recipes.md](docs/collab/recipes.md). Agent
contracts (ack, lease renew, build-slot, pane safety): `AGENTS.md`.

## Changelog / releases

The changelog is GENERATED with [git-cliff](https://git-cliff.org) (Keep a
Changelog + Conventional Commits; the config also groups the repo's
bead-prefixed commits):

```sh
git cliff -o CHANGELOG.md          # regenerate (after commits/tags)
git cliff --unreleased          # preview pending entries (stdout)
git cliff --bump                   # release: bump version, tag vX.Y.Z, write changelog
```

Versioning: semantic (MAJOR breaking / MINOR features / PATCH fixes);
`version` in `Cargo.toml` is the source of truth for the binary's `--version`.

## Exit codes

0 ok · 1 herdr/server error · 2 usage error. stdout = data, stderr = messages,
`--json` for machine output (color auto-disabled when piped / `NO_COLOR`).

## Roadmap (v3)

**Shipped (2026-08):** ADR-0010 execution wiring (real runner `--execute`, headless/pane modes,
model registry + per-role models + retry-on-429, per-step harness override) · ADR-0011 coalitions
(steer+collab layers, intent protocol, soft-conflict, auto-coalesce) · ADR-0012 standalone cron
(distinct YAML type, user dirs, Resonate durable-sleep) · loop role-conditioning (`__send
--profile`) + the stale-landed dedup fix.

**Remaining:**
- **Loop `--engine` parity gate** (`bd-3aen`) — run the 6 `scripts/e2e-hardened-loop.sh` scenarios
  bar-for-bar on `--engine` vs legacy; then the full loop-as-durable-workflow migration + legacy
  removal.
- **Loop reliability** (`bd-bvwf.1–.5`) — orphan-workspace accumulation, first-poll-before-
  detection, loop-exits-between-chain-steps, stray/duplicate loop processes, unknown-agent stall.
- **On-demand `cron run` completion** (`bd-...`) — the run invokes the body but the recording
  runner + composed pipeline does not complete; the scheduled durable loop is the real path.
- **Deferred backlog** (`bd-rwbn`, ❄) — pa (prime-agent) harness, metered dynamic model routing,
  model governance (rate-limits/budgets), deep federation.

Feedback: open an issue at https://github.com/bangedorrunt/flywheel/issues

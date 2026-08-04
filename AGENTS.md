# AGENTS.md — flywheel

flywheel is a Rust CLI: a herdr-backed swarm orchestrator with an NTM-parity
command surface and a durable monitoring loop built on Resonate.

## Session facts

- Backend: herdr server via unix socket (newline-delimited JSON-RPC, one request
  per connection — see `src/herdr.rs`). Raw pane control: `herdr` CLI.
- Tracker: br/bv (CLI-only — there is NO beads MCP server; `beads-mcp` is the
  legacy bd-era plugin, never wire it in).
- Coordination: agent-mail MCP server (`http://127.0.0.1:8765/mcp/`, bearer
  token from `~/.config/flywheel/config.toml`).
- Durable loop: Resonate server via launchd (`com.flywheel.resonate`,
  `http://localhost:8001`, sqlite at `~/.local/state/flywheel/resonate.db`).
- State/logs: `~/.local/state/flywheel/` (event logs, checkpoints, loops.json).
- The binary also lives at `~/.local/bin/flywheel` — rebuild + copy after
  changes (`cargo build --release && cp target/release/flywheel ~/.local/bin/flywheel`).

## Rules

- **TDD**: write failing tests first (RED-GREEN-REFACTOR). `cargo test` must pass.
- **No script-based code changes** in this repo — edit files manually.
- **No file deletion** without explicit user permission.
- **br is CLI-only**: `br ready --json`, `br update <id> --claim --actor "$AGENT_NAME"`,
  `br close <id> --reason "..."`, `br sync --flush-only` + commit `.beads/`
  (br never runs git). `bv --robot-*` for triage; never bare `bv`.
- **herdr protocol**: verify against `herdr api schema --json` (protocol v18).
  CLI output = one JSON line per command; exit 0 ok, 1 server error, 2 usage.
- **Resonate SDK pinned**: `resonate-sdk = "0.6.0"` (crates.io). v0.6.0 has no
  version-based re-invocation — fresh runs get fresh invocation ids
  (`swarm-loop:<project>:<ts>` in `loops.json`). Leaves must take simple
  identifier params (no tuple destructuring) and return the prelude `Result`.
- **cli-design discipline**: stdout = data (`--json` for machine), stderr =
  messages, exit codes 0/1/2, TTY-aware color (NO_COLOR/TERM=dumb), XDG config.

## Skills to load when relevant

- `cli-design` — command surface, help, output discipline
- `br`, `bv`, `beads-workflow`, `beads-compliance-and-completion-verification` — tracker lifecycle
- `agent-mail` — coordination (identities, reservations, threads)
- `resonate-philosophy`, `resonate-basic-durable-world-usage-rust`,
  `resonate-basic-ephemeral-world-usage-rust`, `resonate-durable-sleep-scheduled-work-rust` — durable loop design
- `herdr` — raw pane/workspace control (upstream master copy)
- `rust-best-practices`, `code-quality` — Rust style

## Architecture

```
src/
  main.rs      # clap surface, swarm helpers, sync loop, hidden __poll/__send leaves
  herdr.rs     # socket client: one request per connection, typed helpers
  durable.rs   # Resonate workflow (swarm_loop) + leaves + resume/status
  config.rs    # XDG config, recipes/workflows/pipelines/policy
  state.rs     # event log, checkpoints, active-loop registry, analytics
  mail.rs      # agent-mail JSON-RPC over HTTP
  work.rs      # br/bv pass-through
  output.rs    # TTY-aware color, JSON printing
```

The durable loop (`flywheel loop <project>`) is the orchestrator: polls the
swarm via `__poll` (leaf), feeds queued instructions to idle agents via
`__send`, sleeps durably, and exits when the queue drains. Crashes resume by
re-running the same command (same invocation id while pending).

<!-- br-agent-instructions-v2 -->

---

## Beads Workflow Integration

Bead/issue work in this project is delegated to the **`beads` subagent**
(triage → claim → reserve → announce → execute → verify → close → sync →
handoff). The subagent definition is GENERATED — regenerate it after any
flywheel rebuild so it stays current with the workflow:

```bash
flywheel integration opencode   # -> ~/.config/opencode/agents/beads.md
flywheel integration pi         # -> ~/.pi/agent/agents/beads.md
flywheel integration grok       # -> ~/.grok/agents/beads.md
```

### Essential Commands (same surface the subagent uses)

```bash
br ready --json                    # ready work (open, unblocked, not deferred)
br list --status=open --json       # all open issues
br show <id>                       # full issue + dependencies
br search "keyword"                # full-text search
br create --title="..." --description="..." --type=task --priority=2
br update <id> --claim --actor "$AGENT_NAME" --json   # atomic claim
br close <id> --reason="..."       # close with an honest reason
br sync --flush-only               # export DB -> JSONL (then commit .beads/)
br dep add <new> --type discovered-from <current>     # link discoveries
bv --robot-triage | --robot-next   # graph-aware picks; NEVER bare bv
```

### Lifecycle (mandatory, in order)

1. **Triage** — `bv --robot-triage` / `br ready --json`; respect priority + dependency order.
2. **Claim** — `br update <id> --claim --actor "$AGENT_NAME" --json`. Never work unclaimed.
3. **Reserve** — agent-mail `file_reservation_paths` (exclusive) for files you will touch.
4. **Announce** — agent-mail: which bead, which files, expected outcome.
5. **Execute one narrow slice** — the bead is self-contained; no scope creep.
6. **Quality gates** — narrowest test/lint/typecheck for the slice.
7. **Verify compliance** — acceptance criteria met, not just "compiles".
8. **Close + sync** — `br close` (honest reason), `br sync --flush-only`, `git add .beads/ && git commit` (br never runs git).
9. **Handoff** — agent-mail: what changed / verified / remaining; release reservations.

### Key Concepts

- **Dependencies**: `br ready` shows only open, unblocked, non-deferred work; `br dep add` links blocks/discovered-from.
- **Priority**: 0-4 (P0 critical … P4 backlog). **Types**: task/bug/feature/epic/chore/docs/question.
- **Context contract**: at ~80% session context, write `{"context_pct", "ts"}` to
  `.flywheel/agent-state/<AGENT_NAME>.json` (best-effort), finish + sync the current bead,
  then reply exactly `CONTEXT_LIMIT` — the orchestrator respawns you.
- **br is CLI-only — NO beads MCP server.** `beads-mcp` is the legacy bd-era plugin; never wire it in.
  Canonical agent contract: `br robot-docs guide`, `br capabilities --format json`, `br schema commands --format json`.

<!-- end-br-agent-instructions -->
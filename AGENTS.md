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
- **herdr protocol**: verify against `herdr api schema --json` (protocol v17).
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

<!-- br-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`/`bd`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View ready issues (open, unblocked, not deferred)
br ready              # or: bd ready

# List and search
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br search "keyword"   # Full-text search

# Create and update
br create --title="..." --description="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason="Completed"
br close <id1> <id2>  # Close multiple issues at once

# Sync with git
br sync --flush-only  # Export DB to JSONL
br sync --status      # Check sync status
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Always run `br sync --flush-only` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only open, unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers 0-4, not words)
- **Types**: task, bug, feature, epic, chore, docs, question
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads changes to JSONL
git commit -m "..."     # Commit everything
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always sync before ending session

<!-- end-br-agent-instructions -->

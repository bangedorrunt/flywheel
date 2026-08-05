# AGENTS.md — flywheel

flywheel is a Rust CLI: a herdr-backed swarm orchestrator with an NTM-parity
command surface and a durable monitoring loop built on Resonate.

## Session facts

- **Backends (ADR-0002):** pluggable multiplexer + mail via traits in `src/backend/`.
  Defaults: herdr + agentmail. Discover: `flywheel backend list --json`.
  Details: [Backends](#backends) below.
- **herdr protocol v19** (default mux): unix socket JSON-RPC; event stream via
  `events.subscribe`. Loop = **event-wake + poll heartbeat** (ADR-0001).
- Tracker: br/bv (CLI-only — there is NO beads MCP server; `beads-mcp` is the
  legacy bd-era plugin, never wire it in).
- Durable loop: Resonate server via launchd (`com.flywheel.resonate`,
  `http://localhost:8001`, sqlite at `~/.local/state/flywheel/resonate.db`).
- State/logs: `~/.local/state/flywheel/` (event logs, checkpoints, `loops.json`).
  - `loops.json` schema **v2** (`src/state.rs`, bd-loop-state-schema-v2): top-level
    `"schema": 2` + per-project `{ id, params, created, event_cursor?, escalation?,
    respawn? }`. v1 files (no `schema`) load with fresh control fields and migrate
    on next write. Control state only — telemetry stays in the JSONL event log.
    See module docs on `state.rs` for both-direction back-compat.
- The binary also lives at `~/.local/bin/flywheel` — rebuild + copy after
  changes (`cargo build --release && cp target/release/flywheel ~/.local/bin/flywheel`).

## Backends

Pluggable **multiplexer** + **mail** (ADR-0002). Collaboration spine (ADR-0001)
targets traits, not concretes — backend swaps survive without bead rewrites.

### Traits + matrix

| Role | Trait | Default impl | Stubs (feature-gated, `Unsupported`) |
|------|--------|--------------|--------------------------------------|
| Multiplexer | `MultiplexerBackend` | `HerdrBackend` (`backend-herdr`) | superlogical, tmux |
| Mail | `MailBackend` | `AgentMailMcpBackend` (`mail-agentmail`) | toron (`mail-toron`) |

Factory: `src/backend/factory.rs` → `Box<dyn MultiplexerBackend>` /
`Box<dyn MailBackend>`. Code: `src/backend/`.

### Selection (precedence)

**flag > env > `.env` (dotenvy) > `config.toml` `[backend]` > default**

| Var / key | Values | Default |
|-----------|--------|---------|
| `FLYWHEEL_BACKEND` / `[backend] multiplexer` | `herdr` \| `superlogical` \| `tmux` | `herdr` |
| `FLYWHEEL_MAIL` / `[backend] mail` | `agentmail` \| `toron` | `agentmail` |
| `FLYWHEEL_AGENTMAIL_URL` / `TOKEN` / `FLYWHEEL_MAIL_NAME` | agent-mail MCP | config.toml |

Copy `.env.example` → `.env` (gitignored). Never commit tokens.

Cargo features: `backend-herdr`, `backend-superlogical`, `backend-tmux`,
`mail-agentmail`, `mail-toron` (default = herdr + agentmail + serve).

### Discovery

```bash
flywheel backend list --json   # compiled-in + active + ready|stub
flywheel backend status        # active pair + config source + health
flywheel backend default <multiplexer|mail> <name>  # write config.toml
```

Spawned agents: run `flywheel backend list --json` if dispatch primitive is unclear
(herdr prompt vs future superlogical equivalent).

### Migration path (ADR-0002 §4)

| Phase | State | Action |
|-------|--------|--------|
| **R0** (now) | toron v0.1, superlogical pre-release | Traits + herdr/agentmail wraps; stubs reserve env names |
| **R1** | toron-core ships | Real `ToronMailBackend`; flip `FLYWHEEL_MAIL=toron` when parity passes |
| **R1.5** | toron `Backend` trait | Trait merge; delete flywheel `herdr.rs` when single source lands |
| **R2** | superlogical SDK | Real `SuperlogicalBackend`; env flip |

Cross-links: [ADR-0002](docs/decisions/0002-pluggable-backend-abstraction.md),
[ADR-0001](docs/decisions/0001-event-driven-agent-collaboration-spine.md),
toron ADR-0003 (embed path).

### Products + cross-project contact

agent-mail **products** group projects for unified search/inbox. Captain stack
conventions (create with `flywheel mail product ensure` / `link`):

| Product | Projects |
|---------|----------|
| `trading-research` | hachiko, flywheel (+ strategy repos) |
| `knowledge` | chiebukuro, toron |

```bash
flywheel mail contact request <project> --to <agent> [--to-project <path>]
flywheel mail contact approve|deny <project> --from <agent>
flywheel mail contact list <project>
flywheel mail product ensure --name trading-research
flywheel mail product link --product trading-research <project>
flywheel mail product search --product trading-research "query"
```

Contact policy default: **auto** (not open). Cross-project spam requires shared
context or explicit approve.

#### One-time setup (cross-project swarm, ADR-0001 §9.5)

```bash
# products
flywheel mail product ensure --name trading-research
flywheel mail product link --product trading-research ~/workspace/hachiko
flywheel mail product link --product trading-research ~/workspace/flywheel
flywheel mail product ensure --name knowledge
flywheel mail product link --product knowledge ~/workspace/chiebukuro
flywheel mail product link --product knowledge ~/workspace/toron

# contact handshake (from hachiko side; flywheel orchestrator approves)
flywheel xproj request hachiko --to FlywheelOrch --to-project ~/workspace/flywheel \
  --reason "run backtest swarms"
# on flywheel: flywheel mail contact approve flywheel --from HachikoStrategist
```

#### Runtime fan-out

```bash
# hachiko-strategist → flywheel orchestrator
flywheel xproj send hachiko --to FlywheelOrch --to-project ~/workspace/flywheel \
  --recipe backtest-5m --workers 4 --body "strategy=CrystalRegime asset=BTC"
# body uses subject [xproj] + recipe=/workers=/product= lines
# receiver: parse with flywheel xproj parse "…"; spawn recipe; reply in thread
# requester reads via: flywheel mail product search --product trading-research "xproj"
```

Receiving loop: each durable-loop iteration runs `__xproj-inbox` (orchestrator
inbox scan). Messages with `[xproj]` + `recipe=` are queued as worker prompts
once (`seen_xproj` dedup). Reply results in the same thread after the swarm
runs. Do not overload normal bead dispatch.

## Rules

- **TDD**: write failing tests first (RED-GREEN-REFACTOR). `cargo test` must pass.
- **No script-based code changes** in this repo — edit files manually.
- **No file deletion** without explicit user permission.
- **br is CLI-only**: `br ready --json`, `br update <id> --claim --actor "$AGENT_NAME"`,
  `br close <id> --reason "..."`, `br sync --flush-only` + commit `.beads/`
  (br never runs git). `bv --robot-*` for triage; never bare `bv`.
- **herdr protocol**: verify against `herdr api schema --json` (protocol v19).
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

The durable loop (`flywheel loop <project>`) is the orchestrator: **primary
signal** `__poll_events` (forward-only herdr subscribe); **correctness floor**
`__poll` every iteration; dispatch via `__send`; durable sleep/checkpoint via
Resonate. Crashes resume by re-running the same command (same invocation id
while pending). Verify: `docs/decisions/_scripts/verify-0001.sh`.

### Agent contracts (spawn prompts)

Baked into `bead_prompt` / `resume_prompt` (`src/durable.rs`):

- **Lease renew** — agents renew their own agent-mail file reservations; orchestrator cannot renew on their behalf.
- **Ack** — when `ack_required=true`, call `acknowledge_message` after acting; orchestrator cannot ack on their behalf.
- **Build-slot** — before `cargo build`/`cargo test`, acquire slot `cargo`; release when done (plan-review implementer prompt).
- **Pane safety** — never close peers' panes; only flywheel-created panes are respawn targets.
- **CONTEXT_LIMIT** — at ~80% context write state file + emit marker for respawn.

### Collaboration patterns (ADR-0001 §9)

Catalog: [docs/collab/recipes.md](docs/collab/recipes.md).

| Pattern | CLI | Thread / token |
|---------|-----|----------------|
| Debate §9.1 | `flywheel debate "<prop>" --rounds N` | `debate-<ts>` |
| Plan-review-implement §9.2 | `flywheel plan-review <proj> "<feature>"` | `plan-<bead\|ts>`; APPROVE/CHANGES/VERIFIED/REJECT |
| Peer-triggered §9.4 | peer-aware spawn prompt | peer mail wakes idle agent |
| Cross-project §9.5 | `flywheel xproj send/request` | product + contact; `[xproj]` |

### Pattern: plan-review-implement (ADR-0001 §9.2)

```bash
flywheel plan-review <project> "<feature spec>" \
  [--max-change-rounds 2] [--planner oc] [--reviewer pi] [--implementer gk] \
  [--bead bd-xxx]
```

Threaded gate (`plan-<bead|ts>`): planner drafts → reviewer `APPROVE|CHANGES:` →
implementer (build-slot `cargo` before cargo; paper-first branch, no merge) →
reviewer `VERIFIED|REJECT:`. Max change rounds then escalate. **Paper-first:**
VERIFIED does not close beads or merge — human/overseer sign-off. Tokens are
leading-line only (`parse_review_token`).

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
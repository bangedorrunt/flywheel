# AGENTS.md — flywheel

flywheel is a Rust CLI: a herdr-backed swarm orchestrator with an NTM-parity
command surface and a durable monitoring loop built on Resonate.

## Session facts

- **Backends (ADR-0002):** pluggable multiplexer + mail via traits in `src/backend/`.
  Defaults: herdr + toron. Discover: `flywheel backend list --json`.
  Details: [Backends](#backends) below.
- **herdr protocol v19** (default mux): unix socket JSON-RPC; event stream via
  `events.subscribe`. Loop = **event-wake + poll heartbeat** (ADR-0001).
- Tracker: br/bv (CLI-only — there is NO beads MCP server; `beads-mcp` is the
  legacy bd-era plugin, never wire it in).
- Durable loop: Resonate server via launchd (`com.flywheel.resonate`,
  `http://localhost:8001`, sqlite at `~/.local/state/flywheel/resonate.db`).
- Toron mail daemon: launchd `com.flywheel.toron` — `toron serve --mcp-http
  127.0.0.1:18080` with `TORON_OPERATOR_KEY=~/.config/toron/operator_nsec`
  + `TORON_KEY_PASSPHRASE=toron` env (plist: `~/Library/LaunchAgents/`).
  The `[mail.toron] url` in config.toml points the default backend at it.
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
| Mail | `MailBackend` | `AgentMailBackend` real AM (`mail-agentmail`) | none (toron is a real peer, `mail-toron`) |

Factory: `src/backend/factory.rs` → `Box<dyn MultiplexerBackend>` /
`Box<dyn MailBackend>`. Code: `src/backend/`.

### Selection (precedence)

**flag > env > `.env` (dotenvy) > `config.toml` `[backend]` > default**

| Var / key | Values | Default |
|-----------|--------|---------|
| `FLYWHEEL_BACKEND` / `[backend] multiplexer` | `herdr` \| `superlogical` \| `tmux` | `herdr` |
| `FLYWHEEL_MAIL` / `[backend] mail` | `toron` \| `agentmail` | `toron` |
| `FLYWHEEL_AGENTMAIL_URL` / `TOKEN` | feeds `[mail.agentmail]` only (ADR-0003 V3) | config.toml |
| `FLYWHEEL_MAIL_NAME` | shared identity (`[agent_mail] name`) | `flywheel` |

Mail transport is **per-backend** (ADR-0003): `[mail.toron]` = toron daemon
(absent → embedded in-process); `[mail.agentmail]` = real mcp_agent_mail
server (default `http://127.0.0.1:8765`). The legacy `[agent_mail] url` is
not a routing source.

Copy `.env.example` → `.env` (gitignored). Never commit tokens.

Cargo features: `backend-herdr`, `backend-superlogical`, `backend-tmux`,
`mail-agentmail`, `mail-toron` (default = herdr + toron + agentmail + serve).

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
| **R0** | toron v0.1, superlogical pre-release | Traits + herdr/agentmail wraps; stubs reserve env names |
| **R1** (done) | toron-core ships | Real `ToronMailBackend`; default flipped to `FLYWHEEL_MAIL=toron` |
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
- **Live suite**: `cargo test -- --ignored` runs the 6 live integration tests (agent-mail
  round-trips, herdr parity) against the running server — requires agent-mail on :8765 and
  herdr up. The default `cargo test` skips them loudly (each has an explicit reason + runbook).
- **Review-fix regression gate**: `bash docs/decisions/_scripts/verify-review-fixes.sh` asserts the
  F1-F8 fixes (harvest body, gate loop, xproj to_project/focus, T3 halt, backend migration,
  default agents, cleanup) stay fixed; `verify-0001.sh` / `verify-0002.sh` gate the ADRs.
- **Clippy gate**: `bash scripts/verify-clippy.sh` must exit 0 (cargo clippy --all-targets --all-features --locked -- -D warnings).
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
- `toron` — coordination (identities, reservations, threads, build slots)
- `resonate-philosophy`, `resonate-basic-durable-world-usage-rust`,
  `resonate-basic-ephemeral-world-usage-rust`, `resonate-durable-sleep-scheduled-work-rust` — durable loop design
- `herdr` — raw pane/workspace control (upstream master copy)
- `rust-best-practices`, `code-quality` — Rust style

## Architecture

```
src/
  main.rs      # clap surface, swarm helpers, sync loop, hidden __poll/__send leaves
  herdr_shim.rs # herdr client re-exports + helpers (client lives in toron-core)
  durable.rs   # Resonate workflow (swarm_loop) + leaves + resume/status
  config.rs    # XDG config, recipes/workflows/pipelines/policy
  state.rs     # event log, checkpoints, active-loop registry, analytics
  mail_shim.rs # mail facade over toron-core MailSurface (toron)
  work.rs      # br/bv pass-through
  output.rs    # TTY-aware color, JSON printing
```

The durable loop (`flywheel loop run <project>`, legacy alias `flywheel loop <project>`) is the orchestrator: **primary
signal** `__poll_events` (forward-only herdr subscribe); **correctness floor**
`__poll` every iteration; dispatch via `__send`; durable sleep/checkpoint via
Resonate. Crashes resume by re-running the same command (same invocation id
while pending). Verify: `docs/decisions/_scripts/verify-0001.sh`.

**Stale pending invocations self-clean (bd-87hq)**: the loop checkpoints on the
Resonate server (default `http://localhost:8001`, launchd
`com.flywheel.resonate`, sqlite `~/.local/state/flywheel/resonate.db`). Killed
runs (SIGKILL/crash) leave their root invocation PENDING; the worker's group
anycast replays them, and replaying their checkpointed children against the
CURRENT leaf sequence causes child-id collisions → one leaf reads another's
stored result → `poll_*` serde errors (`missing field name`, `invalid type:
map, expected a sequence`) → the loop continues empty and exits "complete"
dispatching nothing. Every `loop run` now rejects stale pending project loop
ROOT invocations first (`swarm-loop:{base}:<ts>` / `engine-loop:{base}:<ts>`,
exact colon-delimited prefix, roots only — children carry a `.`, never
cleared), preserving the resumed id. `--force` additionally rejects a
corrupted active invocation and starts fresh. Diagnose:
`resonate promises search -S http://127.0.0.1:8001 --state pending`.

### Agent contracts (spawn prompts)

Baked into `bead_prompt` / `resume_prompt` (`src/durable.rs`):

- **Lease renew** — agents renew their own file reservations (toron `renew_file_reservations` / `flywheel mail locks`); orchestrator cannot renew on their behalf.
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
reviewer `VERIFIED|REJECT:`. `--max-change-rounds N` is HONORED — the gate is a
real phase-driven state machine that loops planner↔reviewer up to N rounds then
escalates. **Paper-first:** VERIFIED does not close beads or merge — human/
overseer sign-off. Tokens are leading-line only (`parse_review_token`).

**Content semantics (review-fix F1, 2026-08-05):** debate + plan-review harvest
agents' reply **bodies** (`body_md`), never subjects — a reviewer's
APPROVE/CHANGES token and a debater's argument live in the body. Default agent
names (`oc`/`pi`/`gk`) resolve to the spawned `flywheel-<base>-<short>`
identities (review-fix F7).

### Pattern: blocked escalation (ADR-0001 §4.4)

Tiered by time blocked: T0 ping → T1 re-ping → T2 urgent → **T3 halts the loop**
(bounded, `--on-blocked mail`; `stop` halts at any tier; `continue` never halts).
The T3 exit is a distinguishable `halted:` result, not a normal `complete`
(review-fix F4, 2026-08-05).

## Workflows, Cron, Recipes & Profiles (orchestration vocabulary)

The declarative orchestration tier (ADR-0005) + execution wiring (ADR-0010) + coalitions (ADR-0011) + standalone cron (ADR-0012).
A **profile** = role template (behavior + model + harness); a **recipe/squad** =
group of profiles; a **workflow** = multi-step procedure using profiles; a **cron** =
a FIRST-CLASS scheduled primitive DISTINCT from workflow, with a body composing
workflows. **Full authoring guide: [docs/workflow-authoring.md](docs/workflow-authoring.md).**

| Verb | Command |
|---|---|
| **profile** | `flywheel profile {list, show, create, edit, run, remove}` — the 6 built-ins (oracle/planner/reviewer/coder/scout/housekeeper) + locals in `~/.config/flywheel/profiles.toml` |
| **squad/recipe** | `flywheel squad {list, show, spawn, remove}` — `{profile: count}` groups; legacy `recipes list` stays an alias |
| **workflow** | `flywheel workflow {list, show, lint, run, new, history, crew, remove}` — YAML in `~/.config/flywheel/workflows/`; `lint` is the fail-stop gate |
| **cron** *(ADR-0012)* | `flywheel cron {lint, run, list, show, pause, resume, status, history, start, remove}` — standalone cron YAMLs in `~/.config/flywheel/crons/` + `<project>/.flywheel/crons/` (precedence project > global > builtin); `lint` is the fail-stop gate; `start` runs the durable loops (Resonate `ctx.sleep`, not systemd) |
| **model** *(ADR-0010)* | `flywheel model {list, show, add, remove}` — registry in `~/.config/flywheel/models.toml` |
| **runs** | `flywheel runs {list, show, pending, digest}` — observability: state + owner + per-run cost; headless run logs feed this |

```bash
# create → lint → run → remove
flywheel workflow new upgrade-thing && $EDITOR ~/.config/flywheel/workflows/upgrade-thing.yaml
flywheel workflow lint upgrade-thing
flywheel workflow run upgrade-thing --execute --inputs '{"x":1}' --crew auto
flywheel workflow remove upgrade-thing

# a scheduled, unattended cron — a STANDALONE YAML (ADR-0012), not a workflow
# ~/.config/flywheel/crons/weekly-upgrade.yaml:
#   name: weekly-upgrade / schedule: "30 3 * * 0" / owner: housekeeper / mode: headless
#   policy: {on_failure: bead, catchup: latest} / steps: [call_workflow: upgrade-thing]
flywheel cron lint weekly-upgrade        # fail-stop gate
flywheel cron start                      # durable loops (Resonate ctx.sleep, not systemd)
```

**Status (ADR-0010/0011/0012):** implemented + live — `--execute`, `--mode headless|pane`,
`--model`, `model {...}`, `coalition {...}` (steer+collab layers, intent protocol), and the
standalone cron YAMLs (weekly-review migrated; crons.toml retired). The **live swarm path stays
`flywheel loop run <project> --work`**; `workflow run --execute` is the declarative path.

**Two execution models (ADR-0010 Spec E):** *pane* (interactive, durable promise,
bus-auditable — you watch) vs *headless* (autonomous/cron, synchronous, local-only,
logged to `runs/` — no one watches). Cron defaults to headless. Role-conditioning
(profile `behavior` preamble prepended to every dispatch) + model injection apply to
both.

<!-- br-agent-instructions-v2 -->

---

## Beads Workflow Integration

Bead/issue work is delivered via **slash commands** (triage → claim → reserve →
announce → execute → verify → close → sync → handoff): `/beads-triage`,
`/beads-execute`, `/beads-review`, `/beads-commit`, `/beads-create` — the
`prompts/beads-*.md` files per tool (opencode/pi/grok/omp). GENERATED from the
shared body (`integrations/beads-body.md.tpl`) — regenerate after any flywheel
rebuild so they stay current with the workflow:

```bash
flywheel integration opencode   # -> prompts/beads-{triage,execute,review,commit,create}.md
flywheel integration pi         # -> ~/.pi/agent/prompts/beads-*.md
flywheel integration grok       # -> ~/.grok/prompts/beads-*.md
flywheel integration omp        # -> ~/.omp/agent/prompts/beads-*.md
```

### Essential Commands (the lifecycle the slash commands drive)

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
3. **Reserve** — toron `file_reservation_paths` (exclusive) for files you will touch. (`flywheel mail locks reserve <project> --as YOUR_NAME` is the CLI equivalent; `flywheel mail locks list <project>` shows the board.)
4. **Announce** — toron: which bead, which files, expected outcome (`flywheel mail send` / `toron_send_message`, thread-id = bead id).
5. **Execute one narrow slice** — the bead is self-contained; no scope creep.
6. **Quality gates** — narrowest test/lint/typecheck for the slice.
7. **Verify compliance** — acceptance criteria met, not just "compiles".
8. **Close + sync** — `br close` (honest reason), `br sync --flush-only`, `git add .beads/ && git commit` (br never runs git).
9. **Handoff** — toron: what changed / verified / remaining; release reservations.

### Key Concepts

- **Dependencies**: `br ready` shows only open, unblocked, non-deferred work; `br dep add` links blocks/discovered-from.
- **Priority**: 0-4 (P0 critical … P4 backlog). **Types**: task/bug/feature/epic/chore/docs/question.
- **Context contract**: at ~80% session context, write `{"context_pct", "ts"}` to
  `.flywheel/agent-state/<AGENT_NAME>.json` (best-effort), finish + sync the current bead,
  then reply exactly `CONTEXT_LIMIT` — the orchestrator respawns you.
- **br is CLI-only — NO beads MCP server.** `beads-mcp` is the legacy bd-era plugin; never wire it in.
  Canonical agent contract: `br robot-docs guide`, `br capabilities --format json`, `br schema commands --format json`.

<!-- end-br-agent-instructions -->
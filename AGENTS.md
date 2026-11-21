# flywheel

Fleet orchestration over a terminal multiplexer: the session door, topology
classify, durable loops, workflow compose, and the doorbell. Work tracking is
`br`; mail and file reservations are toron. Dependencies run one way only:
**flywheel → toron-workflow → toron-core**.

Ops crib: `~/.agents/skills/flywheel/SKILL.md`. Detail lives under
`docs/agent-instructions/` and `docs/guides/`.

## Commands

Rust runs through mbx. Prefix every cargo command with
`mbx +nightly-aarch64-apple-darwin` (bare `cargo` misparses under the mbx shim),
and never set `CARGO_TARGET_DIR`: mbx shares one managed target and cache across
the fleet.

```bash
# develop and test (targeted: -p <crate> <filter>)
mbx +nightly-aarch64-apple-darwin test -p flywheel --bin flywheel <filter>
bash scripts/verify-clippy.sh
bash scripts/verify-needles.sh            # every mutation find= matches once; every TEST_FILTER names a real test
bash scripts/verify-mutations.sh scripts/mutations/<spec>.spec.sh

# commit and pre-mail check
flywheel commit <paths> -m <msg> [--repo <dir>]
bash scripts/fence.sh --sha <sha>         # needles, -Dwarnings build, no-run, clippy on that commit

# captain: prove, install, land
bash scripts/clean-proof.sh --sha <rev> --mutations changed --keep --receipt /tmp/<dir>/r.json
AGENT_NAME=<captain> FLYWHEEL_SOURCE=<kept>/flywheel TORON_SOURCE=<kept>/toron \
  bash <kept>/flywheel/scripts/stack.sh install
flywheel stack status --check
flywheel ship land <bead-id> --sha <40hex> --receipt <proof.json> --as <pin> [--repo <dir>]
flywheel integration install|uninstall|status [harness...]   # operator/captain only

# crews and seats
flywheel crew inbox --open [--pin <pin>] [--project p] [--json]
flywheel crew done <brief-id> --receipt-json <file|-> [--project p]
flywheel crew land <brief-id> --proof-receipt <proof.json> [--project p]
flywheel swarm plan|apply|status <project> ...
flywheel retire <pin>... [--project p] [--force-working]
```

`test --exclude-ignored` is invalid. Live tests (`tests/live_*.rs`) need a
multiplexer and mail; run them with `FLYWHEEL_LIVE_E2E=1 bash scripts/e2e-live.sh`.

## Working in the shared tree

Every crew works and builds in this one repo. There are no private worktrees,
exports or scratch target dirs.

- Reserve the files you edit before you edit them (`toron reserve acquire`),
  and release them when you commit.
- Keep the tree compiling. Never leave a half-edit that breaks the build for
  other lanes; commit small steps often. If another lane's file breaks your
  build, mail its holder.
- Commit only with `flywheel commit <paths>`. It commits exactly those paths
  through a private index, runs the pre-commit fence and advances HEAD by CAS,
  so a neighbour's staged entries are never swept in. Never `git add -A`,
  `stash`, `checkout --`, `restore`, `reset`, `rebase` or `--amend`.
- Before you mail a sha, run `scripts/fence.sh --sha <sha>` plus the targeted
  tests and the mutation specs covering the paths you changed. Do not run the
  full workspace suite; the captain's proof does.
- The captain proves a revision in one clean export (`clean-proof.sh --sha`),
  installs only from a green proof's kept export, and lands with `ship land`.
  The mutation stage stops at the first spec that does not prove every row.

## Pane plane

`FLYWHEEL_BACKEND` / `[backend] multiplexer` ∈ `{auto,rex,superlogical,hc}`;
the default is rex, with hc as fallback. Every door reads one resolution from
`toron_core::backend::plane`. A write replays on hc only for `Connect`, since a
timeout may already have committed. Agent status comes from OSC 7501 records
(`rex api describe com.superlogical.terminal.program_status`). Explore the rex
server with `rex api list`, `rex api describe <method>` and `rex api call`.
Detail: `docs/agent-instructions/backends.md`.

## Test sandbox (hard)

Any test that spawns the binary or shells supervisor tools:

1. Builds its `Command` through `flywheel_test_sandbox::TestSandbox`: temp
   `HOME` and XDG dirs, a temp `TORON_DATA_DIR`, scrubbed routing env, and
   `launchctl`/`systemctl` deny-shims first on `PATH`, with a `Drop` that fails
   if a shim was reached. `scripts/check-test-sandbox.sh` refuses any spawn that
   skips it.
2. Binds daemons on spare ports. It never touches live `:18080` (toron) or
   `:18980` (doorbell), the live store `~/.local/share/toron/index.sqlite`, or
   the operator's `~/.config/toron/`.
3. Never prints key material. `~/.config/toron/operator_nsec` is a secret.
4. Never reads ambient state (operator config, wall clock, a live daemon, or a
   shared state dir without `config::test_state_home()`).

The only carve-out is `tests/live_*.rs`: every `#[test]` in it is `#[ignore]`d
and the file is gated on `FLYWHEEL_LIVE_E2E`.

## Rules

- **Tick law.** Reducers are pure `(Model, Msg) -> (Model, Vec<Cmd>)`: no I/O,
  clock, rand, env or locks. See `docs/agent-instructions/effect-kernel.md` and
  `~/.agents/skills/effect-oriented/SKILL.md`.
- **Journal first.** Live effects journal before the provider runs
  (`EffectJournalMode::Enforced`); dry arms are `Recording`.
- **Identity.** Explicit `--as`, then `AGENT_NAME`.
- **Fleet mutation is captain-only.** `upgrade`, `stack install`,
  `daemon`/`doorbell` start/stop/restart and `integration install` go through
  `authority::guard_fleet_mutation`; `--operator` is the deliberate escape.
- **One path per change.** Live seat changes go only through `swarm apply`, and
  retiring a pin only through `retire`.
- **Session door.** `arrive` → `briefing --json`; `decide --json` classifies
  (`open` | `swarm`). Join a live fleet loudly; never compose a second one.
- **Beads.** Create with `-d` and `--verify` (P≤2 also `--principle`). Close with
  `ship land`. Agents use `br --json` / `--robot-*`.
- **Agent-facing text** (wakes, notices, prompts) is plain English and lives in
  `crates/doorbell-core/src/agent_text.rs`; the jargon lint fails the build.
- **TDD. No script-based code edits. No file deletion without permission.** Cite
  the principle that changed a decision (`flywheel principles list`).
- **CLI change** → `Commands` enum, `--robot-capabilities`, this file, and
  `~/.agents/skills/flywheel/SKILL.md`.

## Effect law (tests)

A change is unfinished until reducer tests feed one message log and assert
model plus cmds; interpreter tests fake the clock and services; success, typed
failure, defect and interrupt stay distinct; retry keeps the same effect key;
and terminal states emit no further cmds. Prove each test can fail, with a
mutation that turns it RED.

## Details

- [Backends + config](docs/agent-instructions/backends.md)
- [Orchestration](docs/agent-instructions/orchestration.md)
- [Workflow composition](docs/agent-instructions/workflow-composition.md)
- [Durable loop](docs/agent-instructions/loop.md)
- [Effect kernel](docs/agent-instructions/effect-kernel.md)
- [Workflow authoring](docs/workflow-authoring.md)
- Sibling mailbox: `../toron/AGENTS.md`

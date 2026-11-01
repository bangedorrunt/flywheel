# flywheel

Fleet orchestration over a terminal multiplexer. Owns the session door,
topology classify, durable loops, workflow compose, and doorbell (plane D).
Work tracking is `br`; mail/reservations are toron. Deps one-way:
**flywheel → toron-workflow → toron-core**. Never reverse.

Ops crib: `~/.agents/skills/flywheel/SKILL.md`. Deep detail lives under
`docs/agent-instructions/` and `docs/guides/` — not here.

## Commands

Rust runs through mbx. Prefix every cargo invocation with
`mbx +nightly-aarch64-apple-darwin` (bare `cargo` misparses under the mbx
shim). `test` covers flywheel + doorbell-core/rt/cli via
`[workspace] default-members`.

```bash
mbx +nightly-aarch64-apple-darwin test
bash scripts/verify-clippy.sh
mbx +nightly-aarch64-apple-darwin install --path . --force
mbx +nightly-aarch64-apple-darwin install --path crates/doorbell-cli --force
# macOS stale-signature SIGKILL only:
# codesign --force --sign - ~/.cargo/bin/flywheel
# codesign --force --sign - ~/.cargo/bin/doorbell && launchctl kickstart -k gui/$(id -u)/com.doorbell.serve
```

`mbx +nightly-aarch64-apple-darwin test --exclude-ignored` is invalid.
Live ignored tests need a multiplexer + mail (see Test sandbox).

## Pane plane (rex default)

Default build features: `serve` + `backend-herdr` + `backend-superlogical` +
`mail-toron`. Selection:
`FLYWHEEL_BACKEND` / `[backend] multiplexer` ∈ `{auto,superlogical,herdr}`.

- `auto` (default when `backend-superlogical` is compiled in): **rex-primary**.
  Every door reads one resolution (`backend::factory::resolve_plane`) —
  spawn, status, snapshot, crew launch, preflight, captain.
- `FLYWHEEL_BACKEND=herdr` selects herdr explicitly.
- `REX_SESSION` is a hint about where a process runs, never proof of the plane.
- `flywheel preflight` reports a `pane-plane` check, not a herdr socket.
- Agent status on both backends folds **OSC 7501** program status
  (`idle|working|done|blocked|error|clear`) via `toron_core::osc7501::Board`.
  Spec: https://www.superlogical.com/rex/docs/build/program-status
- Rex vocabulary: **session** / **block** (not herdr workspace/pane). Ids are
  `session:…` / `block:…` and never compare across backends.
- Discover the connected server: `rex api list`, `rex api describe <method>`,
  `rex api call …`. Useful methods include `session.create`,
  `session.new_split`, `session.view`, and block method
  `com.superlogical.terminal.program_status`. One-shot Lua: `rex do -e`
  (`integrations/rex/session-snapshot.lua`). Manual crew escape hatch:
  `rex new` → `rex split` → `rex block` rename to pin → record
  `.flywheel/agent-names.json`.
- Docs: https://www.superlogical.com/rex/docs — also
  `docs/agent-instructions/backends.md`.

## Hosted control plane (hc)

`FLYWHEEL_BACKEND=hc` is reserved for toron-core's hc backend (Part B of the
hc-plane revamp). Until that lands, `auto` / `superlogical` remain the pane
plane and `herdr` stays the explicit escape hatch. Do not document herdr as
the default.


## Test sandbox (hard)

Live suites and any test that shells supervisor tools must:

1. Put **deny-shims** for `launchctl` and `systemctl` first on `PATH`. Shims
   log argv and exit 1. Check the shim logs empty after the run.
2. Use a **temp `HOME`** and temp **XDG** (`XDG_DATA_HOME`, `XDG_CONFIG_HOME`,
   `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`). Prefer an explicit `TORON_DATA_DIR`
   when the test opens a mailbox.
3. Bind daemons on **spare ports**. Never touch live **`:18080`** (toron) or
   **`:18980`** (doorbell), and never open the live store
   `~/.local/share/toron/index.sqlite` (or the operator's real
   `~/.config/toron/`).
4. Never print key material. `~/.config/toron/operator_nsec` is a secret.

A suite that bootouts the operator's real LaunchAgent is a regression, not a
pass.

## Critical overrides

- **Clean-export proof.** A green suite over a dirty tree is not a green tree.
  Proof = `git archive HEAD` into a scratch dir and build there. `git add`
  only paths you edited — never `git add -A` (other lanes share the tree).
- **Classify first.** Session door: `arrive` then `briefing --json`; argv runs
  the work. `decide --json` is the classifier. Modes: `open` | `swarm`. A loop
  is work + one verify check, not a mode. Live fleets join loud, never
  double-compose. An invisible crew is not an empty project — see global
  `~/.agents/AGENTS.md` (mail inbox, `.unanswered`, `/health.injection`).
- **Material reclassify.** On authoritative objective change, re-run
  `decide --json` + `workflow list --brief` (or `--json`) + `stage list`, then
  announce vehicle. Do not decide inside pure `tick`. Sticky-open holds unless
  the operator asks otherwise. Detail: skill + `docs/agent-instructions/`.
- **Tick Law / Elm.** Reducers are pure `(Model, Msg) -> (Model, Vec<Cmd>)`.
  No I/O, clock, rand, env, or locks inside. Checklist:
  `docs/agent-instructions/effect-kernel.md`. Binding skill:
  `~/.agents/skills/effect-oriented/SKILL.md`.
- **Journal honesty.** Live effects journal before the provider runs
  (`EffectJournalMode::Enforced`). Dry arms are `Recording`. Same store backs
  effect rows and `RunJournal` lifecycle.
- **Identity ladder.** Explicit `--as`, then `AGENT_NAME`. Registered pins
  pass verbatim. Toron guard uses the same two rungs.
- **Bead create/close.** Create needs `-d`, `--verify`, and for P≤2
  `--principle`. Close is `br gate report` then `br close --commit-sha` with
  the bead id in the **full** commit message. Ceremony detail: br skill.
  Flywheel `ledger check` requires `sha=<40-hex>` in the gate note.
- **Tracker.** Agents use `br --json` / `--robot-*`. Bare `br` is the human TUI.
- **TDD. No script-based code edits. No file deletion without permission.**
- **Engineering principles.** `flywheel principles list`; cite the name that
  changed the decision.
- **Hosted captain (ADR-0019).** grokbot is pull-tier (`location=hosted` via
  rebind note). Hosted `steer` parks for `captain approve`. Never set
  `hook` / steer on the grok row from that ADR.
- **Skill sync.** CLI change → `Commands` enum + `--robot-capabilities` + this
  file's critical list + `~/.agents/skills/flywheel/SKILL.md` §5.

## Effect law (tests)

A change is unfinished until: reducer tests feed one message log and assert
model + cmds; interpreter tests fake clock/services (no real sleep/network);
success / typed failure / defect / interrupt stay distinct; live effects
journal before invoke; retry keeps the same effect key; terminal states emit
no further cmds. **Prove each test can fail.** No ambient-state tests
(operator `~/.config`, wall clock, live daemon, cold-vs-warm cache). Cheap
fence = whole workspace member (`-p <crate>`); full clean-export suite at
integration HEAD for cross-crate contracts.

## Details (links)

- [Backends + config](docs/agent-instructions/backends.md)
- [Orchestration](docs/agent-instructions/orchestration.md)
- [Workflow composition](docs/agent-instructions/workflow-composition.md)
- [Durable loop](docs/agent-instructions/loop.md)
- [Effect kernel](docs/agent-instructions/effect-kernel.md)
- [Workflow authoring](docs/workflow-authoring.md)
- Sibling mailbox: `../toron/AGENTS.md`

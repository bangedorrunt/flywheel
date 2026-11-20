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
shim).

```bash
mbx +nightly-aarch64-apple-darwin test
bash scripts/verify-clippy.sh
# installed binaries change only by the captain, from a green proof's kept
# export, with that export's own script (captain-only guard, bd-0uzrq.6):
#   AGENT_NAME=<captain> FLYWHEEL_SOURCE=<kept>/flywheel TORON_SOURCE=<kept>/toron \
#     bash <kept>/flywheel/scripts/stack.sh install
flywheel stack status --check
# ADR-0022 rollout 6: front the plan, a live patch, and the live seat view
flywheel swarm plan <project> [spawn flags]      # byte-equal to spawn --dry-run --json
flywheel swarm apply <project> --patch <seat>.<field>=<value> [--count <role>=<n>] [--yes]
#   ADR-0022 D1: every apply is a plan EVENT first (the plan of record is the
#   source of the desired seats); a planned SPAWN needs --yes, exactly as
#   `spawn` needs it past the consent gate (bd-6setu.3/.17)
flywheel swarm status [<project>] [--json]       # live seats + plane status
flywheel retire <pin>... [--project p]            # one seat Retire + lease release + deregister
# after `br close`: bead id in HEAD, committed closed row, legal verdict
flywheel ship check <bead-id>
# one verb for the whole close ceremony (gate -> close -> sync -> release ->
# commit ONLY this bead's tracker rows -> check); idempotent, and another
# lane's dirty tracker row never blocks it; the receipt may prove the citing
# sha or any descendant of it (bd-0uzrq.22)
flywheel ship land <bead-id> --sha <40hex> --receipt <proof> --as <pin> [--repo <dir>]
#   --repo names the repo holding the citing commit and the receipt's specs when
#   the work lives elsewhere (symmetric with `br close --repo`, bd-8q3qf/7d60f7a6);
#   land retries the HEAD-lock race other lanes cause (bd-pgfqg)
# clean-export proof of one revision (no selector = the dirty tree's intended
# paths): needles, build, no-run, clippy, suite incl. tests/, workflow verify,
# formal, coverage, then changed mutation specs (cached by content key). A
# --sha proof runs that revision's own driver; one machine-wide slot; --keep
# keeps the export for the captain's install
bash scripts/clean-proof.sh --sha <rev> --mutations changed --keep --receipt /tmp/<dir>/r.json
# sub-second check that every mutation find= still matches exactly once, AND
# that each spec's TEST_FILTER names a real #[test] fn (bd-6setu.18: a filter
# that matches nothing makes a mutation read NO-RESULTS, not RED)
bash scripts/verify-needles.sh
# the commit-time gate: fence the STAGED index (git write-tree -> throwaway
# commit -> pinned-sibling export -> needles, -Dwarnings build, rustfmt on the
# staged .rs). Installed per clone by scripts/hooks/install.sh (core.hooksPath),
# so a caller staged without its callee dies before HEAD breaks for every lane
bash scripts/fence.sh --index
# bd-f888z: opencode wakes ride session.prompt (harness_api), not TUI typing —
# one route decision in harness_caps; API failure falls back to Inject; journal
# the route before invoke; msg id is idempotent per (watermark/mail, pin).
# Spawn mints ses-* into SpawnEntry.harness_session; never type over operator text.
# mutations alone; runs in its own scratch export, never edits your tree
bash scripts/verify-mutations.sh scripts/mutations/<spec>.spec.sh
# crew briefs (bd-0uzrq.4): a dispatched crew prompt IS a brief; the reply
# closes it and wakes the captain. A pane wake that cannot resolve the captain
# (a mail-only identity with no pane row) falls back to plane-P mail; both paths
# dead is a typed error that says the captain heard NOTHING (bd-ezlme).
# bd-bi0mv: prefer typed receipts — --receipt-json validates sha40/beads/green
# fence, --json carries them parsed, and crew land delegates to ship land.
flywheel crew inbox --open [--pin <pin>] [--project p] [--json]  # what nobody answered (--json carries parsed receipts)
flywheel crew done <brief-id> --receipt-json <file|-> [--project p]  # typed flywheel-crew-receipt.v1; --receipt "<text>" stays but is untyped
flywheel crew land <brief-id> --proof-receipt <proof.json> [--project p]  # lands every bead in a typed receipt via ship land
# a ring the pane already holds is not a failure: the mail is stored and the
# prompt exits 0 saying it is queued (bd-8wdgn) — a scripted send never reads a
# redundant wake as a dead one
# scripts/stack.sh and scripts/ship-check.sh remain compatibility wrappers
```

`mbx +nightly-aarch64-apple-darwin test --exclude-ignored` is invalid.
Live ignored tests need a multiplexer + mail (see Test sandbox).

## Pane plane (rex default, hc fallback)

`FLYWHEEL_BACKEND` / `[backend] multiplexer` ∈ `{auto,rex,superlogical,hc}`;
default is rex-primary with hc fallback. Every door reads ONE resolution from
`toron_core::backend::plane` (`resolve` pure, `connect` the only construction
site); flywheel supplies only its configured name + hc socket. A write replays
on hc only for `Connect` — a timeout may already have committed. Detail:
`docs/agent-instructions/backends.md`.

- Discover the connected rex server: `rex api list`, `rex api describe
  <method>`, `rex api call …`.

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

Tests that spawn the binary build the `Command` through
`tests`' `flywheel_test_sandbox::TestSandbox` (temp HOME/XDG/TORON_DATA_DIR,
scrubbed routing env, deny-shims first on `PATH`, a `Drop` that asserts no shim
was reached). `scripts/check-test-sandbox.sh` refuses any spawn that skips it.
`scripts/test-sandbox-baseline.txt` is RETIRED and stays empty: an entry is a
failure, not a permission, because there is nowhere left to put one.

The ONE carve-out is `tests/live_*.rs`: a live test measures the real plane, so
it cannot be sandboxed. The lint allows the name only when every `#[test]` in
the file is `#[ignore]`d AND the file is gated on `FLYWHEEL_LIVE_E2E`, so a live
test can never run by default. Anything else that spawns gets the sandbox.

## Critical overrides

- **Clean-export proof.** A green suite over a dirty tree is not a green tree.
  Proof = `git archive HEAD` into a scratch dir and build there. In a dirty
  worktree use `scripts/clean-proof.sh` (throwaway index, intended paths
  only). **Crew commit verb:** `flywheel commit <paths> -m <msg> [--repo <dir>]`
  commits exactly the named paths through a private `GIT_INDEX_FILE`, runs the
  repo's pre-commit fence on that tree, and advances HEAD by CAS retry; a
  neighbour's staged entries stay untouched. A shared index can sweep them into
  even `git commit -- <paths>`, so never stage-and-commit in two steps on a
  shared tree. **Prove your commit, not a worktree:** `clean-proof --sha
  <your sha>` (or a clean export of it) is the crew-facing fence and the
  default, because one shared tree is the rule; a per-pin worktree is not the
  proof's job. A syntax error in the dirty tree must not change the result.
  **Crew pre-mail check:** before you mail a sha, run `scripts/fence.sh
  --sha <your sha>`. It exports that commit with `proof_archive` (the same
  function clean-proof uses) and runs the proof's fail-fast prefix —
  `verify-needles`, `-Dwarnings build --workspace --all-targets`,
  `test --workspace --no-run`, `verify-clippy` — stopping at the first red.
  A green fence is necessary, not sufficient; the captain still proves and
  installs.
- **Classify first.** Session door: `arrive` → `briefing --json`; argv runs the
  work. `decide --json` classifies (`open` | `swarm`). Live fleets join loud,
  never double-compose — an invisible crew is not an empty project (check
  inbox, `.unanswered` first). Detail: skill + `docs/agent-instructions/orchestration.md`.
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
- **Fleet mutation is captain-only.** `upgrade`, `stack install`,
  `daemon start|stop|restart`, `doorbell start|stop|supervise`, and
  `scripts/stack.sh install` all ask `authority::guard_fleet_mutation`: a
  non-captain pin, or a crew pane with `AGENT_NAME` unset (every pane carries
  `FLYWHEEL_PANE`), is refused; `--operator` is the deliberate escape and the
  script fails closed when no binary can answer. bd-0uzrq.24.
- **Bead create/close.** Create needs `-d`, `--verify` (P≤2 `--principle`).
  Prefer `flywheel ship land <bead> --sha <40hex> --receipt <proof> --as <pin>`:
  it runs the gate, `br close`, `br sync`, the reservation release, and a
  tracker commit limited to THAT bead's hunks, then `ship check`; a rerun
  resumes. Manual equivalent: `br gate report` then `br close` (br skill), then
  `flywheel ship check <bead-id>`. The check refuses a bead absent from HEAD,
  an uncommitted row for THIS bead, or no legal verdict — other lanes' dirty
  tracker rows never block it.
  `flywheel ledger check` needs a bound verdict — `sha=<40-hex>` (fallback) or
  `task=`/`verdict=` (proved); br skill ceremony is SoT.
- **Tracker.** Agents use `br --json` / `--robot-*`. Bare `br` is the human TUI.
- **TDD. No script-based code edits. No file deletion without permission.**
- **Engineering principles.** `flywheel principles list`; cite the name that
  changed the decision.
- **Hosted captain (ADR-0019).** grokbot is pull-tier (`location=hosted` via
  rebind note). Hosted `steer` parks for `captain approve`. Never set
  `hook` / steer on the grok row from that ADR.
- **P2P Multiplayer mesh.** Peers coordinate directly without in-line captain
  bottlenecks via `flywheel peer {intent, request-release, release-response, review-request}`.
  Wakes format as `⟦p2p:<verb>⟧ from <sender> for <path> (wm <hex>) — run: toron next`.
- **Chie Memory preflight & lessons.** Preflight `flywheel memory check-trap <query/paths>`
  surfaces known failure modes during briefing and before claiming tasks. Record
  confirmed lessons via `flywheel memory record-lesson <content>`.
- **TypeSafe AI System One arbitration (ADR-0016).** `p2p-conflict-resolution`
  (Choice) and `peer-review-severity` (Score 1..4) provide sub-100ms structured
  advisory evidence without LLM hallucination. Advisory only; pure tick holds.
- **Statechart topology checks (ADR-0021 D5).** `flywheel workflow verify [NAME]
  [--export-tla] [--json]` (`flywheel-workflow-verify.v2`) checks the remaining
  hand-written charts (`src/elm/p2p_negotiation.rs`, `seat_statechart.rs`,
  `plan_statechart.rs`) and every catalog workflow (wave-swarm included; the
  retired `swarm_statechart.rs` is verified as that workflow now): every state
  reachable, every reachable leaf can reach its goal. Guards count as enabled
  and data is not modelled, so a pass is not a proof; `--export-tla` text is
  topology-only and no model checker runs. `scripts/clean-proof.sh` gates on it.
- **Standing preferences (ADR-0022 D6).** `flywheel prefer <role.field>=<value> [--project <p>]` +
  `--list [--json]` / `--forget <role.field>`; project beats always, exact role beats `*`.
- **Live seat change (ADR-0022 D7/rollout 6).** One mechanism: `flywheel swarm apply
  <project> --patch <seat>.<field>=<value> [--count <role>=<n>]` → pure reconciler →
  the one `Ports::seat` interpreter, journal-first (respawn journals one `RebindIntent`;
  Spawn runs within `crew.max`; InjectRole is one prompt turn and never for
  review/verify seats). `binding-rebind --model` routes here; never a second apply
  path. `swarm plan` is byte-identical to `spawn --dry-run --json`; `swarm status`
  lists live seats. `SpawnEntry` carries effort/mode so a second reconcile is Keep.
- **Retire one pin (bd-rilvn).** `flywheel retire <pin>... [--project p] [--force-working]`
  is the thin verb over the seat path: one journaled `SeatEffect::Retire` through
  `Ports::seat`, the pin's leases released operator-signed (by HOLDER, so a
  sibling keeps its lease), then the row deregistered. Refuses the captain seat
  and a working pin without `--force-working`. Never a second retire path.
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

# AGENT INSTRUCTION

## Commands

```bash
mbx +nightly-aarch64-apple-darwin test
bash scripts/verify-clippy.sh
k
mbx +nightly-aarch64-apple-darwin install --path . --force
mbx +nightly-aarch64-apple-darwin install --path crates/doorbell-cli --force # plane D CLI (bd-s4fc: proxy resolves it co-located or on PATH)
# macOS (only if a stale-signature SIGKILL appears): codesign --force --sign - ~/.cargo/bin/flywheel
```

Rust runs through mbx (cargo cache): prefix every cargo invocation with `mbx +nightly-aarch64-apple-darwin` (bare `cargo` misparses under the mbx shim).

Live ignored tests need herdr + mail. `mbx +nightly-aarch64-apple-darwin test --exclude-ignored` is invalid.

## Critical overrides (repo invariants — ops live in the flywheel skill)

- **Classify first (ADR-0011):** session door is `arrive` then `briefing --json`, argv runs the work; `decide --json` is the classifier leaf (`topology`, `src/decide.rs`). Live fleets join loud, never double-compose.
- **Catalogs (ADR-0032 wave 1, landed):** one routing table (`src/decide.rs` `ROUTING_TABLE`: earliest-match position, explicit `none`, topology per row) routes `decide`; one workflow catalog (`src/workflow_catalog.rs`: built-ins < user < project tiers) resolves `workflow list` and sub-workflows. `briefing` consumes decide's output. Run ledger rows carry terminal status + real project + failure artifacts; `workflow history` never defaults to `done`.
- **Deps:** flywheel → toron-workflow → toron-core. Never reverse.
- **Mail:** `toron_core::mailbox::surface::MailSurface`. Local `MailBackend` is deleted. Absent `[mail.toron] url` means embedded toron.
- **Identity is one ladder** (bd-reservation-identity-tlnf): explicit `--as` → `TORON_AGENT` → `FLYWHEEL_MAIL_AS` → `AGENT_NAME` → config `[agent_mail].name`, spawn-registry-resolved. Registered pins pass through verbatim (bd-2mdo); the hash fallback is for unregistered herdr slugs only. Leases bind the resolved actor and the strict guard attributes commits through the same order. Self-test: `mbx +nightly-aarch64-apple-darwin test reservation_identity actor_resolution_passthrough`.
- **One operator, one npub** (bd-register-portability-3c2b): `mail register` requires `--name` and binds via `--from-project <slug>` / `--adopt-unnamed <name>` instead of forking. Bare re-registration errors naming the fix.
- **Model routing is strict registry routing:** composite `provider/model` keys, `ModelRegistry::load_routing()` + `resolve_strict()`, caps enforced; unregistered or caps-excluded ids fail the spawn naming the fix. Slot/profile/operator-default semantics live in the flywheel skill; this repo owns the resolver code.
- **Bead close is two-step and policy-enforced** (bd-ut8l): `br gate report` first, then `br close --commit-sha` citing the bead; `--force` variants need `BR_OPERATOR=1` + `--bypass-reason`. The gate row's `--note` must carry `sha=<FULL 40-hex sha>` (bd-u5yr): `flywheel ledger check <id> --dir .` matches that token exactly against HEAD, so a note-less or abbreviated row is unbound and reports clean-false, a verdict is green only at its own revision, and a closed bead can never be re-bound (`closed -> closed` is not a legal gate transition). Ceremony details live in the br skill.
- **Tracker:** `br` CLI + `--json`; bare `br` is the human TUI, never automation. `bv` is deprecated.
- **TDD. No script-based code edits. No file deletion without permission.**
- **Engineering principles (ADR-0019 Spec 5/6):** `flywheel principles list`; cite the name that changed the decision.
- **Compound spine (ADR-0009):** `ce-*` stages, `compound` phase, per-project `docs/learnings/`; `p-code` aliases are retired (`ce-execute`, not `p6`).
- **Preflight:** `steer`/`loop-mode run`/`workflow run --execute` auto-run prerequisite checks; `--skip-preflight` bypasses; standalone `flywheel preflight --mode {steer|loop|workflow|cron}`.
- Wave/swarm/doorbell/crew-control semantics live in the flywheel skill; the Tick Law (`workspace::tick` owns the live path, slice reducers private) holds on every path touching `src/workspace.rs`.
- **Tick Law is Elm law (bd-bpj1):** reducers (`workspace::tick`, `doorbell-core::tick`, `durable::tick`) are pure `(Model, Msg) -> (Model, Vec<Cmd>)` — no I/O, clock, rand, or `&mut` inside; `now` enters only as a Msg payload; maps iterated for output are `BTreeMap`, never `HashMap`; the effect layer never mutates the model, I/O results re-enter as Msgs; every drop is journaled (queue transitions, watch dedups). New reducer paths carry a same-log-same-effects test; `doorbell why` stays mismatch-free on current journals. Review rejects reducer bypasses, wall-clock reads, and silent drops.

## Public API
- Keep the public API surface as small as possible. Use the narrowest visibility that satisfies the current requirement.
- Add public types, traits, methods, features, or re-exports only for a concrete user need.
- Preserve backward compatibility unless a breaking change is explicitly required.
- Document public API changes and update the affected crate's changelog when appropriate.
- Respect the workspace MSRV, feature boundaries, and no_std support where applicable.

## Performance

- Code must remain efficient on low-end and resource-constrained devices.
- Keep CPU, memory, allocation, storage, and network costs proportional to the work performed.
- Avoid unnecessary cloning, repeated parsing or serialization, redundant I/O, and unbounded collections, queues, tasks, or retries.
- Benchmark performance-sensitive changes when practical, and investigate meaningful regressions.
- Prefer clear, efficient code over speculative micro-optimizations. Explain non-obvious optimizations and their tradeoffs.

## Skill Sync Contract

When you add/remove/rename a CLI command, update ALL of:
1. The `Commands` enum (`src/main.rs`) — ground truth (compiles).
2. The `robot_capabilities` JSON (`src/main.rs`, `--robot-capabilities`) — agent self-discovery.
3. This AGENTS.md (the Critical overrides + Details sections).
4. The skill: `~/.agents/skills/flywheel/SKILL.md` §5 Commands.

The skill files are the canonical agent-facing reference. §5 is an ops crib;
the full enum is `flywheel --help` and `flywheel decide`. A 0027 waiter/receipt/role
verb not in the toron skill is invisible. Do not paste every clap variant into the skill.

## Details

- [Backends + config](docs/agent-instructions/backends.md)
- [Orchestration](docs/agent-instructions/orchestration.md)
- [Durable loop](docs/agent-instructions/loop.md)
- Workflow DSL: [docs/workflow-authoring.md](docs/workflow-authoring.md)
- Sibling mailbox: `../toron/AGENTS.md`

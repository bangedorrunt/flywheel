# AGENT INSTRUCTION

## Commands

```bash
mbx +nightly-aarch64-apple-darwin test
bash scripts/verify-clippy.sh
k
mbx +nightly-aarch64-apple-darwin install --path . --force
mbx +nightly-aarch64-apple-darwin install --path crates/doorbell-cli --force # plane D CLI (bd-s4fc: proxy resolves it co-located or on PATH)
# macOS (only if a stale-signature SIGKILL appears): codesign --force --sign - ~/.cargo/bin/flywheel
# Same for the daemon after `cargo install` — a CODESIGNING SIGKILL shows up as the
# planned `doorbell serve` vanishing mid-tick with an empty log and a
# ~/Library/Logs/DiagnosticReports/doorbell-*.ips saying "Code Signature Invalid":
# codesign --force --sign - ~/.cargo/bin/doorbell && launchctl kickstart -k gui/$(id -u)/com.doorbell.serve
```

Rust runs through mbx (cargo cache): prefix every cargo invocation with `mbx +nightly-aarch64-apple-darwin` (bare `cargo` misparses under the mbx shim). `test` runs all four workspace packages (flywheel + doorbell-core/rt/cli) via `[workspace] default-members`, so a doorbell-crate regression is not invisible to the default command.

Live ignored tests need herdr + mail. `mbx +nightly-aarch64-apple-darwin test --exclude-ignored` is invalid.

## Critical overrides (repo invariants — ops live in the flywheel skill)

- **Classify first (ADR-0011):** session door is `arrive` then `briefing --json`, argv runs the work; `decide --json` is the classifier leaf (`topology`, `src/decide.rs`). Live fleets join loud, never double-compose. `briefing.world.ready_beads` is `br ready` (dispatchable), never jsonl `open` lanes. Leftover opens must not steal diagnose/forensics/beads-lifecycle. `diagnose` routes to `forensics`. Loop-work needles are phrases (`work the ready beads`, `ready beads`, `loop-mode`, `loop`), never bare `work`/`beads`.
- **Mid-session material reclassify (GS-048 / GS-049):** on AUTHORITATIVE objective change, explicit topology ask, new tracked work-unit class, classifier miss while working, or stage-chain exhaustion then continued asks — **must** re-run `decide --json` + `workflow list --brief` (or `--json`) + `stage list`, then announce `vehicle: …` or `none: raw`. Tick Law path: `Msg::MaterialContext` → caller-applied `Cmd::Reclassify` → `Msg::DecisionReady` announce. Sticky-open: do not flip open→loop/swarm/join/none without explicit request; hold loop against swarm without explicit. After sticky announce, briefing print/commit **aligns** with `session_topology` (no dual SoT); sticky-held open clears invent workflow/argv (no invent `wave-swarm` headless under open) and DecisionReady announces `none: raw` / open vehicle only. `steer` requires `--project` (or inherits cwd) so AUTHORITATIVE objectives always queue MaterialContext. `live_decide_snapshot` uses real assemble `launch_pending` (never invent-green 0). Routine Channel/Job doorbell wakes stay Prompt-only. `flywheel reclassify "<goal>"` enters Elm-plane MaterialContext (sticky-open) then prints briefing. Do **not** decide inside pure `tick`.
- **Elm deepen (GS-045 P1 / GS-049 / GS-050):** source Msg / source-action identity is immutable through tick (retries keep the same Reclassify goal/reason **and** journal `effect_key` — ADR-0032 I8a; `Cmd::Reclassify.effect_key` is durable I8a journal identity — length-prefixed `reclassify:<sha256>` via `reclassify_effect_key`; host `expand_reclassify_cmds` insert-or-loads Prepared→InFlight→Settled under that key). `workflow evaluate <name>` returns validated partition candidates without journal dispatch (evaluate-without-commit / dry evaluate); live effects stay on `workflow run --execute`. Structured failures carry ActionStage (`stage`: normalize|input|execute|output|effect|park) through CLI/run ledger. Sensor edges (WatchNdjson / Herdr / AckSla / InboxSweep) are Msg producers — **no** Sensor GenServer (docs vocabulary only). **Presence** is via Herdr status + roles (and the trusted dispatcher-internal `Msg::Presence` edge), not a Sensor GenServer arm.

- **Catalogs (ADR-0032 wave 1, landed):** one routing table (`src/decide.rs` `ROUTING_TABLE`: earliest-match position, explicit `none`, topology per row) routes `decide`; one workflow catalog (`src/workflow_catalog.rs`: built-ins < user < project tiers) resolves every `workflow` verb — `list/show/lint/crew/run/compose` all take `--project` and layer the project tier (`bd-workflow-catalog-read-doors-project-tier-jah1`). `briefing` consumes decide's output.
- **Run ledger is honest at every door (bd-run-ledger-truth-readers-j6pg):** one reader rule (`history_row_status`) backs `workflow history`, `runs list`, `--robot-snapshot`, and `cron history` — a row that recorded no status reads `unknown`, never a default `done`. A row that annotates a run rather than recording one declares itself (`annotation_kind`): ship-chain phase gates, the legacy loop monitor's start notice, the warn-first plan-artifact gate. Those carry `kind` + their real verdict, are skipped by `cron history`, and never count as run outcomes. Run rows carry terminal status + real project + failure artifact paths.
- **Run-path effect journal (ADR-0032 Spec 5/6, bd-engine-run-path-journaling-fwlx):** flywheel embeds the engine, so flywheel owns the journal — `src/effect_journal.rs` injects a host-owned `EffectRunner` into every engine built for a run that really dispatches (`workflow run --execute`, `compose --run`, `cron run --live`, the cron daemon): an effectful `job`/`mailbox` leaf commits prepared intent → claims the attempt → invokes its provider **once** → settles (or fails, or records `unknown`), keyed by the engine boundary's own derivation. A sink no provider performs is **not** journaled — it keeps the engine's `bypassed-sink-uncovered` / `bypassed-no-effect-runner` marker with a loud reason. Dry arms (`run` without `--execute`, `cron run` without `--live`) stay unjournaled on purpose: they perform no external effect, so a settled row would claim work nobody did. Mailbox leaves journal only on the in-process plane (a remote `[mail.toron] url` cannot carry `corr`).
- **The journal mode is the honesty switch (bd-engine-run-path-journaling-fwlx):** a journaled constructor is `EffectJournalMode::Enforced` — an effectful leaf with no injected runner **refuses**, it never dispatches unjournaled; a dry arm is `Recording` — the marker, not a refusal. `verify` journals through the same surface: the engine renders the command (a templated `{{inputs.verify}}` reaches the sink resolved), runs the embedding's `VerifySink` first, and hands the executed result via `RunContext.provider_result` — the sink is the provider, so the leaf runner is not invoked and the command is never run twice. With no toron index open the engine keeps the loud marker, never a run that only looks journaled.
- **Deps:** flywheel → toron-workflow → toron-core. Never reverse.
- **Mail:** `toron_core::mailbox::surface::MailSurface`. Local `MailBackend` is deleted. Absent `[mail.toron] url` means embedded toron.
- **Identity is one ladder** (bd-reservation-identity-tlnf): explicit `--as` → `TORON_AGENT` → `FLYWHEEL_MAIL_AS` → `AGENT_NAME` → config `[agent_mail].name`, spawn-registry-resolved. Registered pins pass through verbatim (bd-2mdo); the hash fallback is for unregistered herdr slugs only. Leases bind the resolved actor and the strict guard attributes commits through the same order. Self-test: `mbx +nightly-aarch64-apple-darwin test reservation_identity actor_resolution_passthrough`.
- **One operator, one npub** (bd-register-portability-3c2b): `mail register` requires `--name` and binds via `--from-project <slug>` / `--adopt-unnamed <name>` instead of forking. Bare re-registration errors naming the fix.
- **Model routing is strict registry routing:** composite `provider/model` keys, `ModelRegistry::load_routing()` + `resolve_strict()`, caps enforced; unregistered or caps-excluded ids fail the spawn naming the fix. Slot/profile/operator-default semantics live in the flywheel skill; this repo owns the resolver code.
- **Bead create is fail-closed** (bd-h8d6): `br create` requires `-d/--description`, `--verify '<cmd>'`, and for P≤2 `--principle 'name — decision'`. Title-only create refuses. `br q` is quick-capture only.
- **Bead close is two-step and policy-enforced** (bd-ut8l): `br gate report` first, then `br close --commit-sha` citing the bead in the **full** commit message (`git log -1 --format=%B`); a trailing `.` or `:` is punctuation, not glue (bd-qf99). `--force` variants need `BR_OPERATOR=1` + `--bypass-reason`. The gate row's `--note` must carry `sha=<FULL 40-hex sha>` (bd-u5yr): `flywheel ledger check <id> --dir .` matches that token exactly against the revision `derive_head_sha` resolves — `git log --all -1 --grep=<id>`, NOT bare HEAD — so a note-less or abbreviated row is unbound and reports clean-false, a verdict is green only at its own revision, and a closed bead can never be re-bound (`closed -> closed` is not a legal gate transition). `derive_head_sha` walks the `--grep` matches newest-first and **skips bookkeeping-only commits** (`.beads/**`, `.flywheel/**` — nothing outside those two), so the close ceremony's own gate-row commit cannot move the bound revision; it still binds the newest commit carrying the id AND carrying work, and falls back to the newest bookkeeping match when the id has no work commit at all. Belt and braces: keep the bead id out of the bookkeeping commit message anyway (the hook exempts `.beads` from both its reservation and its bead-token rules, so a token-free message commits fine) — a skip is a heuristic, a clean message is a fact. Ceremony details live in the br skill.
- **Tracker:** `br` CLI + `--json`; bare `br` is the human TUI, never automation. `bv` is deprecated.
- **TDD. No script-based code edits. No file deletion without permission.**
- **Engineering principles (ADR-0019 Spec 5/6):** `flywheel principles list`; cite the name that changed the decision.
- **Compound spine (ADR-0009):** `ce-*` stages, `compound` phase, per-project `docs/learnings/`; `p-code` aliases are retired (`ce-execute`, not `p6`). `ce-evaluate` is the verify-phase, fresh-context, read-only evaluator used by repair loops; its result is advisory, never the host verdict.
- **Host evaluation evidence:** ship-chain phase contracts persist bounded redacted `EvaluationEvidence` under the parent run. `phase_contracts`/`VerifySink` derive the verdict; `artifacts stitch <run_id>` renders the evidence; missing persistence fails the gate (warn mode may continue as `gate-warnings`). Worker `criterion_status` and `claim_support_status` never self-admit artifacts.
- **Upstream parity:** `docs/upstream-parity.toml` pins the checked refs and local witnesses for Compound Engineering and pstack. The offline `upstream_parity` tests and `--robot-capabilities` summary are the parity doors; network refresh is manual, never a build dependency.
- **Workflow selection:** the built-in catalog is 40 workflows. Agents should start with `flywheel workflow list --brief --json` (GS-052 hybrid: **name / family / selection / run / topology / compose_token**), pick the smallest match, then bare `workflow list --json` for description + decide_trigger + rest, or `workflow show <name>` for prose/graph detail (no decide_trigger/family/selection/topology); the one-by-one review lives in `docs/guides/workflow-catalog-review.mdx`. This applies in open mode and on every follow-up/material context change, not only session start; preserve open topology unless loop/swarm is explicitly chosen.
- **Recursive workflows:** durable `call_workflow` children carry serialized lineage and fail closed before child creation on cycles, depth over 8, or exhausted 32-child per-lineage budget. Nested workflow use is bounded execution, not permission to spawn from open mode.
- **Preflight:** `steer`/`loop-mode run`/`workflow run --execute` auto-run prerequisite checks; `--skip-preflight` bypasses; standalone `flywheel preflight --mode {steer|loop|workflow|cron}`.
- Wave/swarm/doorbell/crew-control semantics live in the flywheel skill; the Tick Law (`workspace::tick` owns the live path, slice reducers private) holds on every path touching `src/workspace.rs`. `src/cli_args.rs` and `src/workspace_reporting.rs` are pure ADR-0016 seams; compatibility wrappers must not become new policy authorities.
- **The journal has two halves, one store (bd-mi9n, ADR-0032 Spec 5/6 tail):** `effect_journal::journals` builds the effect runner AND a `RunJournal` from one store. The engine reports its own run lifecycle (`Started` → `Running` → `Succeeded`/`Failed`) through the `RunJournal`, so a flywheel-driven run has a `workflow_runs` row with no embedding lifecycle call, and the run row and its effect rows agree on `run_id` by construction. Idempotent by design (durable replay re-delivers phases): create-if-absent, CAS only when the status differs, terminal rows never move. Every journaled constructor passes it (`pane.rs` live arms, both `cron.rs` live engines); dry arms pass `None` — an engine that performs nothing must not mint a row claiming it ran.
- **Tick Law is Elm law (bd-bpj1 / ADR-0016):** reducers (`workspace::tick`, `doorbell-core::tick`, `durable::tick`) are pure `(Model, Msg) -> (Model, Vec<Cmd>)` — no I/O, clock, rand, environment, locks, or `&mut` inside; `now` enters only as a Msg payload; host policy enters as model data (`WorkspacePolicy`), never through env-reading helpers; maps iterated for output are `BTreeMap`, never `HashMap`; the effect layer never mutates the model, I/O results re-enter as Msgs; every drop is journaled (queue transitions, watch dedups). New reducer paths carry same-log-same-effects coverage and malformed/unknown cases; `doorbell why` stays mismatch-free on current journals. Review rejects reducer bypasses, ambient authority reads, silent drops, queue-as-delivery claims, second live models, and unchecked action-stage literals at typed call sites. The full laws/checklist live in `docs/agent-instructions/effect-kernel.md`.

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
- [Compound/pstack deep dives](docs/guides/compound-engineering.mdx), [pstack](docs/guides/pstack.mdx), [parity manifest](docs/upstream-parity.toml)
- [Durable loop](docs/agent-instructions/loop.md)
- [Functional effect-kernel discipline](docs/agent-instructions/effect-kernel.md)
- Workflow DSL: [docs/workflow-authoring.md](docs/workflow-authoring.md)
- Sibling mailbox: `../toron/AGENTS.md`

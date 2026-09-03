# flywheel

herdr swarm orchestrator. NTM-parity CLI + Resonate durable loop. Embeds
toron (`MailSurface`); never the reverse.

## Commands

```bash
cargo test
bash scripts/verify-clippy.sh
bash docs/decisions/_scripts/verify-review-fixes.sh
cargo install --path . --force
# macOS (only if a stale-signature SIGKILL appears): codesign --force --sign - ~/.cargo/bin/flywheel
```

Live ignored tests need herdr + mail. `cargo test --exclude-ignored` is invalid.

## Critical overrides

- **Classify first:** `flywheel decide "<target>" --json`. Run `argv`. Do not invent a second selector.
- **Deps:** flywheel → toron-workflow → toron-core. Never reverse.
- **Mail:** `toron_core::mailbox::surface::MailSurface`. Local `MailBackend` is deleted. Default mail = toron (absent `[mail.toron] url` → embed).
- **Identity resolution is ONE ladder everywhere** (bd-reservation-identity-tlnf): explicit
  `--as` → `TORON_AGENT` → `FLYWHEEL_MAIL_AS` → `AGENT_NAME` → config `[agent_mail].name`,
  spawn-registry-resolved. A selected token naming a REGISTERED mailbox identity passes
  through verbatim (bd-2mdo) — the hash fallback is for unregistered herdr slugs only.
  Both sides of the seam use it: `mail locks reserve/release/renew`
  bind the lease holder to the RESOLVED actor (never the raw slug), and the toron strict guard
  attributes commits through the same order — so a wrapper-acquired lease always covers its own
  holder's commits. Self-test: `cargo test reservation_identity actor_resolution_passthrough`.
- **One operator is one npub across projects** (bd-register-portability-3c2b): `mail register`
  requires `--name` (it never silently mints an unnamed key) and binds instead of forking via
  `--from-project <slug>` (reuse that project's key for the pin) or `--adopt-unnamed <name>`
  (move a prior unnamed mint into a named slot). A returning operator registering bare gets an
  error naming these fixes — never a second identity.
- **Tracker:** `br` CLI + `--json`. No beads MCP. Bare `br` **is** the TUI (ratatui, since 2026-08-24) — humans run `br`, agents run `br --robot-*` / `br triage|next|plan` with `--format json|toon` (never bare `br` in automation). `bv` is deprecated. Mode commands must ask for the profile/model routing before launching managed work; show the profile default and compatible registry alternatives, and preserve the operator's selection.
- **Bead close is two-step and policy-enforced (bd-ut8l lesson):** record the verdict first — `br gate report <id> --gate unit-test-verified --provider <pin> --status pass --to closed` — then `br close <id> --commit-sha <sha>` (the sha's commit message must cite the bead id). Without a legal gate row the close fails; `--force` variants need `BR_OPERATOR=1` + `--bypass-reason`. Crews that commit a fix but skip the close leave the bead claimed and block dependents — wake them with "finish or release".
- **TUI theme (hacker-night, tui-design skill):** Primary theme is **Ghostty v2 hacker-night** (`#08080e` void / `#c8d0e8` fg / `#0db9d7` cyan / `#bb9af7` lav / `#73daca` mint / `#e0af68` amber / `#f7768e` red / `#89ddff` ice / `#1a1a3e` selection). Single source per crate: `flywheel/src/output.rs` (Slot enum: `paint_slot` with truecolor→256→16 degrade), `toron-cli/src/theme.rs` (`HackerNight`), `beads/src/tui/theme.rs` (same palette, `NO_COLOR`/`TERM=dumb` → `Reset`, `COLORTERM=truecolor` → `Rgb`). Widget code references **semantic slots** only — never hardcodes hex/ANSI (skill §4). Bare `br` TUI lives in `beads/src/tui/{mod,app,ui,keys,theme}.rs`; `toron tui` lives in `toron-cli/src/tui.rs` (`toron-cli/src/theme.rs`); `flywheel` viewer is `view`/`watch`/`status` + `steer_brief.rs` (ANSI-painted, alt-screen sanitized, subscription+poll fallback). Use `tui-design` skill before touching any TUI: layout (§1), responsive (§2, 80×12 min gate, 100-col split, 80-col board stack), interaction (§3, vim `j/k g/G`, `?` help, `[q]uit [/]search [?]help [Tab]focus`), visual hierarchy (§4, bold/dim + symbol, never color alone), and anti-patterns §7. Test at 80×24 / 120×40 / 200×60, inside tmux, with `NO_COLOR=1` and `COLORTERM=truecolor`.
- **Kill:** `--force` skips confirm. Working agents also need `--force-working`, or `--drain` for the graceful two-phase stop (asks each working agent to finish its VERIFY fence via `drain_prompt`, records drain on `ActiveLoop.drain_pending` — the poll seeds `ws.swarm.drain_pending` from it and tick enforces 3-poll grace, past grace → Respawn+LoudMiss with the bead id; skips the close). `--reap-crews` overrides `on_captain_death` (ADR-0025 Spec 4).
  - **Artifacts + loop journal (ADR-0027 one-workspace tick, Tick Law):** `flywheel artifacts <run_id> [--json]` / `stitch`; `flywheel loop replay <project>` (re-drives `Msg` journal through `tick`, sequence-equal) / `flywheel loop why <project> <seq> --corr <id> --seq <n>` + `flywheel doorbell why <project> <pin> --corr <id> --seq <n>` (both print `msg`/`state-delta`/`Vec<Cmd>` triple by folding `tick` from `Default` over `state_dir/journals/doorbell-journal-{workspace}.jsonl`; `seq` is `WorkspaceModel.seq`; replay asserts `Cmd` sequence — not pin-substring grep). Poll path journals every `Msg` under `state_dir/journals/doorbell-journal-{workspace}.jsonl` (not `doorbell/<ws_hash>.jsonl`). **One reducer is the law (ADR-0027) and is held on the live path:** doorbell dispatcher and `swarm_loop` both call `workspace::tick`; `doorbell_tick::tick` / `durable::tick` are `pub(crate)` slice reducers. Reclaim is a Msg. FetchInbox Cmd is the sweep. Fact ticks the live model. Tests: `cargo test adr0027_s1` (S1/S2), `cargo test role_model` (roles), `cargo test presence_model` (presence), `cargo test --workspace adr0027_s2` (S3), `cargo test adr0027_s3` (S4/S5 + secrets tripwire).
- **Execution topologies:** `open-mode` = direct current-pane work with no lifecycle; `loop-mode` = durable solo execution in one current harness (`--work` opts into tracked work); `swarm-mode`/`steer` = captain-led multi-crew execution. A workflow is a reusable procedure, not a topology. `workflow run` without `--execute` records control-flow only (no pane dispatch). `--mode` default is headless. `/swarm-mode` pins `compose --parts wave-swarm --run`. `/loop-mode` pins `loop-mode run <project> --work`.
- **`cron run`** is a 25s dry-run by default; `--live` force-fires once
  through the durable-loop runner (real dispatch). Recurring ticks are
  `cron start`.
- **compose --agent** is wired (planner YAML → lint). Compose arms:
  `--parts` (grammar + stage-first), `--from-yaml -|<file>`, `--dry-run`,
  `--run --keep <name>` (green-run graduation). Composed runs bounded by
  `FLYWHEEL_COMPOSE_MAX_SECONDS` (default 3600).
- **Spawn:** `--profile` (strict, composite `provider/model` registry id — e.g. `opencode-go/deepseek-v4-flash` for coder) is the primary; bare harness+model `spawn --model <id> [--provider <p>]` is the lenient fallback for kind/count spawns without a named profile (registered ids route via `provider/model` composite with default-provider fallback; unknown ids pass through verbatim; `--provider` requires `--model`, bd-j3so). Never `add --oc` when a profiled pane exists.
- **Harness recipes (H1, bd-harness-prime-agent-818f):** a profile whose harness is NOT in herdr's closed kind set (e.g. `pa` = prime-agent) spawns via the compiled-in `[harness.<name>]` recipe: on `unsupported_agent_kind` from `agent.start`, `start_agent_with_retry[(_dyn)]` falls back to `pane.run` the recipe (+ shell-quoted model/effort args) and waits/renames. prime-agent's built-in herdr socket extension self-reports lifecycle; `agent.prompt` stays blocked upstream until herdr ships the kind (discussion herdrdev/herdr#2840).
- **Harness integration matrix (ADR-0019 wave, bd-skills-harness-hardening-8mto):** lifecycle-authority kinds (pi/omp/kimi/opencode/kilo/mastracode+grok), env injection (HERDR_ENV+HERDR_SOCKET_PATH at pane creation), universal boot prime via pane.send_text, integration preflight, pane primitives, alt-screen read ladder → canonical in `~/.agents/skills/flywheel/SKILL.md` §7; herdr client details in `~/.agents/skills/toron/SKILL.md` (Backend section).
  - **Peer-aware spawns BY DEFAULT (ADR-0027 one-workspace tick, ONE reducer):** `spawn --profile` starts one workspace doorbell dispatcher + injects the handler template (`integrations/peer-aware.md.tpl`) with the pin (`mail_identity`). Dispatcher is Elm-shaped: every bus/herdr event becomes a `workspace::Msg` and `workspace::tick` returns `Cmd`s (doorbell/durable ticks are private slices). `apply` maps `Cmd` to herdr/toron I/O (Prompt, SqliteUpsert, FetchInbox→InboxRows, Subscribe, WaitCorr). Wake is `[doorbell]` via `herdr agent.prompt`. Inject reads `Model.presence` (20001), herdr as confirm. After inject the pin has **120s** to show heard (presence or kind-9); else digest `doorbell: unanswered pin=…` (deaf pane). Budget 5/30/3 is dispatcher-enforced. A 5-min self-poll is safety-net only. `flywheel doorbell why <project> <pin> --seq <n> --corr <id>` replays by folding `tick` from `Default` over `state_dir/journals/doorbell-journal-{workspace}.jsonl`. Kind/count spawn is the labeled escape (`spawn --escape`); default multi-lane is `workflow compose --parts wave-swarm --run`.
- **Send:** `--to <full-name|coder1|omp1|pane_id>`. Never bare `--to oc`. `launch_pending` is not idle. `agent_not_ready` → retry pane id. Do not flip omp defaults.
- **Stage payloads (ADR-0006 D1):** `send --stage <name> --param k=v` (repeatable) substitutes
  `{{k}}` slots in the stage template; reserved slots `{{bead}}/{{scope}}/{{exit}}/{{channel}}/{{pin}}/{{report_file}}`.
  A stage's declared required params missing → non-zero exit naming the slot. Zero `--param`
  renders byte-identical to the unparameterized prompt.
- **Wave-scoped dispatch (ADR-0006 D2):** `loop-mode run --work --wave-label wave-N` = ready ∩ exact
  label, held with named blockers while any open/in_progress bead carries wave-M (M<N). Default
  `--work` unchanged. Label-based hold is transitional (TODO → beads ADR-0001 schema-18 typed field).
- **wave-swarm recipe (ADR-0006 D3 + §9 addendum):** `workflow compose "<objective>" --parts wave-swarm
  [--inputs '<json>']` expands GATE→DISPATCH→EXECUTE→OBSERVE→VERIFY→SIGNOFF over the compiled-in
  `wave-*` stages (EXECUTE landed 9ad47e9 — dispatch-only crews never touched the bead; `--inputs`
  carries captain_pin etc., b800381/d2d6f3e); SIGNOFF suspends the run on `request_approval`
  (captain-only by construction). Bounded by `FLYWHEEL_COMPOSE_MAX_SECONDS`.
- **Control-plane vocabulary (ADR-0028 E7, post-swarm truth):** sensors are `Msg` producers only — `WatchNdjson` (bus watch), `HerdrStatus` (pane lifecycle), `AckSlaTick` (60s clock), presence-20001, `Sweep`→`FetchInbox`→`InboxRows` (plane-P sweep). No Sensor module. `WorkspaceModel` partitions: perception (`presence`, `roles`), assignment (`corr` receipts, `pending_wait`), swarm (heal/dispatch), human (`open_human`). No second Memory store (Model + beads + receipts sqlite only). Strategy stays four nouns (stage/workflow/recipe/profile); outcomes → `VerifySink` + beads; Branch lives in the toron engine, tick never branches on YAML. Timers stay `Msg` producers (no 30015/wake kind). Action schema: per-arm invariants in `src/workspace.rs` `Msg`/`Cmd` docs — `Publish` is rumor-14 + `corr`, `WaitCorr` carries non-empty `corr`, malformed tags → `LoudMiss` (send-side `SendTags` hard-errors). Post-swarm contracts: corr authority (sender mints, `thread_id`≠`corr`), presence trust (self-report or `Msg::Presence`; Unknown never prompts), correlated HITL (`Publish` corr → answer `--corr`/`--verdict` → one `HumanDecision`), honest `why`-fold (journal `effect` + crash-resume seed). `pending_wait` drains on receipt (doorbell-only); explicit TTL+cap is still open.
- TDD. No script-based code edits. No file deletion without permission.
- **Model routing is STRICT (nothing bypasses the registry):** profile models are composite `provider/model` registry ids (keys are `provider/model` — e.g. `[models."opencode-go/deepseek-v4-flash"]`); `ModelRegistry::load_routing()` (compiled-in defaults + user `models.toml`) + `resolve_strict()` add the provider prefix and enforce `caps`. Bare `[models.<id>]` keys fail loudly naming the rewrite; `Profile.provider` pins the composite key (unknown provider fails naming `qqq`/`glm-5.2`), bare ids fall back to the default provider (user-file entries first, else alphabetically-first key). An unregistered id or caps-excluded harness fails the spawn naming the fix. big-pickle is opencode-only (scout). Explicit ad-hoc `--model` keeps lenient passthrough. Dot-bearing ids must be quoted in models.toml (`[models."glm-5.2"]`).
- **Engineering principles (ADR-0019 Spec 5/6):** `flywheel principles list`. Cite the name that changed the decision. Full leaves: `flywheel principles show <name>`.
- **Wave 1-2 landed surface (ADR-0019):** `ledger {check,receipt}` = br-gate verdict store + SHA-bound legal close (close without a legal verdict reopens); `ledger receipt --close-reason <text>` runs the advisory VERIFY-tail + principles-citation check (lint warnings, ADR-0006 D3 shared with the wave-swarm VERIFY stage); `workflow lint` enforces playbook structure (feature `data_shape`, bug-fix `repro` predecessor, prototype/arena `n>=2` + mode rules, forensics read-only — lints live in toron-workflow); playbook builtins landed (bug-fix, feature, refactoring, prototype, arena, perf, hillclimb, interrogate, forensics, babysit-bead, ship-beads, autonomous-work, pickup, pause); figure-it-out is decide-routed to `compose --agent` (no builtin YAML); decide rows gained the `!implement` guard (fix-the-leak → bug-fix, never forensics).
- **`[{id}] done` has two senders (bd-loop-reporter-of-record):** the executor's courtesy copy (brief `REPORT`) and the loop's reporter-of-record copy (`__mail-done` → `send_done_mail`, 2026-08-18) — sent only on a verified LEGAL close, best-effort + once per bead per loop invocation (`done_mailed` set), so the captain is notified even when the agent forgets. The durable close signal is `br close` (claim-watch → `close_check`); the `[{id}] done` subject is the dispatch-guard's ANCHORED done hold (`subject_done`: subject ENDS with `] done` — a participle like `[bd-123] done investigating` does NOT match). Duplicate `[{id}] done` mails are expected and harmless.
- **Preflight:** `steer`/`loop-mode run`/`workflow run --execute` auto-run prerequisite checks (herdr, resonate, br, beads, operator key, mail). `--skip-preflight` bypasses. Standalone: `flywheel preflight --mode {steer|loop|workflow|cron}`. `loop-mode` without `--work` is still solo durable execution; only the `--work` specialization requires beads.
- **Captain briefing (bd-xe9r/bd-nmzk):** interactive TTY `steer` and `workflow run --execute` prompt before dispatch — inferred crew counts (editable), per-role model menus w/ cost/caps from the registry, squad/bare-harness shape (including bare `provider/model` escape hatch, bd-j3so), stage-chain preview, go/no-go. Skipped by `--json`, `--yes`, or an explicit `--crew`; non-TTY dispatch is byte-identical. Shared prompt module `src/steer_brief.rs` (`source` fork only, `workflow_briefing_context` extracts `objective`/`goal`/`target` from `--inputs`); prompt logic is pure over injected IO.
 - **Durable doorbell (bd-durable-doorbell-lngf, tick bd-yw4s / bd-teow):** dispatchers are supervised — `recover`/`status` lazily heal dead ones (only workspaces that ever ran one); `flywheel doorbell {status,start,stop,re-inject <project>}` is the explicit surface; wake state (`since_by_pin` + last_wake) persists per workspace so pending wakes survive crashes at-least-once; `doctor` warns on dead dispatchers; re-inject restores the peer-aware handler on pane-split/post-crash panes. Watch child never exits on repeated failure (retry gate on kind-39002), fast path `inbox --since` only (high-water, never `fetch_inbox()` full sweep).

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

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
- **Tracker:** `br` CLI + `--json`. No beads MCP. Never bare `bv`.
- **Kill:** `--force` skips confirm. Working agents also need `--force-working`.
- **workflow run** without `--execute` dispatches nothing. `--mode` default is headless.
- **`cron run`** is a 25s dry-run by default; `--live` force-fires once
  through the durable-loop runner (real dispatch). Recurring ticks are
  `cron start`.
- **compose --agent** is wired (planner YAML → lint). Compose arms:
  `--parts` (grammar + stage-first), `--from-yaml -|<file>`, `--dry-run`,
  `--run --keep <name>` (green-run graduation). Composed runs bounded by
  `FLYWHEEL_COMPOSE_MAX_SECONDS` (default 3600).
- **Spawn:** `--profile`, never `add --oc` when a profiled pane exists. Model via `ModelRegistry` (`opencode-go/deepseek-v4-flash` for coder).
- **Harness recipes (H1, bd-harness-prime-agent-818f):** a profile whose harness is NOT in herdr's closed kind set (e.g. `pa` = prime-agent) spawns via the compiled-in `[harness.<name>]` recipe: on `unsupported_agent_kind` from `agent.start`, `start_agent_with_retry[(_dyn)]` falls back to `pane.run` the recipe (+ shell-quoted model/effort args) and waits/renames. prime-agent's built-in herdr socket extension self-reports lifecycle; `agent.prompt` stays blocked upstream until herdr ships the kind (discussion herdrdev/herdr#2840).
- **Harness integration matrix (ADR-0019 wave, bd-skills-harness-hardening-8mto):** lifecycle-authority kinds (pi/omp/kimi/opencode/kilo/mastracode+grok), env injection (HERDR_ENV+HERDR_SOCKET_PATH at pane creation), universal boot prime via pane.send_text, integration preflight, pane primitives, alt-screen read ladder → canonical in `~/.agents/skills/flywheel/SKILL.md` §7; herdr client details in `~/.agents/skills/toron/SKILL.md` (Backend section).
- **Peer-aware spawns BY DEFAULT (ADR-0005):** `spawn --profile` starts one workspace doorbell dispatcher + injects the handler template (`integrations/peer-aware.md.tpl`) with the pin (`mail_identity`). `--no-peer-aware` skips dispatcher + handler, never the identity line. Wake is `[doorbell]` via `herdr agent.prompt` (kind-9 `p` is npub; `@pin` in content). After inject, the pin has **120s** to show heard (presence or kind-9); else digest `doorbell: unanswered pin=…` (deaf pane). Budget 5/30/3 is dispatcher-enforced. A 5-min self-poll is safety-net only.
- **Send:** `--to <full-name|coder1|omp1|pane_id>`. Never bare `--to oc`. `launch_pending` is not idle. `agent_not_ready` → retry pane id. Do not flip omp defaults.
- **Stage payloads (ADR-0006 D1):** `send --stage <name> --param k=v` (repeatable) substitutes
  `{{k}}` slots in the stage template; reserved slots `{{bead}}/{{scope}}/{{exit}}/{{channel}}/{{pin}}/{{report_file}}`.
  A stage's declared required params missing → non-zero exit naming the slot. Zero `--param`
  renders byte-identical to the unparameterized prompt.
- **Wave-scoped dispatch (ADR-0006 D2):** `loop run --work --wave-label wave-N` = ready ∩ exact
  label, held with named blockers while any open/in_progress bead carries wave-M (M<N). Default
  `--work` unchanged. Label-based hold is transitional (TODO → beads ADR-0001 schema-18 typed field).
- **wave-swarm recipe (ADR-0006 D3):** `workflow compose "<objective>" --parts wave-swarm` expands
  GATE→DISPATCH→OBSERVE→VERIFY→SIGNOFF over the compiled-in `wave-*` stages; SIGNOFF suspends the
  run on `request_approval` (captain-only by construction). Bounded by `FLYWHEEL_COMPOSE_MAX_SECONDS`.
- TDD. No script-based code edits. No file deletion without permission.
- **Model routing is STRICT (nothing bypasses the registry):** profile models are registry ids — `ModelRegistry::load_routing()` (compiled-in defaults + user `models.toml`) + `resolve_strict()` add the provider prefix and enforce `caps`. An unregistered id or caps-excluded harness fails the spawn naming the fix. big-pickle is opencode-only (scout). Explicit ad-hoc `--model` keeps lenient passthrough. Dot-bearing ids must be quoted in models.toml (`[models."glm-5.2"]`).
- **Engineering principles (ADR-0019 Spec 5/6):** `flywheel principles list`. Cite the name that changed the decision. Full leaves: `flywheel principles show <name>`.
- **Wave 1-2 landed surface (ADR-0019):** `ledger {check,receipt}` = br-gate verdict store + SHA-bound legal close (close without a legal verdict reopens); `ledger receipt --close-reason <text>` runs the advisory VERIFY-tail + principles-citation check (lint warnings, ADR-0006 D3 shared with the wave-swarm VERIFY stage); `workflow lint` enforces playbook structure (feature `data_shape`, bug-fix `repro` predecessor, prototype/arena `n>=2` + mode rules, forensics read-only — lints live in toron-workflow); playbook builtins landed (bug-fix, feature, refactoring, prototype, arena, perf, hillclimb, interrogate, forensics, figure-it-out, babysit-bead, ship-beads, autonomous-work, pickup, pause); decide rows gained the `!implement` guard (fix-the-leak → bug-fix, never forensics).
- **Preflight:** `steer`/`loop run`/`workflow run --execute` auto-run prerequisite checks (herdr, resonate, br, beads, operator key, mail). `--skip-preflight` bypasses. Standalone: `flywheel preflight --mode {steer|loop|workflow|cron}`.

## Skill Sync Contract

When you add/remove/rename a CLI command, update ALL of:
1. The `Commands` enum (`src/main.rs`) — ground truth (compiles).
2. The `robot_capabilities` JSON (`src/main.rs`, `--robot-capabilities`) — agent self-discovery.
3. This AGENTS.md (the Critical overrides + Details sections).
4. The skill: `~/.agents/skills/flywheel/SKILL.md` §5 Commands.

The skill files are the canonical agent-facing reference. A command not in the
skill is invisible to any agent that triggers the skill.

## Details

- [Backends + config](docs/agent-instructions/backends.md)
- [Orchestration](docs/agent-instructions/orchestration.md)
- [Durable loop](docs/agent-instructions/loop.md)
- Workflow DSL: [docs/workflow-authoring.md](docs/workflow-authoring.md)
- Sibling mailbox: `../toron/AGENTS.md`

# flywheel

herdr swarm orchestrator. NTM-parity CLI + Resonate durable loop. Embeds
toron (`MailSurface`); never the reverse.

## Commands

```bash
cargo test
bash scripts/verify-clippy.sh
bash docs/decisions/_scripts/verify-review-fixes.sh
cargo build --release && cp target/release/flywheel ~/.local/bin/flywheel
# macOS: codesign --force --sign - ~/.local/bin/flywheel
```

Live ignored tests need herdr + mail. `cargo test --exclude-ignored` is invalid.

## Critical overrides

- **Classify first:** `flywheel decide "<target>" --json`. Run `argv`. Do not invent a second selector.
- **Deps:** flywheel → toron-workflow → toron-core. Never reverse.
- **Mail:** `toron_core::mailbox::surface::MailSurface`. Local `MailBackend` is deleted. Default mail = toron (absent `[mail.toron] url` → embed).
- **Tracker:** `br` CLI + `--json`. No beads MCP. Never bare `bv`.
- **Kill:** `--force` skips confirm. Working agents also need `--force-working`.
- **workflow run** without `--execute` dispatches nothing. `--mode` default is headless.
- **`cron run`** is a 25s dry-run. Live ticks are `cron start`.
- **compose --agent** is wired (planner YAML → lint).
- **Spawn:** `--profile`, never `add --oc` when a profiled pane exists. Model via `ModelRegistry` (`opencode-go/deepseek-v4-flash` for coder).
- **Harness recipes (H1, bd-harness-prime-agent-818f):** a profile whose harness is NOT in herdr's closed kind set (e.g. `pa` = prime-agent) spawns via the compiled-in `[harness.<name>]` recipe: on `unsupported_agent_kind` from `agent.start`, `start_agent_with_retry[(_dyn)]` falls back to `pane.run` the recipe (+ shell-quoted model/effort args) and waits/renames. prime-agent's built-in herdr socket extension self-reports lifecycle; `agent.prompt` stays blocked upstream until herdr ships the kind (discussion herdrdev/herdr#2840).
- **Harness integration matrix (ADR-0019 wave, bd-skills-harness-hardening-8mto):** lifecycle-authority kinds (pi/omp/kimi/opencode/kilo/mastracode+grok), env injection (HERDR_ENV+HERDR_SOCKET_PATH at pane creation), universal boot prime via pane.send_text, integration preflight, pane primitives, alt-screen read ladder → canonical in `~/.agents/skills/flywheel/SKILL.md` §7; herdr client details in `~/.agents/skills/toron/SKILL.md` (Backend section).
- **Peer-aware spawns BY DEFAULT (2026-08-15, captain: collab is not optional):** `spawn --profile` injects the peer-poll template (`integrations/peer-aware.md.tpl`) with the agent's registered mail identity — the canonical `flywheel-<project>-<role><N>` name (S8.3), never a random pair — unless `--no-peer-aware` (single-shot panes). Wake/digest resolves + registers the identity before instructing fetch (S8.1); assigns wake only when unread, unexecuted, and deduped (S8.2). Poll 5-min idle / 30-min budget / ≤3 prompts per task.
- **Send:** `--to <full-name|coder1|omp1|pane_id>`. Never bare `--to oc`. `launch_pending` is not idle. `agent_not_ready` → retry pane id. Do not flip omp defaults.
- TDD. No script-based code edits. No file deletion without permission.
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

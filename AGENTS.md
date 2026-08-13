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
- **Send:** `--to <full-name|coder1|omp1|pane_id>`. Never bare `--to oc`. `launch_pending` is not idle. `agent_not_ready` → retry pane id. Do not flip omp defaults.
- TDD. No script-based code edits. No file deletion without permission.

## Details

- [Backends + config](docs/agent-instructions/backends.md)
- [Orchestration](docs/agent-instructions/orchestration.md)
- [Durable loop](docs/agent-instructions/loop.md)
- Workflow DSL: [docs/workflow-authoring.md](docs/workflow-authoring.md)
- Sibling mailbox: `../toron/AGENTS.md`

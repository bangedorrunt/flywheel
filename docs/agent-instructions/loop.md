# Durable loop

`flywheel loop run <project> [--work]` is a Resonate workflow. Re-run the same
command to resume. One live loop per project (`loop list` / `loop prune`).
`--force` takes over a corrupted invocation.

State: `~/.local/state/flywheel/loops.json` (schema 2) + `last_sent/<project>.json`.

## Cadence

- `--interval N` (default 300) when settled
- `--interval-active N` (default 30, floor 5) while working/blocked or just after dispatch

Confirmed send is default (`__send --confirm` → working|blocked landing). `--no-confirm` is the escape hatch.

## Dispatch

- working → skip
- blocked → `--on-blocked continue|stop|mail` (mail escalates T0–T3; T3 halts)
- idle/done → next queue item
- unknown → never dispatch; 2+ consecutive polls → respawn

Work mode (`--work`): `br ready` → claim/execute prompts. Claim watchdog: unclaimed-while-working gets a claim nudge; settled-unclaimed is re-dispatched; completion requires an empty claim-watch.

Mail wake: idle agents with urgent mail get one MAIL WAKE per message id.

## State-file contract

`<project>/.flywheel/agent-state/<name>.json`:

```json
{ "context_pct": 82, "ts": "2026-08-03T03:00:00Z" }
```

Freshness 15 minutes. Reader threshold `context_pct >= 80`. Writer just reports the number. Verdict = `CONTEXT_LIMIT` marker OR (fresh AND >= 80).

## Beads (slash commands, not a subagent)

`flywheel integration {opencode,pi,grok,omp}` regenerates `prompts/beads-*.md`.

```bash
br ready --json
br update <id> --claim --actor "$AGENT_NAME" --json
br close <id> --reason "..."
br sync --flush-only   # then commit .beads/ yourself
bv --robot-triage      # never bare bv
```

## Kill / recover

`flywheel kill <project> --force` skips y/N. If any agent is `working`, also `--force-working` (cargo test routinely exceeds 15m).

`flywheel recover` : liveness (foreground group) then responsiveness (`state_change_seq` window). Unresponsive → respawn.

`flywheel status <label>` resolves custom `--label` aliases plus the default `flywheel-<base>` name.

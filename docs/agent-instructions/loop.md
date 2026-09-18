# Durable loop

Session door is `flywheel arrive` then `flywheel briefing --json`. Briefing
topology `loop` is what runs this command. Do not start a second loop on a
joinable fleet.

`flywheel loop-mode run <project> [--work]` is the durable **solo execution
loop**, implemented with Resonate workflow machinery. Re-run the same command
to resume. One live loop per project (`loop-mode list` / `loop-mode prune`).

Solo is the topology; the work source is separate:

- without `--work`: drive the current objective, queue, or named workflow with
  one current harness/pane;
- with `--work`: use `br ready` as the work source, claim beads, enforce wave
  gates, run VERIFY, and close/reopen legally.

`--work` therefore requires beads; loop mode itself does not. Loop mode does
not spawn a parallel crew. A reviewer may be summoned explicitly, but that is
not swarm execution. `--force` takes over a corrupted invocation.

State: `~/.local/state/flywheel/loops.json` (schema 2) + `last_sent/<project>.json`.

## Cadence

- `--interval N` (default 300) when settled
- `--interval-active N` (default 30, floor 5) while working/blocked or just after dispatch

Confirmed send is default (`__send --confirm` → working|blocked landing). `--no-confirm` is the escape hatch.

## Dispatch

- working → skip
- blocked → `--on-blocked continue|stop|mail` (mail escalates T0–T3; T3 halts). Tiers are **ack-gated**: the `notify_blocked` leaf reads the agent's own `health-<name>` thread at SLA 0 (`__ack-overdue`, toron's per-recipient `message_reads` state) before each step, and an ack resets the ladder (budget restarts, T0 marked handled). No ping yet → no gate; a read that cannot answer (no index, unregistered target, `[mail.toron] url` remote) is not an ack, so the time ladder keeps climbing — never a silent stop.
- idle/done → next queue item
- unknown → never dispatch; 2+ consecutive polls → respawn

Work mode (`--work`): `br ready` → claim/execute prompts. Claim watchdog: unclaimed-while-working gets a claim nudge; settled-unclaimed is re-dispatched; completion requires an empty claim-watch. This is the bead-backed specialization of solo loop mode, not the definition of loop mode itself.

Mail wake: idle agents with urgent mail get one MAIL WAKE per message id.

## State-file contract

`<project>/.flywheel/agent-state/<name>.json`:

```json
{ "context_pct": 82, "ts": "2026-08-03T03:00:00Z" }
```

Freshness 15 minutes. Reader threshold `context_pct >= 80`. Writer just reports the number. Verdict = `CONTEXT_LIMIT` marker OR (fresh AND >= 80).

## Beads (br, not a slash family)

`br` owns the bead lifecycle. `flywheel integration {opencode,pi,grok,omp,pa}`
regenerates `*-mode.md` only and deletes leftover `/beads-*` slash files.

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

# Durable loop

Session door is `flywheel arrive` then `flywheel briefing --json`. Briefing
topology `loop` is what runs this command. Do not start a second loop on a
joinable fleet.

`flywheel loop-mode run <project> [--work]` is the durable **solo execution
loop**, implemented with Resonate workflow machinery. One live loop per
project (`loop-mode list` / `loop-mode prune`).

Re-running is FRESH by default. A pending invocation whose owner process is
gone is a zombie: its checkpointed leaf results were written by another
binary, and replaying them against the current leaf shapes storms
serialization errors. So the default start rejects the zombie and begins
fresh; `--resume` replays a killed run's checkpoints on purpose. A resumed run
that still hits the stale-checkpoint signature (every critical leaf failing
while the world looks empty) degrades to ONE fresh invocation instead of
aborting; a fresh invocation that stays blind aborts naming the fix
(`flywheel loop-mode doctor --fix`).

For a cadence, `flywheel loop-mode supervise <project> [--work] [--interval N]`
runs `loop-mode run --once` as a CHILD process per pass (a crashed pass cannot
kill the supervisor), never passes `--force` (a live loop must refuse loudly,
not be taken over), and stops after 3 consecutive failed passes naming the
manual pass to run.

Solo is the topology; the work source is separate:

- without `--work`: drive the current objective, queue, or named workflow with
  one current harness/pane;
- with `--work`: use `br ready` as the work source, claim beads, enforce wave
  gates, run VERIFY, and close/reopen legally.

`--work` therefore requires beads; loop mode itself does not. Loop mode does
not spawn a parallel crew. A reviewer may be summoned explicitly, but that is
not swarm execution. `--force` takes over a live/corrupted invocation (it
rejects the prior explicitly and starts fresh).

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

bd-claim-watch-durable-signals-mixy: the watchdog reads durable claims **first**. A live toron reservation (`reason == bead id` on the active board) suppresses the Redispatch/Resume arms — nudges still target the bead's own lane, but no second executor is dispatched over live work; the hold releases when the lease lapses. After the lease, the assignee's pane state decides (settled vs working), and the mail guard scan runs last on every redispatch. The guard's scan also reads the kind-9 steer rooms (`steer-<project>` + `steer-<bead>`), so channel claims count as peer signals.

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
`flywheel arrive --as <pin>` upserts the pin so whois/captain resolve.
`flywheel mail agents <p> --disk` lists key dirs that are not in the registry.

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

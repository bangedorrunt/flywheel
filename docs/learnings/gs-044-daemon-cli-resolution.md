<!-- governed-by: ADR-0012, ADR-0010 -->

# GS-044: A hosted daemon that cannot exec its CLIs is a silently dead ring

Verified live 2026-09-13 against the real launchd daemon on port 18980.

## Symptom

`/health` reported the best possible state — `mode: hosted`, two subscribers,
`sweep.last_age_s: 1`, `degraded: []` — and **not one wake had been delivered
in hours**. The journal looked alive: a `FetchInbox` action row every second
for each hosted workspace. The only oddity was a `RestartWatch` row every
16 seconds, which reads as routine supervision until you measure it.

## Root cause

`file_host:189 restart_watch` spawns `toron watch --live …` by **bare name**,
and the daemon is started by launchd with `PATH=/usr/bin:/bin:/usr/sbin:/sbin`
— where `toron` does not exist. `Command::new("toron").spawn()` returns
`NotFound`, which the host reports as an ordinary child death:

- no `toron watch` process ever existed (zero, not stale);
- `WatchDied` → `RestartWatch` on the retry gate, forever (250 rows in
  ~83 min, exactly 16s apart);
- the same bare name broke every plane-P inbox fetch — 3 audit rows per
  second, one per stale registry pin: `toron spawn/timeout failed`;
- `herdr` (the prompt sink) was equally unresolvable from `~/.local/bin`.

So the ring was dead on **both** hops while the one surface an operator looks
at said everything was fine. `degraded: []` was a lie of omission: the health
handler had no probe for either fact.

## Fix (landed)

1. `doorbell-rt::binpath::resolve_cli` — resolve `toron`/`herdr` to an
   absolute path: `PATH` → `~/.cargo/bin` → `~/.local/bin` → beside our own
   exe → mise shims **last**. Shim-dir-last is the load-bearing detail: a mise
   shim is a dispatcher that infers a tool version from env/cwd, so under
   launchd the *resolved* path still fails (`mise ERROR No version is set for
   shim: toron`). It is worse than not resolving, because the path looks
   healthy in a log line. Explicit `DOORBELL_TORON_BIN`/`DOORBELL_HERDR_BIN`
   overrides are honoured verbatim.
2. `doorbell-rt::tailprobe` — bounded tail probes (last 256 KiB) over the
   plane's own append-only surfaces, so `/health` can answer "why didn't I get
   woken?" from ground truth that survives a restart:
   `cli_unresolved`, `inbox_fetch_failing`, `watch_restart_loop`;
   `/health.binaries` reports the resolved pair.
3. `doorbell serve` logs the resolved pair at startup.

Proven live after reinstall + `launchctl kickstart -k`: two real
`toron watch` children alive, `Watch` rows landing in the journal again
(zero before), `inbox_error` stopped, `degraded: []` — now true.

## What actually generalized (the reusable lessons)

- **A green health surface is not a health check; it is a claim with
  evidence or it is noise.** The probes had to read the same files the
  failure writes. A status page with no probe for its own edges will always
  be green while the system is dead.
- **Verify the launcher's environment, not your shell's.** Every harness run
  from a terminal passes because the terminal has the developer's `PATH`.
  `launchd`/`systemd` do not. Any daemon that shells out must either resolve
  absolute paths at startup or declare them in the unit.
- **Count, don't sample, for periodic events.** The first thrash probe asked
  "was there a restart in the last 60s?" — but a healthy quiet watcher
  restarts by design every ~109s (90s `--idle-exit` + the retry gate), and two
  hosted workspaces phase them independently, so a "recent restart" probe
  fires on a healthy plane about half the time. Thrash is a frequency (16s vs
  109s); measure the count in a window against the designed cadence.
- **A resolved-looking path can still be a landmine.** Prefer a real binary
  over a shim/dispatcher whenever the consumer has no shell context.
- **Say when a hop is structurally absent.** The ring's last hop could not be
  proven in this environment because zero herdr agents existed, so no pane was
  injectable (`herdr agent list` → `[]`, `should_inject` accepts only
  idle|done|blocked). Report the gap; do not dress up a decision row as a
  delivered prompt.

## Second finding: subscriptions alone do not ring (ADR-0012 §8 Phase 2 re-opened)

`refresh_index` builds the sweep pin set, `SelfIndex`, and pin→pane routing
from the **spawn registry** only. A subscribed identity that was never spawned
in the project therefore is never fetched, never `addressed`, and has no
route. Live: the operator pin `QuietHeron` has a subscription row and the
probe mail was delivered into its inbox, yet **zero** journal rows mention it
and no API fetch ever targets it (audit pins = the three stale registry pins
only). The delivery side of Phase 2 (subscription store, budgets, journal
scoping) landed; the index/producer side did not. Until it does, "subscribed
human receives a wake with zero live swarm" is not true for the operator pin.

## Also observed (file, don't chase)

- The audit trail is what proved this: `doorbell/<ws>.audit.jsonl`, one
  `inbox_error` per pin per second. Read the audit before the log.
- `SlaFired count: 89` every 60s — 89 stale corrs past SLA being re-fired.
  Noise, not the ring, but it buries signal; worth its own bead.
- The live registry is a stale fleet (panes `w11:*` long gone) while the only
  live subscription is the operator's. Reap stale registry rows, not just
  stale subscriptions.
- HEAD was not `cargo fmt`-clean (`src/main.rs`, `src/doorbell.rs`,
  `doorbell-rt/src/subscribe.rs`), so the repo's own fmt/clippy gate was red
  before this change. Run the gate before trusting a green CI story.

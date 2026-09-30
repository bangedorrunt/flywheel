<!-- governed-by: ADR-0005, ADR-0007, ADR-0011, ADR-0012, ADR-0013 -->

# Configuration, inside out

One document for everything tunable in the flywheel × toron × beads stack:
every file, every layer, every daemon, and what rots when. Written
2026-09-13 (bd-y6gw) because the configuration was tribal knowledge with
no examples — toron already ships `toron/examples/toron.sample.toml`;
this is the flywheel counterpart plus the cross-daemon map.

## 1. The layer ladder

```
compiled-in defaults
  < user files (~/.config/flywheel/)
    < project files (<repo>/.flywheel/)
      < environment variables
        < CLI flags
```

Each layer only overrides what it names (field-level pick for profiles,
per-id extend for models, name-shadow for workflows/crons). Lower layers
never delete upper-layer entries — to remove a builtin, shadow it by name
or leave it; there is no "disable builtin" flag. Exact merge points:

| Surface | Compiled | User | Project | Env | CLI |
|---|---|---|---|---|---|
| flywheel core | `config.rs` defaults | `config.toml` | — (no project tier) | `FLYWHEEL_*` (below) | flags per command |
| models | `models.rs` 6 models + 3 slots | `models.toml` | — | — | `model add/set-default`, `--model/--provider` |
| profiles | `profile.rs` 6 profiles + 4 squads | `profiles.toml` | — | — | `profile set`, `binding-rebind` (live panes) |
| workflows | `builtin_workflows.rs` (40) | `workflows/*.yaml` | `.flywheel/workflows/*.yaml` adds a name the user tier lacks | — | `config seed`, `workflow seed` |
| stages | `stages.rs` seed | `stages/*.toml` | `.flywheel/stages.toml` adds project stages | — | `config seed`, `stage new` |
| judgment packs | `defaults/judgment-packs` in the binary | `judgment-packs/*.yaml` (this copy wins) | `.flywheel/judgment-packs` only if the user tier has no file for that id | `FLYWHEEL_JEV`, `TYPESAFE_API_KEY` | `config seed`, `config drift` |
| crons | `cron.rs` builtins | `crons/*.yaml` | `.flywheel/crons/*.yaml` | — | `cron lint/start/run` |
| standing orders | principle registry | `standing-orders.md` (generated, `regen`) | `.flywheel/standing-orders.md` (authored, regen never touches) | — | — |
| doorbell | budgets 5/30/3, digest 300s/10, TTLs (code) | `[doorbell]` in `config.toml` | — | `DOORBELL_*` | `doorbell …` verbs |

## 2. The files

### `~/.config/flywheel/config.toml` — transports and hosts

```toml
[doorbell]
url = "http://127.0.0.1:18980"  # presence selects the daemon host; absent = embedded
beads_poller = false             # Phase-4 flags, all default off
cron_doorbell_sink = false
```

Also supported: `[mail.toron]` url+token, `[backend]` multiplexer/mail,
`[backend]` multiplexer/mail, `[crew] max` (default 5), `[loop]`
thresholds (unknown_diagnose, respawn_budget, detect_grace, drain_grace,
claim_nudge), `socket_path`, `workspace_root`, `resonate_url`,
`on_captain_death` (stop | continue | adopt), `kinds`, `verifier_family_override`. Full annotated
inventory with every default and validation rule: `config.sample.toml`
at the repo root (unknown keys are ignored — typos fail silent, check
with `flywheel config list`). Judgment has no `config.toml` keys (env
only: `FLYWHEEL_JEV`, `TYPESAFE_API_KEY`, `FLYWHEEL_JEV_THRESHOLDS` —
see §3 and the [Jev enable guide](./guides/jev-w2-enable.mdx)).

### `~/.config/flywheel/models.toml` — registry overlay

```toml
[slots]  # empty = compiled-ins stand; per-slot override example:
# [slots.standard]
# harness = "pi"
# model = "opencode-go/muse-spark-1.3-contributor"

# [models."myprov/mymodel"]  # user key: composite provider/id REQUIRED
# caps = ["opencode"]        # harnesses allowed; [] = all
# fallback = "other/model"   # registry-level reroute rung

# [defaults]  # DANGER: wholesale override, collapses ALL slots onto one
# harness = "pi"             # model (removed 2026-09-13: it made tiers a no-op)
```

Compiled-in fleet: cheap=opencode/big-pickle, standard=pi/muse-spark-1.3
(+`zai/glm-5.3-flash` model-fallback), premium=omp/zai/glm-5.3
(+`fallback_harness = "grok"` — the grok harness runs its own default
model, model-opaque, no `grok/*` key needed). Bare `[models.<id>]` keys
fail loudly; `flywheel model add <id> --provider <p>` is the fix.

**A slot names a model, not a promise about every harness.** `big-pickle` is
capped to `opencode` because it is free only there, so a profile naming
`@cheap` has nothing routable on a `pa` crew from that slot alone. When every
declared rung misses, resolution falls back to a **default**: the harness's own
implied slot, then a coupled harness's own family model, then a portable
registry pick that prefers the tier's own provider. The hop is *named* in
`profile show --json` (`implied_slot`, `coupled_default`, `dynamic_default`) so
the spawn ledger records which rung fired rather than leaving a substitution to
infer.

This applies to **Open-tier** harnesses only. `grok`, `codex` and `claude` are
frontier CLIs that run their own models and are never handed one, so a profile
naming a tier still fails on them — loudly, and by design. If you want a role
to crew one of those, the profile must name that harness.

A profile with a **concrete** model and no harness still fails: there is no
default harness, because picking one would silently decide where every
harness-less profile runs. Use a slot, or name the harness.

### `~/.config/flywheel/profiles.toml` — role overlay

```toml
[profiles.coder]
harness = "opencode"                     # empty = inherit slot
model = "muse-spark-1.3-contributor"
provider = "opencode-go"                 # pins the composite key
effort = "high"
fallback = ["scout"]                     # profile names: billing/auth/provider hops
model_fallback = ["opencode-go/deepseek-v4.1-flash"]  # model ids: rate_limit hops
```

Six builtins (oracle/planner/reviewer @premium, coder/housekeeper
@standard, scout @cheap). `fallback[]` ≠ `model_fallback[]` (ADR-0013
D4). Unknown fallback name = load error. Live panes change only via
`flywheel binding-rebind` (journaled), never by editing this file.

### `~/.config/flywheel/workflows/*.yaml`, `crons/*.yaml`

Unified toron-workflow schema (`name`/`trigger`/`steps`). Same name as a
builtin shadows it (loud warning). Cron fields: `name schedule owner
scope mode policy{on_failure, repeat_threshold, max_retries, catchup}
steps[]`. Tiers: builtin < global < project; user cron names must not
collide with builtins (the `weekly-review` shadow cost a debugging
session; renamed `weekly-review-sunday`).

### Project tier: `<repo>/.flywheel/`

`stages.toml` (dispatch stages ride CLI sends), `standing-orders.md`
(authored ceremony), `workflows/` + `crons/` (shadow globals by name),
`agent-names.json` (spawn-registry mirror — volatile, do not hand-edit).

### Toron side

`toron/examples/toron.sample.toml` is the annotated reference (5-layer
ladder: CLI > `TORON_*` env > `./.toron.toml` > `~/.config/toron/`
> `/etc/toron`). `~/.config/toron/passphrase` (0600) feeds the daemon.

## 3. Environment variables

| Var | Effect |
|---|---|
| `XDG_STATE_HOME` | state root (unset macOS fallback: `~/.local/state` — unified 2026-09-13, was Application Support for the CLI only) |
| `FLYWHEEL_SOCKET/ROOT/TIMEOUT/BACKEND/MAIL/TORON_URL/TOKEN/MAIL_NAME/CREW_MAX/VERIFY_STRICT/COMPOSE_MAX_SECONDS/DISPATCH_TTL_SECS/PANE_STALL_SECS/RECLAIM_*/MCP_TIMEOUT_SECS/NO_DOORBELL_ENSURE` | per-name overrides (TIMEOUT fail-loud) |
| `DOORBELL_STATE_DIR` / `DOORBELL_FLYWHEEL_STATE_DIR` / `DOORBELL_PORT` | daemon dir/port resolution (unit exports the first; state default: `$DOORBELL_STATE_DIR` > `$XDG_STATE_HOME/doorbell` > `~/.local/state/doorbell`, legacy `~/Library/Application Support/doorbell` auto-migrated once) |
| `DOORBELL_TORON_BIN` / `DOORBELL_HERDR_BIN` | watch/prompt child binaries (tests set `true`) |
| `DOORBELL_WEBHOOK_ALLOWLIST` | egress hosts allowed for `--sink webhook` rows (comma/space separated, exact or `.suffix`; absent = the daemon registers no webhook adapter and those wakes stay digest-visible) |
| `TORON_DATA_DIR` / `TORON_MCP_DATA_DIR` / `TORON_KEY_PASSPHRASE` | toron roots + operator key (never `TORON_OPERATOR_PASSPHRASE`) |
| `RESONATE_URL` | durable-loop server (default `http://localhost:8001`) |
| `HERDR_SOCKET_PATH` | multiplexer socket |
| `AGENT_NAME` / `TORON_AGENT` / `FLYWHEEL_MAIL_AS` | your pin (attributed commits; unattributed fail closed) |
| `FLYWHEEL_JEV` | judgment gate (bd-0glq P7: on by default). Unset = auto (enrich iff `TYPESAFE_API_KEY` is set, else loud `jev: unconfigured` passthrough, never escalate); `1/true/yes` = force on (deny escalates even keyless); `0/false/no` = force off (byte-identical) |
| `TYPESAFE_API_KEY` | System One key, mikasa-only custody. Absent = unconfigured lane (marker, no escalate); present + failure = I4 hard escalate. Never commit a real key |
| `FLYWHEEL_JEV_THRESHOLDS` | provisional `topology,honesty,announce` floats (default `0.85,0.80,0.60`, captain-tunable, re-validation duty) |

## 4. The daemons (who runs what, who touches whom)

| Daemon | Binary | Port | Unit | Owns |
|---|---|---|---|---|
| toron | `toron serve` | 18080 | `com.toron.serve` | mailbox/bus/Nostr archive |
| flywheel | `flywheel serve` | 8787 | `dev.flywheel.daemon` | dispatch snapshots, loop |
| doorbell | `doorbell serve` | 18980 | `com.doorbell.serve` | wake loops, journals |
| resonate | `resonate` | 8001 | (dev process) | durable promises |

All three units are launchd-persisted (macOS) / systemd user (Linux).
Ports, pidfiles (`{flywheel,doorbell}.pid`), sockets and units are
disjoint — daemons never fight over binds (second binder exits 0 over a
healthy doorbell peer, hard-fails naming a squatter).

Shared surface (one tree, partitioned): everything lives under the ONE
state dir (`~/.local/state/flywheel`). `doorbell/` holds pid/gen/project/
unanswered/audit + `subscriptions.json`; `journals/` holds per-workspace
decision logs; `owner-{ws}.json` leases arbitrate the single writer per
workspace (stale = pid dead or heartbeat > 90s).

Start/stop asymmetry (by design, now documented): `flywheel daemon start`
ALSO ensures the doorbell daemon (ride-along); `daemon stop`, `kill`, and
`doorbell stop` never touch the other daemon. `flywheel kill` drops the
spawn's subscription rows but never the daemon. `doorbell stop` refuses
unmanaged holders, even with `--force`.

## 5. Hosting, plainly (the doorbell host feat)

A subscription row is `identity × scope × filter × sink × budget × endpoint`
(`state/doorbell/subscriptions.json`). Spawn writes one per pin
(`spawn-<ws>-<pin>`); `doorbell subscribe` writes `cli-*` rows with an
optional `--project` and, for `--sink webhook`, the row's own `--url` —
the daemon registers one adapter per distinct subscribed endpoint at host
start, gated by `DOORBELL_WEBHOOK_ALLOWLIST`, so the endpoint *is* the
human's route when they have no pane. Every 60s the daemon reconciles: for each
subscribed `(project, workspace)` not yet hosted, it claims the lease
(skips live writers — never steals) and spawns a Brain loop; spawn-scoped
rows silent for 7 days are reaped. Standby with zero subscriptions is the
only quiet state; `standby_with_known_workspaces` is degraded and names
the arming verb. Boot `--host p/w` pairs (baked into the unit) cover the
known workspaces; subscribe-time rows cover everything else — no manual
`--host` needed after the first install.

Failure modes we hit and fixed: adopt-without-hosting (`start` returned
before `--host` applied — restart with hosts); stale unit definitions
(launchd keeps the loaded plist — `bootout` + `bootstrap` to reload);
pre-populated hosted list skipping all boots (sole-writer invariant);
reconcile self-deadlock (never hold a guard across the loop body);
fresh-binary SIGKILL (ad-hoc `codesign -s -`); malformed plist env
(blind token replace — now `_KEY`/`_VALUE` tokens); macOS state split
(unified on `~/.local/state`).

## 6. Stale doctrine (what rots, who reaps it)

| Artifact | Liveness signal | TTL / reaper | Manual command |
|---|---|---|---|
| toron reservations | holder activity | intent 5min / held 90d; the LOOP releases on a verified close (reason == bead id) | `toron reserve release --project … --as …`; operator `release-by-reason <bead>` |
| doorbell subscriptions | pane/workspace alive | `kill` drops its spawn rows; daemon reaps week-silent spawn rows | `flywheel doorbell unsubscribe <id>` |
| spawn registry pins | herdr pane exists | none automatic (heal warns) | `flywheel recover`; `toron role clear` |
| doorbell journals | subscription present | rotation 5k lines / 5 MiB (never silent loss) | archive, never delete |
| bead claims | `updated_at` movement | stale ~2h (agent) / ~1 day (human) | reclaim with audit comment |
| toron bus events | — | 100k cap / 90d prune | — |
| legacy slug identities | — | none (bead `bd-4a0u`) | operator follow-up |
| unit definitions | file mtime vs loaded | none — `bootout`+`bootstrap` after `install` | (above) |

Landed automation: the durable loop observes the open→closed transition in
its close-detect arm and runs the operator-signed `__release-bead-locks`
leaf (`release_by_reason`, reason == bead id) — `durable.rs`
`ClaimWatchAction::ReleaseReservations`, pinned by
`close_path_releases_the_beads_reservations`. It is best-effort and
idempotent (`released: 0` is a clean no-op), and `reclaim_bead` runs the same
leaf when a pane exits. A close still does NOT release by itself: nothing
frees a lease if no loop is watching that workspace, so a peer holding one
drops it explicitly.

Open automation (follow-up beads, not this session): spawn-registry prune of
pane-less entries; journal archiving for subscription-less workspaces.

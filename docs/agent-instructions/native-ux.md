# Native UX contract (Phase 3)

One dispatch contract, one boundary-parsing rule, one config inventory, typed
failures, one freshness gate. Nouns follow [vocabulary](../vocabulary.md) —
open | loop | swarm, stage / workflow / compose token / profile / topology.

## 1. Dispatch contract

`arrive` → `briefing` → run the emitted argv:

```bash
flywheel arrive <project> --as <pin>
flywheel briefing "<target>" --project <p> --json # then --commit / --yes
```

`decide` is the only selector (kind: `flywheel decide "<target>" --json`).
No second selector: a target that classifies `none` gets a declared trigger or
`workflow compose --agent`, never a forced fit. Command aliases are deleted, not
wrapped (the `loop` → `loop-mode run` rewrite is the last one standing, kept for
back-compat and documented at its call site).

One "make it so" path per topology (open: current pane; loop:
`loop-mode run <project> [--work]`; swarm: `workflow compose --parts wave-swarm
--run`). Escape hatches (`--raw` briefs, `--escape` spawns, `--no-peer-aware`
panes) drop their scaffold by name and say so loudly — an escape never silently
becomes the default path.

## 2. Boundary parsing (`TimeSpec`)

`TimeSpec` is the one temporal type at every CLI edge that takes a time, and it
is specified here for the code lane to implement (no `TimeSpec` exists in source
today — every current door takes exactly one shape and says which):

- Accepted forms: RFC3339 (`2026-11-06T03:00:00Z`) | relative (`6h`, `30m`,
  `90s`, `7d`) | `last` (the daemon-side high-water, agents only).
- Parsed once at the CLI edge into an absolute instant or cursor token;
  internal code never re-parses a string.
- Ambiguous input fails with the accepted forms named; unknown units fail, they
  do not fall back to a default window.
- Today's doors (unchanged until the code lane lands `TimeSpec`):
  `toron mail inbox --since last` takes `last` or RFC3339 (never a unix int,
  never `6h`); MCP `fetch_inbox since_ts` takes RFC3339 only.

## 3. Config inventory (98 knobs: 64 `FLYWHEEL_*` + 34 `TORON_*`)

Counts by `rg -o "FLYWHEEL_[A-Z0-9_]+" src/ | sort -u` (65 names + one
`SUPERVISOR_{…}` doc-brace phantom and one `FUNDED_BALANCE_{PROVIDER}` prefix
pattern whose live instance is `_ZAI`) and the `TORON_` equivalent over
`toron/crates` (34): 99 knobs total — 65 `FLYWHEEL_*` + 34 `TORON_*`.

Rule for all rows (contract): **parse-or-refuse** — an unknown enum name, a
malformed number/duration, or an unknown value is an error naming the accepted
forms, never a silent off/default. `Today` marks what the code currently does:
**S** = already refuses, **L** = lenient fallback today (migrate),
**–** = not classified (shape only).

### Flywheel (64)

| Name | Default | Rule / today |
|------|---------|--------------|
| `FLYWHEEL_BACKEND` | `auto` (rex when CLI present, else hc + reason) | S: unknown names refuse naming `auto\|rex\|superlogical\|hc` |
| `FLYWHEEL_MAIL` | `toron` (only backend) | S: unknown names refuse |
| `FLYWHEEL_MAIL_NAME` | unset → shared default | L: empty ignored |
| `FLYWHEEL_TORON_URL` | unset → in-process embed | L: empty ignored |
| `FLYWHEEL_TORON_TOKEN` | unset → none | L: empty ignored |
| `FLYWHEEL_TIMEOUT` | 1_800_000 ms (`DEFAULT_TIMEOUT_MS`) | S: non-number refuses naming ms |
| `FLYWHEEL_ALLOW_EXEC` | on (only `0` opts out) | L today: any other value stays on (contract keeps this exact spelling) |
| `FLYWHEEL_TIMER` | on (only `0\|false\|off` opts out) | L today (contract keeps this exact spelling) |
| `FLYWHEEL_NO_DOORBELL_ENSURE` | ensure on (`1` skips with loud hint) | L today |
| `FLYWHEEL_VERIFY_STRICT` | off (`1`/`true` arms) | L today |
| `FLYWHEEL_BR_BIN` | `br` | L: empty ignored |
| `FLYWHEEL_BR_HOLD_TIMEOUT_MS` | 10 s (`BR_HOLD_TIMEOUT`; must be >0) | L today |
| `FLYWHEEL_COMPOSE_MAX_SECONDS` | 3600 s | L today (must be ≥1) |
| `FLYWHEEL_DISPATCH_TTL_SECS` | 90 min (`DISPATCH_GUARD_TTL_US`; must be >0) | L today |
| `FLYWHEEL_GATE_BUDGET` | 4000 tokens | L today |
| `FLYWHEEL_GUARD_GRACE_SECS` | 300 s | L today |
| `FLYWHEEL_CREW_MAX` | 5 (must be ≥1) | L today |
| `FLYWHEEL_MCP_TIMEOUT_SECS` | 10 s (`DEFAULT_TOTAL_TIMEOUT`) | L today |
| `FLYWHEEL_MAIL_HEALTH_ESCALATE_AT` | 3 consecutive failures | L today |
| `FLYWHEEL_SUPERVISOR_JOIN` | `all` | L today (`Join::parse`, unparseable → default) |
| `FLYWHEEL_SUPERVISOR_STRATEGY` | `OneForOne` | L today |
| `FLYWHEEL_SUPERVISOR_MAX_WALL_MS` | unset (no wall gate) | L today |
| `FLYWHEEL_SUPERVISOR_EPOCH` | unset (no forced cursor) | – |
| `FLYWHEEL_SUPERVISOR_CLOCK_SCALE` | 1 (must be ≥1) | L today |
| `FLYWHEEL_SUPERVISOR_HINTS` | on (`off` disables) | L today |
| `FLYWHEEL_SUPERVISOR_PASSES` | on (`off` disables) | L today |
| `FLYWHEEL_SUPERVISOR_TICK_MS` | 1000 (floor 10) | L today |
| `FLYWHEEL_SUPERVISOR_ACK_MS` | unset | – |
| `FLYWHEEL_SUPERVISOR_LEASE_MS` | unset | – |
| `FLYWHEEL_SUPERVISOR_GENERATION` | set by relaunch only | – |
| `FLYWHEEL_SUPERVISOR_HOOK_FILE` | unset (no hook path) | – (path, no parse) |
| `FLYWHEEL_SUPERVISOR_RELAUNCH_BIN` | unset (current exe) | – |
| `FLYWHEEL_RECLAIM_SWARM_SECS` | 7200 s (must be >0) | L today |
| `FLYWHEEL_RECLAIM_HUMAN_SECS` | 86400 s (must be >0) | L today |
| `FLYWHEEL_PANE_STALL_SECS` | 1800 s (must be >0) | L today |
| `FLYWHEEL_STALE_ESCALATE_SECS` | 240 s | L today |
| `FLYWHEEL_BUSY_TURN_MAX_SECS` | 3600 s | L today |
| `FLYWHEEL_INBOX_TICK_MS` | struct default (must be >0) | L today |
| `FLYWHEEL_INBOX_DEBUG_ROUND` | off (set = DBG eprintln lines) | – (debug-only) |
| `FLYWHEEL_INBOX_CLOCK_SCALE` | struct default (must be >0) | L today |
| `FLYWHEEL_DIGEST_SECS` | 300 s (`DIGEST_INTERVAL_DEFAULT_SECS`) | L today |
| `FLYWHEEL_DIGEST_THRESHOLD` | 10 lines | L today |
| `FLYWHEEL_DOORBELL_RING_MAX` | 3 | L today |
| `FLYWHEEL_DOORBELL_GRACE_SECS` | 90 s | L today |
| `FLYWHEEL_SHIP_GATE_MODE` | `Warn` (only `block` arms) | L today |
| `FLYWHEEL_RUN_STATE_LOG` | off (`1`/`true` attaches) | L today |
| `FLYWHEEL_WEDGE_INSPECT_COUNT` | 3 | L today |
| `FLYWHEEL_WRITE_WALK_TIMEOUT` | unset-gated (absent = feature off) | L today |
| `FLYWHEEL_WRITE_WALK_PRUNE` | unset-gated (comma list) | L today |
| `FLYWHEEL_WRITE_WALK_MAXDEPTH` | unset-gated | L today |
| `FLYWHEEL_VERBOSE` | off (nonempty arms) | L today |
| `FLYWHEEL_JEV` | off (set arms the gate) | – |
| `FLYWHEEL_JEV_THRESHOLDS` | unset | – |
| `FLYWHEEL_FUNDED_BALANCE_ZAI` | unset (no funded check) | – |
| `FLYWHEEL_ROLE` | unset (pane-injected identity) | – |
| `FLYWHEEL_MAIL_AS` | unset (pane-injected identity; identity ladder) | – |
| `FLYWHEEL_ROOT` | unset (config `workspace_root`) | L: empty ignored |
| `FLYWHEEL_SOCKET` | unset (config `socket_path`) | S today (config layer refuses bad parse elsewhere) |
| `FLYWHEEL_SOURCE` | `~/workspace/flywheel` | L today |
| `FLYWHEEL_REPO_DIR` | unset (flag `--repo-dir` or error) | S today (missing errors loudly) |
| `FLYWHEEL_TEST_ENV_PROBE` | test-only marker | – |
| `FLYWHEEL_PAUSE_RESURFACE_SECS` | 3600 s | – |
| `FLYWHEEL_OP` | protocol marker (`U+2063 + FLYWHEEL_OP:`), not a toggle | S today (unknown markers are not lifecycle events) |

### Toron (34)

| Name | Default | Rule / today |
|------|---------|--------------|
| `TORON_BACKEND` | `auto` | S today (unknown names refuse; explicit `rex` never falls back) |
| `TORON_DATA_DIR` | XDG chain (`TORON_DATA_DIR` > `TORON_MCP_DATA_DIR` > `$XDG_DATA_HOME/toron` > `~/.local/share/toron`) | L: empty ignored |
| `TORON_MCP_DATA_DIR` | see chain above | L: empty ignored |
| `TORON_MCP_URL` | unset → in-process embed | L: empty ignored |
| `TORON_COMMS_INBOX` | on (only `0\|false\|off` opts out) | L today (contract keeps this exact spelling) |
| `TORON_GUARD_STRICT` | on (`0` disables with loud escape-hatch line) | L today |
| `TORON_GUARD_TIMEOUT_MS` | 100 ms | L today (unparseable → default) |
| `TORON_OPERATOR_KEY` | `~/.config/toron/operator_nsec` (a PATH, never the secret) | – (path, no parse) |
| `TORON_OPERATOR_NSEC` | **DEAD** — rejected name (test `readme_offers_only_the_live_operator_key_env`); use `TORON_OPERATOR_KEY` | S today (test refuses) |
| `TORON_KEY_PASSPHRASE` | `"toron"` dev default | – (secret, no parse) |
| `TORON_KEY_PASSPHRASE_FILE` | unset | – (path, no parse) |
| `TORON_PROJECT` | none (required flag-ish) | S today (missing fails loud, never a `demo` guess) |
| `TORON_WAKE_SOCK` | `<state_dir>/wake.sock` | L: empty ignored |
| `TORON_WAKE_SOCK_IDLE_MS` | code default | – |
| `TORON_WAKE_SOCK_HANDSHAKE_MS` | code default | – |
| `TORON_WAKE_MAX_SUBSCRIBERS` | unset (must be >0 when set) | L today |
| `TORON_PRESENCE_TTL_SECS` | 60 s | L today |
| `TORON_FAULT` | unset (test fault point; set aborts the process) | – (test-only) |
| `TORON_UNDER_TEST` | unset (test-only) | – |
| `TORON_CRASH_CHILD_DB` | unset (test-only) | – |
| `TORON_DEMO` | unset | – |
| `TORON_AGENT` | legacy identity fallback (`AGENT_NAME` wins) | – |
| `TORON_CONFIG` | unset (no extra file) | L: empty ignored |
| `TORON_GIT_BINARY` | `which git` | L today |
| `TORON_TOOL_TIMEOUT_MS` | unset (no timeout) | L today |
| `TORON_TUI_TIMEOUT_MS` | unset | L today |
| `TORON_MCP_ALLOWED_HOSTS` | unset | L today |
| `TORON_METRICS_FILE` | unset (no file sink) | – |
| `TORON_COALESCE_WINDOW_MS` | code default | – |
| `TORON_COALESCE_SUBWINDOW_MS` | code default | – |
| `TORON_PROOF_GATE_ENABLED` | off | L today |
| `TORON_PROOF_SCHEME` | `schnorr` | L: empty ignored |
| `TORON_ENVELOPE_TTL_DAYS` | 90 | L today |
| `TORON_REGEN_SHAPES` | off (`1` rewrites test fixtures) | L today |

`--help` verbs named above: `arrive`, `briefing`, `decide`, `config list`,
`loop-mode run`, `toron mail inbox`, `toron doctor` — all resolve.

## 4. Failure codes (typed, specified for the code lane)

No shell-text matching. Each code carries one remediation line:

| Code | Meaning | Remediation |
|------|---------|-------------|
| `StoreSkew` | The toron store was written by a newer binary than the reader (`inbox_store_skew` / `ack_sla_store_skew` today) | Reinstall toron from the source that wrote the store; probe honestly with `toron ack-sla --project <p>` |
| `DaemonUnreachable` | No live daemon answers (`com.toron.serve` down, socket gone, `:18080` refused) | `toron daemon status`, then `start`/`restart`; a `stop` writes `stopped-by-operator` (deliberate, not an outage) |
| `NoInjectablePane` | Pins exist but no pane can take a prompt (`pins > 0`, `panes_seen == 0` on `/health.injection`) | Start agents or fix the stale registry; address by npub and use mail — never retry the prompt wake |

## 5. Freshness gate

`arrive`, `briefing`, and `doctor` name all three revisions: repo HEAD, the
installed binary, and the store schema. A stale binary is a named degradation,
never a silent field-missing:

- `toron doctor` already names the running build (`build.verdict` =
  `current`|`replaced`) and FAILS (exit 3) on a stale daemon — `daemon restart`
  clears it.
- The door adds the same triple: HEAD sha (the checkout the agent stands in),
  `flywheel --version` / `toron` build identity, and the durable-store schema
  the reader expects. Any skew is announced at the door with its remedy
  (reinstall / restart / migrate), and the announcement rides the normal
  briefing output — never a second channel, never silence.

Follow-up code beads (file + owner + verify idea) live in the wave report —
the captain files them. This page specifies; it implements nothing.

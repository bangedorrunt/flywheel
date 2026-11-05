# Backends + config

Pluggable multiplexer + mail. Spine targets traits, not concretes.

## Matrix

| Role | Trait | Default | Stubs |
|------|-------|---------|-------|
| Multiplexer | `toron_core::backend::Backend` (re-export `MultiplexerBackend`) | auto = rex-first + hc fallback (both planes always compiled in toron-core) | none (single selected backend) |
| Mail | `toron_core::mailbox::surface::MailSurface` | toron (`mail-toron`) | none (single backend) |

Factory: `src/backend/factory.rs`. Code: `src/backend/`.

## Identity vocabulary (ids never match across backends)

| Term | hc | rex | Notes |
|------|-------|-----|-------|
| Session id | workspace id = root terminal address (`hc:t1`) | session id (`session:…`) | Opaque; compare only within one backend |
| Session label | workspace label | session label | Human-readable; collides across sessions — never an identity |
| Pane id | terminal address (`t7`, `hc:t7`) | block id (`block:…`) | Opaque; compare only within one backend |
| Agent name | hc `toron_pin` label, else terminal name | block label | Set by spawn/rename (the trusted writer); the wake binding checks it |
| Binding stamp | `terminal.label` (`toron_pin` / `toron_project` / `toron_npub`) | none — label plus session id | Provenance `hc-label` vs `rex-label`; never mix the checks |

## Selection

**flag > env > `.env` > `config.toml` `[backend]` > default**

| Var / key | Values | Default |
|-----------|--------|---------|
| `FLYWHEEL_BACKEND` / `[backend] multiplexer` | hc | rex | superlogical | auto | auto |
| `FLYWHEEL_MAIL` / `[backend] mail` | toron | toron |
| `FLYWHEEL_TORON_URL` / `TOKEN` | `[mail.toron]` only | config.toml |
| `FLYWHEEL_MAIL_NAME` | `[agent_mail] name` | `flywheel` |

`[mail.toron] url` = daemon. Absent = in-process embed. Legacy `[agent_mail] url` is not a routing source.

Ack-overdue reads (`flywheel __ack-overdue`, `Mail::ack_overdue`) are **in-process only**: they read per-recipient `message_reads` state through the mailbox projection, and the remote daemon exposes no ack-overdue tool, so the call fails loudly instead of answering an empty set. The blocked escalation's ack gate (see [loop.md](loop.md)) treats that as "not acked" and keeps its time ladder.

Mailbox root: `TORON_DATA_DIR` > `TORON_MCP_DATA_DIR` > `$XDG_DATA_HOME/toron` > `~/.local/share/toron`. Never per-pid temp. `src/mail.rs` + `src/coalition.rs` share this.

Plane selection is resolved once per process (`factory::resolve_plane`, a thin wrapper over `toron_core::backend::plane::resolve`) and every door reads that one answer — spawn, `status`, `snapshot`, crew launch, `preflight`, captain. The table and the construction site are toron-core's (toron ADR-0045); flywheel passes only the configured name and its `[backend] socket_path`. `REX_SESSION` records where the process runs; it is not evidence of the plane. So `auto` is rex whenever the rex CLI answers, env or no env (`bd-mknvb`), an explicit `superlogical` pin never falls back silently, and a fallback prints one line naming its reason. A door that keyed on the env instead split the planes: spawn wrote a rex session while `status` read hc (`bd-l7b05`).

The fallback is per effect: reads replay on `Connect`/`Io`/`Timeout`; bounded waits on `Connect`/`Io` (a timeout is the caller's answer); **writes on `Connect` only** — a timed-out write may already have committed and there is no idempotency key. Semantic rejects and `Unsupported` surface loud, never masked. Pane ids are backend-namespaced (hc `t7`/`terminal:`, rex `block:`/`session:`), and `pane_keep` matches `"hc"` exactly, so auto skips plane-only janitor work in the safe direction. Agent status on both backends folds one OSC 7501 grammar (`toron_core::osc7501::Board`; `src/backend/rex_map.rs`). One-shot Lua rides `rex do -e` (`integrations/rex/session-snapshot.lua`); handler scripts would outlive the call bound and must not ride it.

Liveness on rex, strongest signal first: explicit 7501 records (interactive
pane sessions emit working/blocked/done with titles — verified live;
headless `opencode run` one-shots leave no records), then the foreground
process (`process` block method: agent binary in front is Working, a shell
is at a prompt), then `child_exited`/`process_changed` events. `pane_run` executes in the pane on both backends (text plus Enter); a headless one-shot belongs in Lua `rex.exec`, not in `pane_run`.

Manual rex crews (the escape hatch for a harness with no spawn recipe): `rex new` a session, `split` blocks, `block rename` each to its pin, start the harness via the block itself, and record `pane_id`/`name`/`harness` rows in the project's `.flywheel/agent-names.json` mirror (recover/rebind read it when the registry is empty). Bindings verify through the rex label (label plus session) with provenance `rex-label`; stamped-token panes keep the token path on every backend. Inbox observe on rex reads classified `agent_list` rows projected onto the pane shape; `pane_list` leaves status Unknown by construction and would silence every ring.

```bash
flywheel backend list --json
flywheel backend status
flywheel backend default <multiplexer|mail> <name>
```

hc OpenRPC. Socket: `HC_SOCKET_PATH` or `$XDG_STATE_HOME/herdr-core/$HC_SESSION/$HC_SESSION.sock`.

## Effect-kernel authority

Runtime adapters are the effect boundary. Resolve host identity, paths, configuration, clocks, process paths, and network clients once, then pass them into interpreters. Reducers never read these sources directly. A daemon or remote backend returning no proof yields `unknown` or a loud degraded result, never an invented success. See [effect-kernel discipline](effect-kernel.md).

## Runtime

- Resonate: `http://localhost:8001`, sqlite `~/.local/state/flywheel/resonate.db`; `flywheel deps --ensure` or `flywheel daemon start` starts it when down (loopback urls only)
- Toron daemon (optional): `toron daemon` / `com.toron.serve`, `127.0.0.1:18080`. Passphrase file, never plist.
- State: `~/.local/state/flywheel/` — event logs, `loops.json` schema v2 (`src/state.rs`)
- Config: `~/.config/flywheel/config.toml` — models registry `models.toml` uses composite `provider/model` keys (e.g. `[models."opencode-go/deepseek-v4.1-flash"]`); `Profile.provider` pins the composite; bare `--model`/`--provider` on spawn is lenient fallback, profile routing is strict.

Copy `.env.example` → `.env`. Never commit tokens.

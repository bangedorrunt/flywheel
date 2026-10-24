# Backends + config

Pluggable multiplexer + mail. Spine targets traits, not concretes.

## Matrix

| Role | Trait | Default | Stubs |
|------|-------|---------|-------|
| Multiplexer | `toron_core::backend::Backend` (re-export `MultiplexerBackend`) | auto = rex-first + herdr fallback (`backend-superlogical`); herdr when that feature is off | none (single selected backend) |
| Mail | `toron_core::mailbox::surface::MailSurface` | toron (`mail-toron`) | none (single backend) |

Factory: `src/backend/factory.rs`. Code: `src/backend/`.

## Identity vocabulary (ids never match across backends)

| Term | herdr | rex | Notes |
|------|-------|-----|-------|
| Session id | workspace id (`w22`) | session id (`session:…`) | Opaque; compare only within one backend |
| Session label | workspace label | session label | Human-readable; collides across sessions — never an identity |
| Pane id | `wX:pY` | block id (`block:…`) | Opaque; compare only within one backend |
| Agent name | herdr agent name | block label | Set by spawn/rename (the trusted writer); the wake binding checks it |
| Binding stamp | `report_metadata` tokens | none — label plus session id | Provenance `report_metadata` vs `rex-label`; never mix the checks |

## Selection

**flag > env > `.env` > `config.toml` `[backend]` > default**

| Var / key | Values | Default |
|-----------|--------|---------|
| `FLYWHEEL_BACKEND` / `[backend] multiplexer` | herdr \| superlogical \| auto | auto (feature on) / herdr (feature off) |
| `FLYWHEEL_MAIL` / `[backend] mail` | toron | toron |
| `FLYWHEEL_TORON_URL` / `TOKEN` | `[mail.toron]` only | config.toml |
| `FLYWHEEL_MAIL_NAME` | `[agent_mail] name` | `flywheel` |

`[mail.toron] url` = daemon. Absent = in-process embed. Legacy `[agent_mail] url` is not a routing source.

Ack-overdue reads (`flywheel __ack-overdue`, `Mail::ack_overdue`) are **in-process only**: they read per-recipient `message_reads` state through the mailbox projection, and the remote daemon exposes no ack-overdue tool, so the call fails loudly instead of answering an empty set. The blocked escalation's ack gate (see [loop.md](loop.md)) treats that as "not acked" and keeps its time ladder.

Mailbox root: `TORON_DATA_DIR` > `TORON_MCP_DATA_DIR` > `$XDG_DATA_HOME/toron` > `~/.local/share/toron`. Never per-pid temp. `src/mail.rs` + `src/coalition.rs` share this.

`auto` retries a call on herdr only for transport codes (`Connect`/`Io`/`Timeout`); semantic rejects surface loud, never masked. Pane ids are backend-namespaced (herdr `wX:pY`, rex `block:`/`session:`), and `pane_keep` matches `"herdr"` exactly, so auto skips herdr-only janitor work in the safe direction. Agent status on both backends folds one OSC 7501 grammar (`toron_core::osc7501::Board`; `src/backend/rex_map.rs`). One-shot Lua rides `rex do -e` (`integrations/rex/session-snapshot.lua`); handler scripts would outlive the call bound and must not ride it.

Liveness on rex, strongest signal first: explicit 7501 records (empty for opencode today — verified live, so a silent harness never reads as failed), then the foreground process (`process` block method: agent binary in front is Working, a shell is at a prompt), then `child_exited`/`process_changed` events. `pane_run` executes in the pane on both backends (text plus Enter); a headless one-shot belongs in Lua `rex.exec`, not in `pane_run`.

Manual rex crews (spawn is herdr-led and refuses): `rex new` a session, `split` blocks, `block rename` each to its pin, start the harness via the block itself, and record `pane_id`/`name`/`harness` rows in the project's `.flywheel/agent-names.json` mirror (recover/rebind read it when the registry is empty). Bindings verify through the rex label (label plus session) with provenance `rex-label`; stamped-token panes keep the token path on every backend.

```bash
flywheel backend list --json
flywheel backend status
flywheel backend default <multiplexer|mail> <name>
```

herdr protocol v20. Socket: `HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`.

## Effect-kernel authority

Runtime adapters are the effect boundary. Resolve host identity, paths, configuration, clocks, process paths, and network clients once, then pass them into interpreters. Reducers never read these sources directly. A daemon or remote backend returning no proof yields `unknown` or a loud degraded result, never an invented success. See [effect-kernel discipline](effect-kernel.md).

## Runtime

- Resonate: `http://localhost:8001`, sqlite `~/.local/state/flywheel/resonate.db`; `flywheel deps --ensure` or `flywheel daemon start` starts it when down (loopback urls only)
- Toron daemon (optional): `toron daemon` / `com.toron.serve`, `127.0.0.1:18080`. Passphrase file, never plist.
- State: `~/.local/state/flywheel/` — event logs, `loops.json` schema v2 (`src/state.rs`)
- Config: `~/.config/flywheel/config.toml` — models registry `models.toml` uses composite `provider/model` keys (e.g. `[models."opencode-go/deepseek-v4.1-flash"]`); `Profile.provider` pins the composite; bare `--model`/`--provider` on spawn is lenient fallback, profile routing is strict.

Copy `.env.example` → `.env`. Never commit tokens.

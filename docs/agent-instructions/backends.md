# Backends + config

Pluggable multiplexer + mail (ADR-0002). Spine targets traits, not concretes.

## Matrix

| Role | Trait | Default | Stubs |
|------|-------|---------|-------|
| Multiplexer | `toron_core::backend::Backend` (re-export `MultiplexerBackend`) | herdr (`backend-herdr`) | superlogical (`Unsupported`) |
| Mail | `toron_core::mailbox::surface::MailSurface` | toron (`mail-toron`) | none (bd-euav: single backend) |

Factory: `src/backend/factory.rs`. Code: `src/backend/`.

## Selection

**flag > env > `.env` > `config.toml` `[backend]` > default**

| Var / key | Values | Default |
|-----------|--------|---------|
| `FLYWHEEL_BACKEND` / `[backend] multiplexer` | herdr \| superlogical | herdr |
| `FLYWHEEL_MAIL` / `[backend] mail` | toron | toron |
| `FLYWHEEL_TORON_URL` / `TOKEN` | `[mail.toron]` only | config.toml |
| `FLYWHEEL_MAIL_NAME` | `[agent_mail] name` | `flywheel` |

`[mail.toron] url` = daemon. Absent = in-process embed. Legacy `[agent_mail] url` is not a routing source.

Ack-overdue reads (`flywheel __ack-overdue`, `Mail::ack_overdue`) are **in-process only**: they read per-recipient `message_reads` state through the mailbox projection, and the remote daemon exposes no ack-overdue tool, so the call fails loudly instead of answering an empty set. The blocked escalation's ack gate (see [loop.md](loop.md)) treats that as "not acked" and keeps its time ladder.

Mailbox root (bd-t9ey): `TORON_DATA_DIR` > `TORON_MCP_DATA_DIR` > `$XDG_DATA_HOME/toron` > `~/.local/share/toron`. Never per-pid temp. `src/mail_shim.rs` + `src/coalition.rs` share this.

```bash
flywheel backend list --json
flywheel backend status
flywheel backend default <multiplexer|mail> <name>
```

herdr protocol v20. Socket: `HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`.

## Effect-kernel authority

Runtime adapters are the effect boundary described by ADR-0016. Resolve host identity, paths, configuration, clocks, process paths, and network clients once, then pass them into interpreters. Reducers never read these sources directly. A daemon or remote backend returning no proof yields `unknown` or a loud degraded result, never an invented success. See [effect-kernel discipline](effect-kernel.md).

## Runtime

- Resonate: `http://localhost:8001`, sqlite `~/.local/state/flywheel/resonate.db`; `flywheel deps --ensure` or `flywheel daemon start` starts it when down (loopback urls only)
- Toron daemon (optional): `toron daemon` / `com.toron.serve`, `127.0.0.1:18080`. Passphrase file, never plist.
- State: `~/.local/state/flywheel/` — event logs, `loops.json` schema v2 (`src/state.rs`)
- Config: `~/.config/flywheel/config.toml` — models registry `models.toml` uses composite `provider/model` keys (e.g. `[models."opencode-go/deepseek-v4.1-flash"]`); `Profile.provider` pins the composite; bare `--model`/`--provider` on spawn is lenient fallback, profile routing is strict.

Copy `.env.example` → `.env`. Never commit tokens.

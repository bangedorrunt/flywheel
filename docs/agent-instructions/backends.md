# Backends + config

Pluggable multiplexer + mail (ADR-0002). Spine targets traits, not concretes.

## Matrix

| Role | Trait | Default | Stubs |
|------|-------|---------|-------|
| Multiplexer | `toron_core::backend::Backend` (re-export `MultiplexerBackend`) | herdr (`backend-herdr`) | superlogical, tmux (`Unsupported`) |
| Mail | `toron_core::mailbox::surface::MailSurface` | toron (`mail-toron`) | none. agentmail is a real peer |

Factory: `src/backend/factory.rs`. Code: `src/backend/`.

## Selection

**flag > env > `.env` > `config.toml` `[backend]` > default**

| Var / key | Values | Default |
|-----------|--------|---------|
| `FLYWHEEL_BACKEND` / `[backend] multiplexer` | herdr \| superlogical \| tmux | herdr |
| `FLYWHEEL_MAIL` / `[backend] mail` | toron \| agentmail | toron |
| `FLYWHEEL_AGENTMAIL_URL` / `TOKEN` | `[mail.agentmail]` only | config.toml |
| `FLYWHEEL_MAIL_NAME` | `[agent_mail] name` | `flywheel` |

`[mail.toron] url` = daemon. Absent = in-process embed. Legacy `[agent_mail] url` is not a routing source.

Mailbox root (bd-t9ey): `TORON_DATA_DIR` > `TORON_MCP_DATA_DIR` > `$XDG_DATA_HOME/toron` > `~/.local/share/toron`. Never per-pid temp. `src/mail_shim.rs` + `src/coalition.rs` share this.

```bash
flywheel backend list --json
flywheel backend status
flywheel backend default <multiplexer|mail> <name>
```

herdr protocol v19. Socket: `HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`.

## Runtime

- Resonate: launchd `com.flywheel.resonate`, `http://localhost:8001`, sqlite `~/.local/state/flywheel/resonate.db`
- Toron daemon (optional): `toron daemon` / `com.toron.serve`, `127.0.0.1:18080`. Passphrase file, never plist.
- State: `~/.local/state/flywheel/` — event logs, `loops.json` schema v2 (`src/state.rs`)
- Config: `~/.config/flywheel/config.toml`

Copy `.env.example` → `.env`. Never commit tokens.

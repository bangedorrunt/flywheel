# Captain-pane herdr plugin (bd-xenf)

Read-and-notify only: captain-pane presence, toast, and binding
reconcile. Never dispatches, closes, or writes beads. Terminology is
**captain pane** throughout.

## Events

| Hook | Script | What |
|---|---|---|
| `pane.closed`, `pane.exited`, `pane.agent_status_changed` | `captain-presence.sh` | Upsert toron presence row (`--as <toron_pin> --state working --status live\|gone`) + write last snapshot |
| `pane.agent_status_changed` | `captain-notify.sh` | `notification.show --sound request` when a captain pane turns `blocked` |
| `pane.closed` | `captain-reconcile.sh` | Snapshot-vs-live diff: vanished captain panes get a `gone` row and leave the snapshot |

Captain pane = a herdr pane carrying a `toron_pin` token (`herdr agent
list` → `tokens.toron_pin`). The pin + project ride the presence row
that bd-3vr9 consumes.

## Env (all inherited, all optional)

`HERDR_BIN_PATH`, `HERDR_PLUGIN_STATE_DIR` (snapshot:
`captain-pane-snapshot.json`), `HERDR_PLUGIN_EVENT[_JSON]`,
`HERDR_PANE_ID`, `TORON_MCP_URL` (needed where a toron daemon serves;
absence only warns). Every script exits 0 — a hook never breaks
herdr's event loop.

## Link / verify

```bash
herdr plugin link ./integrations/herdr-plugin
herdr plugin list 2>&1 | grep captain-pane
```

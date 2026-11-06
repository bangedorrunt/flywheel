# bd-wave12-config-strict-n9hcs — flywheel parse-or-refuse (CEILING)

status: CEILING
done:
  - strict helpers + load-time validation + supervisor/inbox try_ + 7 green
    tests (receipt .flywheel/receipts/bd-wave12-config-strict-n9hcs-c9dc22f3baf6.verify.txt, commit c9dc22f3baf6ed3165f3147b3fc4cfd5cfc3ac72)

## Helper

`src/config.rs`: `ConfigError { name, value, accepted }` (Display:
`NAME="value" is not valid (accepted: …)`), `parse_flag`
(`1|true|on|0|false|off`), `parse_enum(name, accepted, parse)`,
`parse_u64`, `parse_path`. No new dependency. `Config::load` runs
`validate_env_knobs` (validates, stores nothing — caller precedence and
defaults byte-identical).

## Knob inventory (66 names; migrated / left)

Migrated (garbage → loud error at startup; valid unchanged):
- `Config::load` validation: FLYWHEEL_BACKEND (`auto|rex|superlogical|hc`
  via `PlaneChoice`), FLYWHEEL_MAIL (`toron`), FLYWHEEL_CREW_MAX (int ≥ 1),
  FLYWHEEL_TIMER / ALLOW_EXEC / NO_DOORBELL_ENSURE (bool),
  FLYWHEEL_SHIP_GATE_MODE (`block|warn`).
- `HostCfg::try_from_env` (supervisor): TICK/ACK/LEASE/MAX_WALL_MS (u64),
  STRATEGY, JOIN, CLOCK_SCALE; 2 prod callers migrated
  (`supervisor_host`, `doorbell_host` via inbox); legacy `from_env` now
  `#[cfg(test)]`.
- `inbox_host::HostCfg::try_from_env`: INBOX_TICK_MS / CLOCK_SCALE (> 0);
  old `from_env` deleted (single caller migrated).
- `br_program` via `parse_path` (shape-only; any non-blank is a name).
Left (with reason):
- FLYWHEEL_VERIFY_STRICT — readers only in `src/main.rs:14193`,
  `src/cmd/ledger.rs:142` (heal lane; file+line per brief).
- FLYWHEEL_STATE_DIR — no reader in `src/` (only `DOORBELL_*` variants).
- FLYWHEEL_MAIL_AS — flows through pane env maps, no process-env read.
- PASSES/HINTS (`!= "off"`) — total functions per native-ux contract
  ("on (`off` disables)"); no garbage class.
- EPOCH/GENERATION — open marker strings / write-only; no garbage class.
- `enabled()`/`exec_allowed`/`gate_mode` tables — kept; garbage pre-empted
  at load, same doctrine as toron wave7.

## Tests

- `config::config_strict_tests` (5 unit: flag table, error shape, load
  refuse/accept) + `tests/config_strict.rs` (13-knob refuse/accept table +
  clean-env baseline via `config list`, hermetic temp HOME/XDG/CWD).
- Mutation proof: accepting "perhaps" as true fails the flag table (ran,
  FAILED, reverted).
- `mbx test -p flywheel config_strict`: EXIT 0, 36 suites ok, 7/7 strict
  tests green. `check -p flywheel --all-targets`: no errors (only
  pre-existing warnings). My files fmt-clean.

## Residual inventory (gate leg; one row per residual)

- `src/cmd/send.rs:16` — import-order fmt diff — re-run `cargo fmt`
  (heal lane owns `cmd/*`) — enabler: none (mechanical; then re-run the gate).
- `src/test_source.rs:74` — fmt diff — same remedy — enabler: none.
- `bd-wave12-config-strict-n9hcs` — full VERIFY re-run
  (`verify-clippy.sh` + `test config_strict`) on a formatted tree, then the
  standard gate+ledger close — enabler: none.

recommendation: close with amendment + residual

PRINCIPLES: prove-it-works — 7/7 tests green plus a ran-and-reverted
mutation failure, no close without the gate; fix-root-causes — one helper
per shape, silencers deleted (not wrapped), garbage pre-empted at load;
sequence-verifiable-units — check, tests, report, each evidenced.

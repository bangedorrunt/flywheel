# bd-jbz1 verification: restart preserves hosts (fix was bd-u54l)

## What was wrong

`doorbell restart` with no `--host` built the expected launchd unit from its
own empty argv, mismatched the installed unit carrying three hosts, and fell
back to starting detached with a hostless config. Fixed in bd-u54l: the armed
host set is STATE (`armstate::load` + `resolve(persisted ∪ argv)` in main.rs
before start/restart), persisted on supervised start.

## Verified live 2026-09-29

- `doorbell restart --host flywheel/flywheel-flywheel` → mode=hosted,
  degraded=[], all three hosts preserved across two consecutive restarts.
  Resolve is union, not replace: re-asserting one host keeps the rest.
- The bead's verbatim verify passes (exit 0).

## Environmental trap found on the way

`degraded: [stale_build]` was red from the dirty worktree, not from the
restart: the probe walks `crates/doorbell-{cli,rt}/src` mtimes, and any
uncommitted-or-uninstalled change keeps it red. Rebuilding from a tree clean
in those crates (`mbx install --path crates/doorbell-cli --force`) cleared
it. Do NOT "fix" a red verify by rebuilding from a tree dirty in those
crates — that ships unreviewed code into the live daemon.

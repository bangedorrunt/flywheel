# bd-registry-strict-mutating-e5ko3 — FINAL LANE report

status: done
done:
  - strict loader wired into every mutating door (receipt
    `.flywheel/receipts/bd-registry-strict-mutating-e5ko3-57d5a96b93ce.verify.txt`,
    commit `57d5a96b93ce72402879265e97278ae9f454e75b`)

## Helper

`state::merge_registry_for_mutation(project, current)` — fold legacy alias
+ dedupe + mirror, exactly like the lenient path minus the degrade. Each
door strict-loads first (refuse), then completes through the helper, so
healthy reads see byte-identical content (proven by
`merge_for_mutation_matches_lenient_on_healthy`).

## Wiring (refuse with the typed path error)

- `cmd_recover` / `cmd_kill` / `cmd_binding_rebind` /
  `rebind_from_registry` (main.rs) — bail `spawn registry corrupt: <path>`.
- `record_spawn_entry` / `bind_pane_mail_identity` (main.rs, return unit) —
  loud `REFUSING … <path>` + skip the write (no more one-row pave-over).
- `spawn_crew_into_rex` / `spawn_crew_into` (pane.rs) — bail before
  prune+append+save.
- `LivePorts::observe` (supervisor_host.rs) — loud skip of the crew spawn
  pass, plane messages preserved (returns `out`, not Err: a corrupt file
  must not break the supervision loop).
- `cmd_add[_rex]` / `cmd_spawn[_rex]` heads (cmd/spawn.rs) — refuse before
  any bulk overwrite or mirror save.
- `send` — NO change: already compliant (lenient warning names the file,
  missing fleet bails at `in_ws.is_empty`), proven at the resolve layer.

## Tests (all green)

- Tripwire `mutating_doors_load_spawn_registry_strict`: every door above
  carries the strict loader (main.rs/pane.rs/supervisor_host.rs/cmd/spawn.rs).
- `record_…_refuses` / `bind_…_refuses`: corrupt file survives byte-identical.
- `send_resolve_…`: empty fleet refuses, live fleet resolves registry-free.
- Mutation proof (ran twice): removing the kill guard reddens the tripwire
  (exit 101); revert greens it. A first slicer version leaked into neighbour
  fns and one mbx run served a stale binary — both caught during the proof,
  the slicer now lives in `tripwire::{fn_body,body_from}` with the
  definition-line skip.
- Regression: 91 registry-filtered + 414 spawn/supervisor/send tests green;
  `verify-clippy.sh` exit 0; touched files fmt-clean.

## Residual inventory

- None from this bead. Read-only doors (~20 callers) intentionally keep
  warn-and-degrade per the brief. `reap_dead_registry_rows` / `pre_spawn`
  prune-guarded saves are no-write-on-corrupt by construction (left as is).

recommendation: close (unit-test-verified)

PRINCIPLES: fix-root-causes — a mutating door must not treat a corrupt fleet
as empty (the record-overwrite data-loss shape is now a loud refusal);
prove-it-works — tripwire + functional tests + two ran-and-reverted mutation
proofs, gate exit 0 before close; sequence-verifiable-units — check, targeted
tests, regression sweep, report, each evidenced.

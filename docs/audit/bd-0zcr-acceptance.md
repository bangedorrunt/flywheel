# bd-0zcr acceptance record — doorbell Phase-3 cutover

Closed via three coder lanes (manual steer, vehicle `stage ce-execute`). Parent verified by captain re-running both children's VERIFY fences after close.

## Units and evidence

| Unit | Lane | Commit | Fence (captain-rerun) | Result |
|------|------|--------|------------------------|--------|
| bd-nshr fmt convergence (prereq) | CopperSparrow | c637425 | `bash scripts/verify-clippy.sh` | exit 0 |
| 1-3 daemon default host + spawn-API subscription + kill paths | EmeraldOtter | 12d3746 | `flywheel kill --force flywheel` then `flywheel doorbell digest flywheel` | swarm closed, daemon untouched, digest exit 0 |
| 4-5 launchd --host autostart pairs + toron ack-sla verb | NavyDog | 1bd062b | `toron ack-sla --project flywheel --json` | exit 0, 80 events scanned |

## Notes

- Daemon was unreachable (embedded degraded mode) before, during, and after: pre-existing environment state, not a cutover regression. Cutover flips the default host; degraded mode remains the fallback.
- `flywheel doorbell digest` takes no `--json` flag; the child fence line was run without it (output above).
- Follow-up discovered during lane 3: bd-itsc (daemon roster/reservation visibility gap), left open.

PRINCIPLES: sequence-verifiable-units — cutover is a checkable unit per item; prove-it-works — captain reran every fence, no self-reports accepted.

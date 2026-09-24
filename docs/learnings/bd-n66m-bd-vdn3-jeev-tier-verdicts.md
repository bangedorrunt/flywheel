# bd-n66m: opencode v2 exonerated, spawn-path fixed-name bug confirmed (2026-09-24)

**Bead:** bd-n66m (research; implementation landed as bd-o1zh, commit 67cc551)
**Verdict:** hypothesis REFUTED by live test. Do not pin opencode, do not add
`--standalone`; fix unique names per spawn attempt.

## Evidence (live, same session)

- `herdr agent start oc-probe --kind opencode` boots healthy in 3.7s,
  argv is just `opencode`.
- Two concurrent opencode panes coexist, both idle, no collision.
- Shared background service (`opencode.exe serve --service`, one per user)
  is harmless under herdr.
- Upstream: sst/opencode carries v2.0.x tags but no published v2
  breaking-change notes; no multiplexer-collision issues found.

## Root cause (the actual bug)

Every `--profile X` spawn reuses the fixed herdr name
`flywheel-flywheel-X`. Attempt 2 hits `agent_name_taken`, the rename
rescue fails with `agent_launch_pending`, fail-fast fires. Flywheel-side
naming/retry bug, independent of opencode version. Fixed in bd-o1zh:
unique name per attempt as the idempotency key, stable effect key per
logical spawn.

# bd-vdn3: model+harness tier audit, answered by tier-pick (2026-09-24)

**Bead:** bd-vdn3 (research; implementation landed as bd-88m4 commit
30bf036, plus live-wiring bd-vlvn commit b435586)
**Verdict:** tier instinct confirmed; profile-pick alone was insufficient.

## Verified state

- Slots: cheap = opencode/big-pickle, standard = pi/muse-spark-1.3
  (+zai-flash fallback), premium = omp/zai-glm-5.3 (fallback grok,
  model-opaque, no model menu).
- oracle/planner/reviewer ride `@premium`; coder `@standard`; scout `@cheap`.
- profile-pick.v1 scored coder-family profiles only: no model/harness tier
  dimension, no confidence gate, offline keyword table in briefing.

## Landed

- tier-pick.v1.yaml (reasoning_depth + harness_coupling), tier_suggestion
  in steer briefing, coupled claude/codex entries, omp implied slot.
- Live wiring: annotate on key present, table as FallbackTable, reject-all
  gate (default bar 0.60), depth as Score, batched per-candidate scores.

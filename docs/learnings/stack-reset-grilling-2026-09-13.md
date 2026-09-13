<!-- governed-by: ADR-0012, ADR-0013, toron ADR-0032 -->

# Grilling: stack reset (profiles x workflows x stages) — 2026-09-13

Agent: QuietHeron · Bead: bd-2o1v · Outcome: full integration reset, all in this session.

Question: how should agent profiles, workflows, and stages be reset across
flywheel x toron x beads now that ADR-0012 (doorbell plane D), ADR-0013
(AgentRef/AgentBinding journaled rebind), and toron ADR-0032 (workflow
honesty) are implemented, for optimal integration?

## Verdict

The three ADRs were implemented but the stack was running them degraded:
the doorbell daemon delivered zero wakes (standby, never armed), the model
tier system was a no-op (operator `[defaults]` collapsed all slots onto one
model), and the profile fallback taxonomy ADR-0013 built had zero users.
"Reset" meant audit-and-fix across authoring + ops, not wipe-and-rebuild.

## Captain's ten decisions (grilling rounds, 2026-09-13)

1. **Full integration reset** — authoring (profiles/workflows/stages) PLUS
   the ops faults (doorbell arming, tier defaults, roles/captain, cron
   shadow, ADR flips, skill sync). Not catalog-only.
2. **Restore designed slots + bare-harness routes legal** — remove
   `[defaults]`; cheap=opencode/big-pickle, standard=pi/muse-spark-1.3
   (+glm-5.3-flash model-fallback), premium=omp/zai/glm-5.3. The grok
   harness runs its own default model (model-opaque, no `grok/*` key
   needed) — already implemented in the ladder, verified not wired.
3. **Crew plans are proposals** — the agent recommends (e.g. 3 coder +
   1 oracle), MUST ask the operator to change/remix (role/harness/model/
   count) before spawning, and adapts on the operator's word. Lives in
   AGENTS.md + skills (contract) AND the code (crew-confirm gate).
4. **Wire tier-down fallback chains** — coder→[scout],
   oracle/planner/reviewer→[coder], housekeeper→[scout];
   model_fallback where the registry offers a rung. Every hop rides the
   rebind journal. (No forced live-hop proof required.)
5. **Doorbell: clean slate + host real** — purge dead spawn-* rows, host
   the real workspaces, add a durable operator subscription. Plus: `kill`
   releases its spawn's rows (lifecycle symmetry), a daemon reaper drops
   week-silent spawn rows, and the daemon auto-hosts on subscribe
   (standby only with zero subscriptions). Side answers: no stale-sub
   accumulation by design; hosting on by default, no manual `--host`.
6. **All four culls** — delete 3 one-off user workflows
   (kill9-witness*, CREW-WF), fix project stages (lane-dispatch stale
   verbs, .beads ceremony aligned), profile hygiene (builtin flag,
   deepseek keys), toron skill sync (role auto-bind landed).
7. **Rename the shadowing cron** — `weekly-review` → `weekly-review-sunday`,
   intent explicit (Sun 04:00, oracle, host).
8. **Clear all stale pins** — roles repin via spawn auto-bind, captain
   alias cleared (FCFS per session), legacy slugs purged (no prune verb
   exists — operator follow-up).
9. **ADR-0032 Accepted on captain word** (this session), **P0-5 folds as
   option (a)** — affinity READ from workflow steps, never re-declared.
   The Rev-4 ambiguity worry is answered structurally: profiles are never
   re-typed into routing rows, so no second truth exists.
10. **All in this session** — ops + authoring + code, ledger-tracked here.

## Live evidence witnessed this session

- `doorbell start flywheel` armed nothing with no live fleet
  (`doorbell_project_workspaces` returns empty on an empty spawn registry)
  and the installed 0.6.0 predated bd-enov/F17 — rebuilt from HEAD,
  installed, found the mise shims shadowing the fresh binaries, symlinked
  them, armed `flywheel-flywheel` + `flywheel-toron`, baked hosts into the
  launchd unit (reboot-persistent). Daemon now `hosted`, zero degraded.
- New `doorbell start` adopted-without-hosting when a healthy daemon ran:
  `--host` only rides a fresh serve spawn (lifecycle.rs `start` returns
  after adopt). Restart-with-hosts is the workaround; auto-host-on-
  subscribe (shipped this session) makes it moot.
- Reconcile self-deadlock: `for ... in f(subs, &hosted.lock())` held the
  parking_lot guard across the loop body while `spawn_host_loop` re-locked
  it — non-reentrant, whole runtime wedged behind /health (found via
  `sample`, threads in `lock_slow`). Fixed by binding the list first;
  `spawn_host_loop` also made idempotent (no double-Brain double-ring).
- Fresh-install SIGKILL (-9) under launchd until ad-hoc `codesign -s -`.
- `verify-0013.sh` first run failed on a compile race (scripts built
  main.rs mid-edit); re-ran clean.
- Scout drift caught twice by re-verification: ProfileScout's
  `recipes.toml`/`.bak` paths don't exist; its "unroutable grok rung"
  was wrong (model-opaque step 4 already handles it).

## What landed (flywheel, this session)

- Ops: 20 orphan subscriptions purged; daemon hosted x2 + unit-persistent;
  roles/captain cleared; ADR-0032 flipped + committed (`cf4cec2` toron);
  shims fixed; Resonate verified up (doctor FAIL was a probe artifact).
- Authoring: `[defaults]` removed (per-slot tiers live); tier-down
  fallback chains on all six profiles; 3 workflows deleted; cron renamed;
  lane-dispatch verbs fixed; WIP dirt parked (`d837559`).
- Code: profile projections (`fallback` surfaced, real `builtin` flag);
  crew-confirm gate (non-TTY `--commit` on spawn topologies needs `--yes`,
  TTY asks inline) + contract in AGENTS.md + both skills;
  `Subscription.project` + spawn/cli writers name it; serve reconcile
  (lease-fenced auto-host + 7d reaper); `kill` drops its spawn's rows;
  P0-5 fold (`workflow_role_from_steps` + `check_profile_routes`,
  `decide --json` + `briefing` name `role`, `decide_router_profile_affinity`
  green). Full bin suite 1692/0; doorbell-bin 8/8; doorbell-rt 73/73.

## Post-mortem: the hosted-list lie (found during verification)

- The idempotency guard (`spawn_host_loop` skips already-hosted keys)
  checked a `hosted` list pre-populated from `cfg.hosts` — so every boot
  host looked hosted and was skipped. Health showed hosted strings with no
  Brain behind them: zero sweeps, phantom clean exits. Fixed by starting
  the list empty with `spawn_host_loop` as the sole writer.
- Bisect proved it (old binary: 12 sweeps; new: 0 in identical env), but
  only after chasing phantoms: backgrounded shells ignore TERM
  (environmental — the script's `timeout -s TERM` exists for exactly
  this), port squatters from orphaned smoke serves (the script killed the
  watcher, not serve — hardened with a port-holder kill), and a shared
  temp-dir flake in `daemon_gen_gate` (two tests, one dir — renamed).
- toron ADR-0032: `status: accepted` (captain word 2026-09-13).

## Follow-ups (not this session)

- Human digest sink: only herdr sink is wired in serve; a durable human
  wake needs the webhook/DM sink or a standing operator pane.
- Legacy slug identities (6): no prune verb; operator action.
- `bd-vhia` (flywheel mail migration-10 skew) and `bd-u5yr` (ledger check
  note-field skew): separate beads, loop-owned.
- Portable codec (P2-11) / `dead_code` cleanup (P2-14): still their own items.
- Clock-jump test (ADR-0012 §8): still unchecked.

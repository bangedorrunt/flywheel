# Consolidation campaign — closure record (2026-11-06)

Wave 12 / lane C (`bd-wave12-campaign-report-g96kz`). The durable record of
the consolidation campaign: waves, commits, ledger verdicts, disproofs, live
gaps, and the verification matrix. Every claim carries a sha, path, or
command; a reader who was not in the session can re-verify from here.

## 1. Wave table (verified from `git log --grep` + `flywheel ledger check`)

Verdict LEGAL = re-checked at report time. "Close-commit in log" = the
close ceremony commit exists; the verdict row was not re-read for that bead.

| Wave | Lane | Shipped | Commits | Bead + verdict |
|---|---|---|---|---|
| 4 | B (toron) | durable host → toron-workflow | `4da63cc` | bd-wave4-host-move-hvc9, close-commit in log |
| 5 | toron | ToJson → toron-core | `551441c` | bd-wave5-tojson-core-x7kj, LEGAL (prior lane-C report) |
| 5 | C (flywheel) | toron-mcp dep dropped | `d469d4d3` | bd-wave5-dep-drop-l2pm9, CEILING (forbidden-file churn), close-commit in log |
| 5 | A (flywheel) | main.rs eviction to cmd/*, gate green | `2c62d0f3` (+20 wip) | bd-wave5-main-eviction-d5hht, gate green per message |
| 1 | B (toron) | dispatcher surface → toron-workflow | `ea4ecfb` | bd-wave1-dispatcher-move-9hpl, close-commit in log |
| 6 | toron | pane-plane port inversion | `a1fb7d9` | bd-wave6-plane-port-8udj, LEGAL (prior lane-C report) |
| 6 | flywheel | owner wire-or-delete | `46880a5d` | bd-wave6-flywheel-owner-qrgjb, close-commit in log |
| 7 | C (toron) | parse-or-refuse config + doctor triple | `b30f42a` | bd-wave7-config-strict-dck6, LEGAL |
| 7 | C (toron) | toron-plane crate split | `9342e7a5` | bd-wave7-plane-crate-tx3w, LEGAL (amendment close) |
| 7 | flywheel | doorbell one host loop | `e4afb93d` | bd-wave7-doorbell-host-ng46a, close-commit in log |
| 8 | C (skills) | skills native audit | `375e4ed3` | bd-wave8-skills-audit-1wecs, LEGAL |
| 8 | C (flywheel) | record/slot/inbox disproofs | `50d9e7e0` | bd-wave8-disproofs-buehf, LEGAL |
| 8 | toron | daemon discovery + config test | `d705525` | bd-wave8-daemon-discovery-nxzx, close-commit in log |
| 8 | toron | backend default `auto` fix | `d36e07f` | bd-config-default-test-red-98gy, LEGAL |
| 8 | toron | wake KEEP decision | `57c27ae` | bd-wake-decommission-dsd8, LEGAL |
| 9 | C (toron) | doctor judges live daemon, exit 3 | `236422f` | bd-wave9-doctor-freshness-exmk, LEGAL |
| 10 | C (docs) | plane-split docs truth | `b61bc4f8` + `f27fc9c` | bd-wave10-plane-docs-uhmfi, LEGAL |
| 11 | A | one snapshot producer | `efb72963` | bd-wave11-snapshot-mndgf, LEGAL (unit-test-verified) |
| 11 | B | always-on doorbell host | `14e49c4` | bd-wave11-doorbell-host-yiilt, LEGAL |
| 12 | D (flywheel) | parse-or-refuse FLYWHEEL_* knobs | `c9dc22f3` | bd-wave12-config-strict-n9hcs, LEGAL (amendment close at heal) |
| 12 | A | heal full flywheel suite | `d8094222` | bd-wave12-heal-suite-3r4q3, LEGAL (unit-test-verified) |
| 13 | toron | default discovery store identity | `6077811` | bd-wave13-discovery-isolation-zt1z, closed |
| 14 | toron | with-lease non-git zero-match | `e6ad5f5` | bd-wave14-withlease-nongit-2s0o, closed |
| 14 | toron | comms-flag / backend-ADR-0045 tests | `2fe0f25` / `cc3152f` | both closed |
| 15 | toron | doorbell host PATH + restart | `07444d67` + `98bf6be7` | bd-wave15-doorbell-live-2c6jf, closed |
| 16 | C | presence fold + RunEnv bead | `630c79c8` | bd-presence-runenv-snapshot-fe4bh, closed (decision record) |
| 17 | B | push wake end-to-end on rex | `0420fd0f` | bd-wave17-presence-producer-yjfol, LEGAL (unit-test-verified + live wake) |
| 12 | C | campaign report (this page) | — | bd-wave12-campaign-report-g96kz, OPEN (this bead) |
| 16 | C | campaign refresh (this update) | — | bd-wave16-report-refresh-j228e, OPEN (this bead) |

Earlier lane-C groundwork (close-commits in log, reports linked below):
docs truth sync (`79b0e4a9`/`0936ab7`), skills residuals + core rewrite
(388→180 / 328→176), vocabulary + divergences pages, AGENTS claims audit,
native-UX contract page.

## 2. Evidence links

Lane reports (all exist at report time):
`../../.flywheel/reports/lane-c.md`,
`../../.flywheel/reports/wave0-skills.md`,
`../../.flywheel/reports/wave1-skills-core.md`,
`../../.flywheel/reports/wave2-docs.md`,
`../../.flywheel/reports/wave3-claims.md`,
`../../.flywheel/reports/wave4-native-ux.md`,
`../../.flywheel/reports/wave5-dep-drop.md`,
`../../.flywheel/reports/wave8-skills-audit.md`,
`../../.flywheel/reports/wave8-disproofs.md`,
`../../.flywheel/reports/wave10-plane-docs.md`,
`../../.flywheel/reports/wave11-doorbell-host.md`,
`../../.flywheel/reports/wave12-config-strict.md`.
Toron-side receipts/reports live under `../../../toron/.flywheel/reports/`
(`wave5-tojson.md`, `wave6-plane-port.md`, `wave7-config-strict.md`,
`wave7-plane-crate.md`, `wave9-doctor-freshness.md`,
`wave14-withlease-nongit.md`, `wave13-discovery-isolation.md`).
Disproofs: [consolidation-disproofs.md](consolidation-disproofs.md).
Divergences: [../known-divergences.md](../known-divergences.md).
Vocabulary: [../vocabulary.md](../vocabulary.md).
Native UX: [../agent-instructions/native-ux.md](../agent-instructions/native-ux.md).

## 3. Disproofs (link, not restated)

Record lanes, build slots, inbox reads — see
[consolidation-disproofs.md](consolidation-disproofs.md). All three
candidates DISPROVED as duplicates (documented KEEPs); no follow-up beads.

## 4. Remaining live gaps, exactly

- **Always-on doorbell host** — DONE (`bd-wave11-doorbell-host-yiilt`,
  LEGAL at `14e49c4`): `flywheel daemon start` installs one supervised
  host per armed workspace; live `/health` shows 2 hosted leases,
  12 subscribers, `degraded: []` (observed at refresh time).
- **One snapshot producer** — DONE (`bd-wave11-snapshot-mndgf`, LEGAL at
  `efb72963`): one `snapshot::assemble`; `status`/`progress`/`snapshot`/
  `helm`/`snapshot_reporting` are pure projections; fixture
  `tests/adr0010_d10.rs` proves the doors agree. Residuals (robot/serve keep
  their GLOBAL fleet shape rather than a per-project read, and `status`
  keeps its doorbell/facts/integrations extras) are named in
  `.flywheel/reports/wave11-snapshot.md`.
- **Presence fold / push wake** — DONE (`bd-wave17-presence-producer-yjfol`,
  LEGAL at `0420fd0f`): rex `PlaneStatus` seeds the host model directly and
  rides one 3s poll thread; `Msg::PlaneStatus` folds into the prompt gate's
  presence map; the per-pin inbox cursor no longer advances past an
  undecided wake; `PlanePromptSink` types through the selected plane. Live:
  an idle rex pane showed the typed `[doorbell] plane-P urgent` line with
  no spawn process alive. RunEnv + FleetContext remain a docs-level
  decision (`bd-presence-runenv-snapshot-fe4bh`, closed) riding SnapshotV1.
- **Doorbell host PATH + restart** — DONE (`bd-wave15-doorbell-live-2c6jf`,
  closed).
- **Installed-binary + daemon freshness ritual** — operator procedure, no
  bead: `mbx +nightly-aarch64-apple-darwin install --path . --force`
  (+ doorbell-cli), codesign if macOS kills the signature, `toron daemon
  restart`; `toron doctor --check` exits 3 while stale (live-proven wave9;
  this refresh re-observed exit 3 with the remedy on the live daemon).
- **Full flywheel suite green** — DONE (`bd-wave12-heal-suite-3r4q3`,
  LEGAL at `d8094222`): gate `verify-clippy.sh` exit 0 re-observed by
  this refresh.

## 5. Final verification matrix (real numbers; not-run marked)

| Check | Result | Basis |
|---|---|---|
| flywheel bin tests | 2710 listed (heal lane counted 2702 at close; +8 landed since) | `--bin flywheel -- --list`, refresh run |
| flywheel full suite | green per heal close (unit-test-verified LEGAL at `d8094222`); gate exit 0 re-observed | heal bead + `verify-clippy.sh`, refresh run; full re-run not done by this lane |
| flywheel integration targets | 35 files; green per the same heal verdict | heal bead; not re-run by this lane |
| toron workspace suite | 2111 passed, 0 failed, 10 ignored (119 result lines) | `mbx test --workspace`, refresh run (ambient env; a `TORON_UNDER_TEST` override was found to break one sandbox test and removed) |
| toron lease-TTL flake | not present this run; seen twice on earlier parallel runs (180±5s window vs KDF contention), passes solo | runs wave9 + refresh |
| toron clean-export | check exit 0 in `git archive HEAD` + sibling-HEAD layout | refresh run (sibling `elm-effect-rs` is a path dep; `2d3c81f` clean; test-in-export not run) |
| toron per-crate (earlier) | plane 77, mcp 120, cli-bins 145, workflow 271, core lib 914, cli full 174+1OT (the 1 since fixed) | runs wave7-strict |
| config_strict / doctor legs | 5/5 + 23 suites ok (wave7); doctor_freshness 2/2, serve_build 11/11 (wave9) | prior lane-C runs |
| installs | flywheel 0.10.1, toron 1.6.1, doorbell 0.3.0, hc 0.1.18 | `--version`, refresh run |
| doorbell `/health` | `lease_hosted` 2 leases, `subscribers` 12, `degraded` `[]`, mode standby | `curl 127.0.0.1:18980/health`, refresh run |
| `toron doctor --check` | exit 3 on the live daemon with remedy (`daemon_build` green while stale) | refresh run (re-observed; first proven wave9) |

## 6. What changed for the agent

- **Removed doors**: `flywheel toron|recipes|template|workflows|pipeline`,
  bare `loop`, `loop-status`; toron herdr-TUI/tmux stub. Teach `squad`,
  `workflow`, `loop-mode`, `loop-mode status`.
- **One dispatch**: briefing argv is the door, `decide` the only selector;
  `send` is captain→crew, `channel`+`job` crew→crew, mail private.
- **Mail/br/snapshot**: `mail inbox --since` takes RFC3339, `30s|15m|6h|7d`,
  or `last`; MCP `since_ts` is RFC3339 only; `br` claims route by pin.
- **Typed failures**: store-skew refuses the open; garbage `TORON_*` exits
  nonzero naming accepted forms (never a silent default).
- **TimeSpec**: CLI lenient, MCP strict — the boundary is per-surface.
- **Skills floor**: both SKILL.md ≤180 lines, router+rules+pointers.
- **Plane crate**: backends live in `toron-plane`; the kernel takes them
  through the injected `PanePlane` port; resolver
  `toron_plane::backend::plane`.
- **Doorbell runner**: one host loop, two entries; `watch` is the doorbell,
  `flywheel watch` is hc terminal status.
- **Freshness ritual**: installed binary + daemon judged separately;
  `doctor --check` fails stale with reinstall+restart remedy.
- **Strict config both halves**: garbage `TORON_*` and `FLYWHEEL_*` exit
  nonzero naming accepted forms (wave7-strict + wave12-config-strict,
  the latter LEGAL at the heal commit).
- **Supervised doorbell host**: `daemon start` installs one host per armed
  workspace (wave11 report); `/health` currently shows 2 hosted leases,
  12 subscribers, `degraded: []`.

## 7. Installed-binary verification (2026-11-07)

Fresh binaries, installed from the committed HEADs (not `target/debug`):

- `~/.cargo/bin/flywheel` 0.10.1 @ `c58a024d` (includes the push-wake,
  strict-registry, snapshot, steering-friction and hc-alias commits)
- `~/.cargo/bin/toron` 1.6.1 @ `456245d` (`toron doctor --check` exit 0,
  daemon build newer than source)
- `~/.cargo/bin/doorbell` 0.3.0

Live matrix (all against the installed binaries):

| Surface | Evidence |
|---|---|
| rex wake | `[installed-probe] wake` mail → journal `InboxFetched SwiftBay` with a `Prompt` action at 2026-11-06T18:25:04Z; the idle pane showed `[doorbell] plane-P urgent. Pull: toron next` with no spawn process alive |
| peer-to-peer (collab) | `flywheel collab --to WindyCreek` → thread `collab-20261106T190020015-18036` has 2 messages (ask + `re:`) — the peer read via `toron next` and replied in-thread |
| doorbell host | supervised host runs `~/.cargo/bin/flywheel __doorbell`; `doorbell/<ws>.gen` hash == installed binary; `/health.degraded == []` |
| hc plane wake | `FLYWHEEL_BACKEND=hc flywheel spawn <scratch> --pi 1` → pane `t1p`; a high mail to the pin made the hc pane print `[doorbell] plane-P urgent. Pull: toron next` (typed through the hc backend) |
| hc workspace lookup | after `HcBackend::workspace_list` groups terminals by root and the basename-slug alias, `FLYWHEEL_BACKEND=hc flywheel status <scratch>` resolves `workspace_id: hc:t1r` |

### Residuals found by this pass (exact)

1. **hc send-by-pin** — `FLYWHEEL_BACKEND=hc flywheel send … --to <pin>` answers
   `no terminal "<pin>"`: hc spawn writes a workspace-scoped registry key
   (`<project>-<hash>.json`) while the send resolver reads the project key, and
   the hc pane's `name` is null (the pin lives in labels). One registry key per
   project (or a labels-based resolver) is the fix; `status` and the hc wake
   already work.
2. **hc labels are applied asynchronously** — one `hc ls` snapshot right after
   spawn showed `labels: null`; a later read showed `flywheel_workspace` /
   `toron_pin`. A resolver must tolerate the missing-label window.
3. **Wake cursor is at-least-once** — the per-pin cursor advances on
   Prompt/Digest/queued. A wake row the fold cannot decide is re-fetched until
   the pin is routable (deliberate: no mail is eaten; observed once as a refetch
   before the queued case was counted as a decision).
4. **Two supervision paths** — `flywheel daemon start` installs launchd host
   units while `flywheel cron start` heals detached children; the lease keeps
   exactly one host, but they "Supersede" each other on restart. Pick one owner.
5. **`recover` respawns the captain pin** — a dead registry row for the
   operator's own pin produced a duplicate idle `AmberHarbor` pane. Needs an
   operator policy (skip / `--include-captain`).
6. **Snapshot robot/serve** keep their global fleet shape (per-project
   `assemble` cannot serve them without a second read); RunEnv/FleetContext stay
   a docs-level decision.
7. **Ledger ceremony** — `br close --commit-sha` alone is not a legal verdict;
   the gate note must carry `task=<id> verdict=<gate>` (or a leading `sha=`).
   Captain re-close fixed three lanes this session.

Also improved while steering: host stderr now lands in
`doorbell/<ws>.{out,err}.log` (was `/dev/null`), `crew control` resolves through
the selected plane on rex, a busy `send` names the queued mail door, duplicate
registry rows no longer double the inbox fetch, the wake hint points at
`toron next` (per-message delivery), and the test env guard restores
`FLYWHEEL_BR_BIN`/`PATH` so br/git suites stop racing.

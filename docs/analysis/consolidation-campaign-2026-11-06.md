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
| 11 | A/B | snapshot producer / always-on host | — | bd-wave11-snapshot-mndgf, bd-wave11-doorbell-host-yiilt — OPEN |
| 12 | A | heal full flywheel suite | — | bd-wave12-heal-suite-3r4q3 — OPEN |

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
`../../.flywheel/reports/wave10-plane-docs.md`.
Toron-side receipts/reports live under `../../../toron/.flywheel/reports/`
(`wave5-tojson.md`, `wave6-plane-port.md`, `wave7-config-strict.md`,
`wave7-plane-crate.md`, `wave9-doctor-freshness.md`).
Disproofs: [consolidation-disproofs.md](consolidation-disproofs.md).
Divergences: [../known-divergences.md](../known-divergences.md).
Vocabulary: [../vocabulary.md](../vocabulary.md).
Native UX: [../agent-instructions/native-ux.md](../agent-instructions/native-ux.md).

## 3. Disproofs (link, not restated)

Record lanes, build slots, inbox reads — see
[consolidation-disproofs.md](consolidation-disproofs.md). All three
candidates DISPROVED as duplicates (documented KEEPs); no follow-up beads.

## 4. Remaining live gaps, exactly

- **Always-on doorbell host** — push wake dies with the spawn process; pull
  `captain watch` works. Bead: `bd-wave11-doorbell-host-yiilt` (OPEN).
- **One snapshot producer** — `status`/`progress`/`snapshot`/robot doors
  still read independently. Bead: `bd-wave11-snapshot-mndgf` (OPEN).
- **Presence fold / RunEnv** — no bead filed; confirm with the captain
  whether they ride the snapshot bead or need their own.
- **Installed-binary + daemon freshness ritual** — operator procedure, no
  bead: `mbx +nightly-aarch64-apple-darwin install --path . --force`
  (+ doorbell-cli), codesign if macOS kills the signature, `toron daemon
  restart`; `toron doctor --check` exits 3 while stale (live-proven wave9).
- **Full flywheel suite green** — blocked on the red tree; bead:
  `bd-wave12-heal-suite-3r4q3` (OPEN).

## 5. Final verification matrix (real numbers; not-run marked)

| Check | Result | Basis |
|---|---|---|
| toron-core lib | 917 passed + 1 failed (pre-existing parallel-load flake in `test_default_lease_ttl…`, passes solo; file owned by another lane) | `mbx test -p toron-core --lib`, wave9 |
| toron-cli full | 174 passed + 1 failed at wave7-strict; the 1 (stale hc-default) since fixed, `cli_framework` 7/7 | runs wave7-strict + wave8 lane-D |
| toron-plane / mcp / cli-bins / workflow | 77 / 120 / 145 / 271, all green | runs wave7 |
| config_strict / doctor filter / doctor_freshness / serve_build | 5/5, 23 suites ok, 2/2, 11/11 | runs wave7-strict + wave9 |
| full toron workspace single run | not run | per-crate numbers above instead |
| full flywheel suite | not green at HEAD (eviction WIP red tree) | pending bd-wave12-heal-suite-3r4q3 |
| clean-export (`git archive HEAD` build) | not run | pending |
| installs | flywheel 0.10.1, toron 1.6.1, doorbell 0.3.0, hc 0.1.18 | `--version`, observed 2026-11-06 |
| doorbell `/health` | reachable, `degraded: []`, binaries named | `curl 127.0.0.1:18980/health`, observed |
| `toron doctor --check` | exit 3 on the stale live daemon, remedy named | observed wave9 (daemon_build green while stale — the trap, now caught) |

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

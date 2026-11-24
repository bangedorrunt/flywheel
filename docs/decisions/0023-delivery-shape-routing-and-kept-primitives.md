# ADR-0023: Delivery-Shape Routing and Kept Primitives

- **Status:** Accepted (`D8/R1`)
- **Date:** 2026-11-24
- **Context:** `src/decide.rs` grew to 44 `RouteEntry` rows mixing delivery shapes (`direct`, `reviewed`, `wave`) with non-delivery lifecycle primitives. Per `D8/R1`, delivery routing splits into (a) a pure 3-row delivery-shape table (`DELIVERY_SHAPE_TABLE`) where an LLM or caller may propose a shape and the pure table accepts or refuses it (both recorded in `DecideCmd::proposal` for Tick Law replay), and (b) a named list of 17 kept non-delivery primitives (`KEPT_NON_DELIVERY_PRIMITIVES`). Existing `ROUTING_TABLE` rows stay intact for backward compatibility until callers migrate.

## Pure 3-Row Delivery-Shape Table

| Shape | Primitive | Topology | Argv | When Accepted / Refused |
|---|---|---|---|---|
| `direct` | `steer-direct` | `swarm` | `flywheel steer <target> --direct` | Refused if `snap.contention` (escalates to `reviewed`) or target names `beads`/`tracker`/`gate` without `no tracker` (escalates to `wave`). |
| `reviewed` | `ce-lfg` | `swarm` | `flywheel workflow run ce-lfg --execute --mode pane` | Always accepted. |
| `wave` | `ce-wave` | `swarm` | `flywheel workflow run ce-wave --execute --mode pane` | Refused if target names `no tracker` (downgrades to `direct`). |

## Named Kept Non-Delivery Primitives (17)

`ce-resume`, `ce-debug`, `ce-measure`, `ce-compound`, `ce-triage`, `ce-retro`, `ce-bakeoff`, `join`, `steer`, `spawn-send`, `cron-start`, `compose-parts`, `compose-agent`, `coalition-form`, `loop-work`, `figure-it-out`, `none`.

## Complete 44-Row `ROUTING_TABLE` Mapping

| # | Primitive | First Matcher | Mapping |
|---|---|---|---|
| 1 | `ce-review` | `audit` | (a) delivery-shape: `reviewed` |
| 2 | `ce-debug` | `repro` | (b) kept: `ce-debug` |
| 3 | `ce-debug` | `bug` | (b) kept: `ce-debug` |
| 4 | `ce-measure` | `refactor` | (b) kept: `ce-measure` |
| 5 | `ce-bakeoff` | `prototype` | (b) kept: `ce-bakeoff` |
| 6 | `ce-bakeoff` | `arena` | (b) kept: `ce-bakeoff` |
| 7 | `ce-measure` | `perf` | (b) kept: `ce-measure` |
| 8 | `ce-measure` | `hillclimb` | (b) kept: `ce-measure` |
| 9 | `ce-debug` | `forensic` | (b) kept: `ce-debug` |
| 10 | `ce-resume` | `babysit` | (b) kept: `ce-resume` |
| 11 | `ce-resume` | `pickup` | (b) kept: `ce-resume` |
| 12 | `ce-resume` | `pause` | (b) kept: `ce-resume` |
| 13 | `ce-review` | `interrogate` | (a) delivery-shape: `reviewed` |
| 14 | `figure-it-out` | `figure it out` | (b) kept: `figure-it-out` |
| 15 | `ce-resume` | `ship beads` | (b) kept: `ce-resume` |
| 16 | `ce-lfg` | `feature` | (a) delivery-shape: `reviewed` |
| 17 | `ce-wave` | `goal` | (a) delivery-shape: `wave` |
| 18 | `ce-triage` | `triage-deep` | (b) kept: `ce-triage` |
| 19 | `ce-triage` | `triage` | (b) kept: `ce-triage` |
| 20 | `ce-retro` | `weekly-review` | (b) kept: `ce-retro` |
| 21 | `ce-resume` | `autonomous-work` | (b) kept: `ce-resume` |
| 22 | `ce-compound` | `reflect` | (b) kept: `ce-compound` |
| 23 | `ce-compound` | `compound the learning` | (b) kept: `ce-compound` |
| 24 | `ce-compound` | `sweep the learnings` | (b) kept: `ce-compound` |
| 25 | `ce-retro` | `awareness-digest` | (b) kept: `ce-retro` |
| 26 | `ce-lfg` | `onboard` | (a) delivery-shape: `reviewed` |
| 27 | `join` | `join the fleet` | (b) kept: `join` |
| 28 | `spawn-send` | `one pane` | (b) kept: `spawn-send` |
| 29 | `ce-wave` | `wave-swarm` | (a) delivery-shape: `wave` |
| 30 | `ce-debug` | `repair-until-verified` | (b) kept: `ce-debug` |
| 31 | `ce-wave` | `beads-lifecycle` | (a) delivery-shape: `wave` |
| 32 | `ce-review` | `argue both sides` | (a) delivery-shape: `reviewed` |
| 33 | `ce-lfg` | `brainstorm` | (a) delivery-shape: `reviewed` |
| 34 | `ce-wave` | `break this down` | (a) delivery-shape: `wave` |
| 35 | `ce-wave` | `ship the chain` | (a) delivery-shape: `wave` |
| 36 | `ce-wave` | `convert to beads` | (a) delivery-shape: `wave` |
| 37 | `loop-work` | `work the ready beads` | (b) kept: `loop-work` |
| 38 | `steer` | `beads` | (b) kept: `steer` |
| 39 | `steer-direct` | `no tracker` | (a) delivery-shape: `direct` |
| 40 | `steer` | `crew-steer` | (b) kept: `steer` |
| 41 | `compose-parts` | `compose` | (b) kept: `compose-parts` |
| 42 | `compose-agent` | `shape unknown` | (b) kept: `compose-agent` |
| 43 | `coalition-form` | `contention` | (b) kept: `coalition-form` |
| 44 | `cron-start` | `cron` | (b) kept: `cron-start` |

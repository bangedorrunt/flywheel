# lane3 bd-gmjo DRAFT — ADR-0027 Slice 1 acceptance

**Status:** DRAFT RED — 11/13 fail, 2 pass. Expected until wave-1 impl beads close.
**Bead:** bd-gmjo (acceptance Slice 1: S1/S2 + source-scan guards)
**Lane:** 3 — flywheel/tests/** (primary)
**Commits:** flywheel 631b14e (tests/adr0027_slice1.rs) + toron beads claim (bd-gmjo in_progress)
**Wave-1 blockers:** bd-7fat (closed 047f1bd), bd-6b1y (in_progress), bd-gs8b (open), bd-2u27 (in_progress), bd-gokp (open), bd-dhm1 (open)

## Files
- `/Users/bangedorrunt/workspace/flywheel/tests/adr0027_slice1.rs` — 13 tests, `adr0027_s1_*`, VERIFY `cargo test --manifest-path flywheel/Cargo.toml adr0027_s1`

## S1 — tasked mail corr=FM-GS-012 survives walker death
- Drives real binaries: `toron mail send --project adr27-s1 --as adr27-sender --to adr27-recip --subject ... --body ... --corr FM-GS-012 --wake tasked --priority queue --audience agents`
- Kills walker (new Scratch with same mailbox), asserts `toron receipt get --corr FM-GS-012` still visible (receipts sqlite, not HashMap)
- Empty tasked → one-line result against corr + result_path
- **RED:** `error: unexpected argument '--corr'` — bd-2u27 not yet landed (extra_tags unused), bd-gokp receipts table missing

## S2 — standing empty silent vs tasked empty result
- Standing + empty → no Cmd to human pin; tasked + empty → Cmd records result + result_path
- Checked via workspace.rs tick source scan for standing/tasked/queue/interrupt/audience handling
- **RED:** workspace.rs tick lacks E3 semantics (bd-dhm1 open)

## Source-scan guards (flywheel tests)
- `no watch_sends after tick` — durable.rs still has `actions.append(&mut watch_sends)` + DELIBERATELY comment → RED (bd-gs8b #2)
- `no Diagnose pane_exited filter` — durable.rs 6956-6962 still filters `pane_exited` → RED (bd-gs8b #4)
- `no double bell_model` — main.rs has 2 `bell_model = Default` (7747/7819) → RED (bd-6b1y #6)
- `no status pre-insert` — main.rs pre-inserts statuses before HerdrStatus tick (8006/8444) → RED (#7)
- `WatchDied keeps model` — main.rs discards model' at 8055 → RED (#8)
- `--once calls tick` — doorbell.rs apply_watch_line second reducer still on live path → RED (#9)
- `every live event constructs Msg` — main.rs does construct AckSlaTick/WatchNdjson/HerdrStatus/WatchDied/InboxRows → **PASS** (already green)
- `heal/reclaim/fact/fold` — state.rs ActiveLoop missing heal_states → RED (#14), FetchInbox/Fact already present but not folded
- `why --corr/--seq replay` — doorbell why still greps pin, not folds tick from Default over journals/doorbell-journal-{workspace}.jsonl → RED (bd-6b1y #11)

## Receipt store contract (bd-gokp)
- Pinned: receipts(corr TEXT PRIMARY KEY, state TEXT NOT NULL, wake TEXT, due_at INTEGER, result_path TEXT, updated_at INTEGER NOT NULL) + Receipt { corr, state: ReceiptState, wake: Option<Wake>, due_at, result_path, updated_at } + upsert/get/list
- **RED:** has_table=false has_struct=false — migration/store missing, receipt get/list CLI missing

## Verification
```
cargo test --manifest-path /Users/bangedorrunt/workspace/flywheel/Cargo.toml --test adr0027_slice1 adr0027_s1
# 11 failed (RED) / 2 passed → DRAFT as designed
```

## Next
- Wait for wave-1 impl beads to close; watch `br list --json` (no spin)
- When GREEN (all 13 pass): `br gate report bd-gmjo --gate unit-test-verified --provider CreamFern --status pass --to closed; br close bd-gmjo --commit-sha <sha> --reason ...; mail "[bd-gmjo] done" to captain thread bd-gmjo`
- Wave gates: bd-twcq after bd-gmjo + wave-2 impl; bd-r0mu/bd-z2nk after

## Principles cited
- prove-it-works — scenarios run real binaries, not unit proxies

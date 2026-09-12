<!-- governed-by: ADR-0026, ADR-0032 Spec 7 (I4–I5), ADR-0018, ADR-0007 -->
# GS-043 — Profile fallback + AgentRef rebind (as-is → to-be)

**Date:** 2026-09-12 (Melbourne UTC+10)  
**Scope:** flywheel only. HOLD merge. No mailbox-as-journal.

## As-is (pre-GS-043)

| Surface | Reality |
| :--- | :--- |
| Peer-aware templates | Dual bodies: `integrations/peer-aware.md.tpl` (~10KB spawn) vs `integrations/prompts/peer-aware.md.tpl` (~3KB slash) — **PROVED DIFFER** |
| `Profile.fallback` | Declared; spawn/headless/pane hops audited via `record_reroute_audit` |
| `Profile.model_fallback` | Spawn route ladder (ADR-0007); headless 429 used **registry** `ModelEntry.fallback` first |
| Identity vs role | `SpawnEntry` mixes pin/profile/harness; no typed AgentRef |
| Captain live rebind | `flywheel rebind` = name bind only; `profile set` edits catalog SoT |
| Timeout honesty | Classifier marks timeout non-reroute; risk of inventing hops from ack/timeout remained undocumented |

## To-be (this ship)

| Surface | Contract |
| :--- | :--- |
| Peer-aware SoT | **One body:** spawn inject `integrations/peer-aware.md.tpl`. Slash chrome is thin; render appends SoT. CI `scripts/check-peer-aware-sot.sh` |
| `AgentRef` | `project` + `pin` (+ `npub`) **immutable** for pane lifetime; fail-closed if unbound after settle |
| `AgentBinding` | `profile_name` / `model` / `harness` **replaceable only** via journaled `RebindIntent` |
| Journal | Flywheel event log (`state::log_event`) — **not** mailbox `owed→acked→resulted` (ADR-0032 I6) |
| I4 / I5 | Intent `prepared` before `in_flight`; timeout/empty evidence **never** invent model/harness/profile change |
| Hops | `billing`/`auth`/`provider` → `fallback[]` (profile names); `rate_limit` → `model_fallback[]` then registry fallback |
| Captain path | `flywheel binding-rebind --pin … [--profile\|--model\|--harness]` journals Cmd; does **not** edit `profiles.toml` |
| Catalog authoring | `flywheel profile set --fallback a,b --model-fallback id1,id2` (distinct flags) |

## Prove fallback honesty

1. `bash scripts/check-peer-aware-sot.sh`
2. `cargo test --offline agent_ref:: tests::timeout_alone_never_allows_rebind`
3. `cargo test --offline peer_aware_prompt_chrome_is_thin_not_second_sot`
4. Billing hop: profile with `fallback = ["scout"]`, force 402 → journal row `state=prepared` then `in_flight` then `settled|failed`; no hop on timeout string alone
5. Rate-limit: profile with `model_fallback = ["…"]` → headless journals `ModelFallback` before second harness invoke
6. Captain: `flywheel binding-rebind <project> --pin <pin> --profile scout --note "…"` → spawn registry profile column updates; profiles.toml unchanged

## Auto-fix fold (clear_for_pr:false)

| Item | Fix |
| :--- | :--- |
| Peer-aware SoT | Migrated `CONTROL-PLANE MARKER` (U+2063 + `FLYWHEEL_OP`) + `RED-SURFACE PING` into `integrations/peer-aware.md.tpl`; CI asserts markers in SoT |
| Invent-green `--model` | Fail-loud before Settled: `SpawnEntry.model` is write-only (no spawn/pane/headless consumer). Never invent Settled. |
| I4 ModelEntry.fallback | Headless + pane registry last rung journals via `commit_rebind` |
| A1 npub freeze | `AgentRef::with_npub` no-ops when already `Some` |
| B2 pane 429 ProfileFallback | `commit_rebind_opt` → Settled\|Unknown; empty-evidence else refuses hop + journals Unknown (no silent continue) |
| C1 evidence redact | `redact_evidence` on `journal_rebind` **and** `record_reroute_audit` before persist/eprintln |

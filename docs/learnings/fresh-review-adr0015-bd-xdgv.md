# Fresh-eyes review: ADR-0015 implementation (bd-xdgv)

Scope: flywheel crew-A P-lanes (bd-wn3m, 10 commits) + toron crew-B G-lanes
(bd-lzo6, ADR-0034 + G1–G7). Lens: effect-oriented skill. Method: diff-read
every lane commit + targeted test runs (prove-it-works — ran, not eyeballed).
No code touched (read-only bead); findings only.

## Verdict: NO BLOCKERS. 3 lows + 1 unwired + 3 infos. Trees green.

Test runs: flywheel `--bin flywheel` (retry/breaker/authority/
workflow_catalog/fs_util) 22/22 green; toron-core (policy/jsonl/slots) 30/30
green. Full toron-core lib was 666/0 at lane close; unchanged since for the
touched files (no new commits after e0bfc64).

## Findings (toron G-lanes, own work — adversarial pass)

1. **G5 drivers unwired (the one real catch).** `policy::RetryPolicy`,
   `retry_sync`, `retry_async` have zero production callers — only `Timeout`
   was adopted (G2 poll loop, G6 quiescence). Tested, generic, dead. Fix:
   follow-up bead to migrate one real loop (e.g. a judgment-adjacent wait)
   onto `retry_async`, or delete the drivers. Vocabulary nobody calls is
   shelfware.
2. **G2 (low): `try_wait` Err path leaks the child.** The `Err(e)` arm bails
   without `kill()`. Unreachable in practice (OS-level wait failure), but the
   function promises kill-on-every-exit. One-line fix: kill before bail.
3. **G3 (low, cosmetic): loss-path re-read can report holder-less conflict.**
   If the winner releases between the atomic decision and the holder
   re-read, the loser gets `conflicts` with empty `holders`. Retry succeeds;
   worst case is a confusing message. Acceptable; noted.

## Findings (flywheel P-lanes, crew A)

4. **P4 drift (low): authority centralized, not once.** Module doc says
   "resolve once at main(), thread down", but all 4 call sites
   (work.rs, coalition.rs, herdr_shim.rs, cmd/doorbell.rs) call
   `HostAuthority::resolve()` per use — env re-read every call. Values are
   correct; the once-per-process guarantee P4 names does not exist. Fix:
   cache behind a static, or reword the doc to "single choke point".
5. **P1 jitter is a fixed markup (low).** `delay_for` adds `capped/10` —
   deterministic +10%, never ±. No thundering-herd decorrelation despite the
   name. Toron G5 uses ±25% xorshift. Align or rename to `headroom`.
6. **P1 headline drift (info).** `ask_with_retry` ("judgment POST retry
   edge") was added in 838cb5f then removed in wiring c69e299; judgment POST
   is single-attempt today — identical to pre-ADR behavior, so NO regression,
   but the P1 commit message advertises an edge that exists nowhere.
7. **P3 mtime seconds (low).** Pack-cache invalidation compares
   whole-second mtimes; a sub-second pack rewrite keeps the stale entry.
   Same granularity pattern as the rest of the tree; noted.

## Checked and explicitly clear

- P2 breaker: threshold/cooldown/half-open probe, success reset on dispatch
  win, failure only on terminal fail — sound, tested.
- P5 `pick_workflow` wired at crew preview (exact → unique prefix → loud
  Unknown/Ambiguous); enum has its call site, not orphaned.
- P6 rename probe best-effort (`let _`), correct — a slow fs must not fail
  the write.
- G4 envelope: first error block byte-identical (e2e malformed path green),
  envelope additive; `timed-out` matcher safe.
- G6 quiescence: bounded (2s/50ms), error path degrades to loud
  already-claimed; no open wait.
- G7: all 3 file appenders migrated (presence projection, purge audit log,
  git-facts journal); herdr/operator socket lines are RPC, correctly out of
  file scope.
- P7 live-filter doc + TTL bound: prose-only, nothing to execute.

## Recommendation

File one follow-up bead for G5-driver wiring (finding 1); batch findings
2–5 as a single polish commit. Nothing here justifies revert or re-verify.

# Order-blind fixtures and dead guards in flywheel (bd-9w95)

A sweep of `src/` for two shapes that make a green suite mean less than it
appears to: a **tie resolved by iteration order** (the fixture is order-blind),
and a **guard that can never fire** (the fixture is dead weight). Both are
measured with `scripts/verify-mutations.sh`, not asserted from reading the code.

Run it:

```bash
bash scripts/verify-mutations.sh scripts/mutations/sweep-order.spec.sh  --cargo mbx
bash scripts/verify-mutations.sh scripts/mutations/sweep-guards.spec.sh --cargo mbx
```

## What the sweep found

**1 · `newest_urgent` resolved a shared microsecond by slice order** — FIXED.
The function exists because the wire order cannot be trusted (bd-vbf2 replaced a
`.first()` that cited stale ids). Keying on `created_ts_us` alone left the same
defect one level down: a burst can land two urgent rows in the same microsecond,
and the winner became whichever row the slice ended on. Now keyed on
`(created_ts_us, id)`, with a fixture checked in **both** orders.

The existing test `vbf2_newest_urgent_wins_regardless_of_wire_order` claims
order-independence and only ever used three **distinct** timestamps, so it could
not see this. A fixture whose winner happens to arrive last agrees with
last-wins; ids are content hashes, so "which one wins" is a coin flip and a
fixed insertion order passes about half the time while reading as coverage.

**2 · `skills_injected_for`'s "highest generation wins" was not true** — the
comment was wrong, the code was pinned. `incarnation` counts relaunches of ONE
pane, so two fresh panes serving the same name both sit at `0` — the common case,
not an edge case. Among equal incarnations `max_by_key` returns the LAST such
row, and the registry is append-ordered, so the answer is the latest
registration. The comment now says that, and a test states it in both orders.

**Unresolved and deliberately not "fixed":** `fold_legacy_alias` appends the
legacy file's rows AFTER the canonical ones, so for equal incarnations the
selection can still land on a folded legacy row. Fixing that needs a freshness
key `SpawnEntry` does not carry. Inventing one (this sweep tried
`(incarnation, pane_id)` and withdrew it — a higher pane id is not evidence of a
later registration) would be a semantics change dressed as a tiebreak. **This
needs an operator decision, not a patch.**

**3 · `settle_retry`'s `.max(1)` was dead** — FIXED, DELETED.
`retries` is a `u32`, so `saturating_add(1)` is `>= 1` for every input —
including `u32::MAX`, which saturates to `u32::MAX` rather than to `0`. Two
mechanisms each guaranteed the floor; only one was needed. Re-adding it changes
no test (S4), and dropping the surviving `+1` turns a retry budget into a total
attempt count (S5, RED).

**4 · Three guards are unfired but kept, with the reason recorded.**
`artifact.rs`'s `MAX_EVALUATION_SNAPSHOT_BYTES.saturating_sub(suffix.len())` is
const-vs-const and cannot fire; it is insurance against a **future edit** to the
constant, not a guard on a runtime value. `run_state_recorder.rs`'s
`(next_seq() - 512).max(0)` cannot underflow `i64` and a negative floor prunes
nothing, so it is behaviourally inert today — kept because it states the intent
and the arm that would make it matter is a seq that can go backwards.
`headless.rs`'s `argv.len().saturating_sub(1)` is the interesting one: green
means **no test decides**, and the two readings — "argv cannot be empty" and
"no test says so" — are not the same claim. Left open rather than guessed.

## Two method notes

**A vacuous MUTATION is not a vacuous TEST.** The first cut of S2 was
`.rev()` before `max_by_key`, on the reasoning that reversing the iterator is
the other half of an order-dependent answer. It reported VACUOUS — and that is
a fact about the mutation, not the test: once the tiebreak exists the answer is
order-independent **by construction**, so reversing cannot change it. An
order-blind fixture needs an order-DEPENDENT mutation. S2 became "let the id
outrank the timestamp", which is the real other half of the rule.

**A pre-existing flake will impersonate your mutation.** `wake_identity_tests`
went red roughly 1 run in 3 and green 4/4 in isolation:
`register_spawned_agent_pins_unique_adjnoun_identity` sets only
`TORON_DATA_DIR`, so its spawn registry lands in the ambient `XDG_STATE_HOME` —
and when no sibling test has set one, in the operator's **real**
`~/.local/state/flywheel` (43 `fw-*` fixture files are sitting there now). The
"first spawn" then finds that row instead of minting a pin, and the
colliding-name search — which can only match inside the 9900-pair space — comes
up empty.

Adding `test_state_home()` to the two affected tests was tried and **did not
fix it** (still 3/8 red), so it was reverted rather than shipped as a non-fix.
What did fix the harness was narrowing `TEST_FILTER` to the three hermetic tests
the mutations actually touch: a harness that runs the suite once per mutation
cannot distinguish someone else's flake from its own edit, and the symptom is a
red "control after restore" on an unmutated tree — which destroys the one signal
the control exists to provide.

**Untested, needs a decision:** the 43 fixture files in
`~/.local/state/flywheel/agents/`, and whichever other tests write there. Not
touched — deleting operator state needs permission, and hermeticising the rest
of the module is its own bead.

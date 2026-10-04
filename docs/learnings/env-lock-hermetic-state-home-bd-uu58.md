# An env lock that does not pin the state home is not hermetic (bd-uu58)

Two lessons from one afternoon of chasing 139 junk files in
`~/.local/state/flywheel/agents/`. The first is a product bug. The second cost
more time than the bug and is worth reading first.

## 1. Run long suites in the FOREGROUND, never detached

`mbx +nightly-aarch64-apple-darwin test` takes about five and a half minutes.
Run detached — `nohup bash -c '...' & disown` — and the session harness reaps
the job when the tool call returns its process group.

What it looks like is much worse than it is:

- the test binary vanishes with **no `EXIT=` line, no panic, no crash report**,
  and no live process;
- tests reported so far are all `ok`, zero failures;
- and the death position is **nondeterministic** — measured at 2464 oks,
  2467 oks, and once only 256.

I filed a bead for it (bd-pb39) as a suspected `ENV_LOCK` deadlock, because
several `wake_identity` tests hold the lock across mail-backend work. That was
wrong, and the tell was available from the first measurement: **a real hang or
a real failing test reproduces at a fixed point.** Three different positions is
not a bug in the code under test.

Foreground, the same command returns `2120 passed; 0 failed; 6 ignored in
266.15s`, and the full workspace suite is green on every line.

The general rule: when a long job "dies" silently, sample the death position
across runs before theorising. Constant position means your bug; varying
position means your harness.

Two smaller measurement traps from the same session, both mine, both caught:

- `$?` after a pipeline is the **last** command's exit. `flywheel … | head -5;
  echo $?` reported `exit=0` for a command that actually exits 1, and briefly
  convinced me the product swallowed transport errors. It does not — measured
  without the pipe, `mail agents` exits 1 on an unreachable surface.
- A test filter that names only *some* of a spec's `expect=` tests makes the
  rest report VACUOUS. The suite stays green because the named test was never
  selected. One `TEST_FILTER` covers the whole spec.

## 2. `lock_env()` handed out a lock without hermeticity

`config::lock_env()` is the crate-wide serialiser for tests that mutate process
env. It took `ENV_LOCK` and returned the guard. It did **not** set
`XDG_STATE_HOME`.

Env is process-global, so that is not hermetic. A test taking only that lock
inherits whatever `XDG_STATE_HOME` a sibling left behind — or, when no sibling
pinned one, the operator's real `~/.local/state/flywheel`. Four `main.rs` spawn
tests took only `lock_env()`, so each suite run added four files to `agents/`.
139 had accumulated, and four more appeared during the session that found them.

**The fix belongs in the lock accessor, not at the call sites.** An earlier
attempt added `test_state_home()` to the four tests and was reverted as a
non-fix, correctly — that treats the four that happen to be broken today rather
than the invariant. `lock_env()` is the one function every env-mutating test
already calls; pinning there routes all of them. The return type widened from
`MutexGuard` to the guard struct, which broke exactly one caller
(`models.rs::with_config_dir`) and that was the whole blast radius.

`TestStateHome::drop` also had to **restore** the prior value rather than
remove the variable. Removing it hands the *next* test the real state dir —
the same leak arriving through teardown instead of construction.

### What proving it required

The obvious test — assert `state_dir()` is under temp while holding the lock —
passes for the wrong reason too easily: a guard that pinned some *other* stale
dir satisfies it. So the test asserts two things, and the second is the one
that matters: **two lock-holders never share a directory**, because inheriting
a stale fixture is the whole mechanism.

Restoring the previous value has a second, less obvious trap. `ENV_LOCK` is
not reentrant, so a test cannot set a sentinel and *then* call
`test_state_home()` — that deadlocks the entire binary, and the symptom is a
suite that stops mid-run with no output, which looks exactly like lesson 1.
Both reads in that test happen under the lock for the same reason: reading
after the guard drops without re-taking it races whoever grabbed the lock in
between.

The `Some` arm of the restore is unreachable on a clean process (prior is
`None`, so "restore None" and "remove the var" are the same act), which made
mutation M2 report VACUOUS. Reaching it needs a value already set when the
guard is *constructed*, and the one sanctioned way to set the variable is
inside a guard, which always captures `None`. So the guard is built by hand
from a lock the test already holds — and the lint that flags unguarded setters
had to learn that a raw `ENV_LOCK` **is** a guard. It is: the rule exists to
stop a setter being observed mid-flight, and holding the lock prevents exactly
that.

### The payoff, measured

The leak was also the cause of a documented flake (bd-9w95):
`register_spawned_agent_pins_unique_adjnoun_identity` went red roughly one run
in three, because "first spawn" found a stale registry row left by an earlier
run. After the fix, 20 consecutive runs: **20 passed, 0 failed**, with the live
`agents/` directory holding steady. Baseline for comparison: the same suite run
with the fix stashed pushed that directory from 14 files to 17.

## The general shape

A guard that grants *serialisation* is not the same as a guard that grants
*hermeticity*, and the repo already had a helper for each — they were just
separate. When a test helper takes a lock, ask what else the lock is standing
in for. Here it was silently standing in for "no other test's environment",
which it never did.

Checklist for the next one:

1. Does the helper grant everything its callers assume it grants?
2. Does the cleanup restore, or does it clear?
3. Can every branch of the new logic be driven without the thing that broke?
4. Do the new tests fail when the fix is inverted — and does a VACUOUS verdict
   here mean "the test cannot fail" or "the filter never selected the test"?

### bd-pb39 resolution (2026-10-04, Buffy)

Closed as measured: the foreground suite completed green three consecutive
times (2651 passed, exit 0 each), with both bracketing tests —
`register_spawned_agent_folds_slug_to_pin` and
`register_spawned_agent_pins_unique_adjnoun_identity` — reporting `ok` inside
the full-suite run. Four earlier full-suite runs the same day also completed
without a mid-run death (they failed on unrelated tripwires, since repaired).
No lock-ordering deadlock was found; the death was the detached-run reaper
described above, and the only real code defect in the cluster was the
non-hermetic lock bd-uu58 fixed at `lock_env()`. The durable rule is the
sampling one: a death position that varies across runs is the harness, not
the code under test.
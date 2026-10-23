# Sync interpret ends at the Brain and at detached work (bd-tlct ceiling)

`DurableInterpret::interpret_in_env` (landed for the Audit arm, bd-ma3y)
is synchronous by construction. Auditing every remaining `RtHost::tick`
arm for a host-side sync move found none that fit:

- **Prompt** — delivery is budgets + `inject_allowed` + sinks + digest +
  stats, all owned by the rt Brain (`brain.rs` deliver/gated/deliver_prompts).
  A host sync edge would fork the prompt gate (storm risk).
- **FetchInbox / CheckAckSla** — signals for DETACHED async work by design
  (bd-1i8c: serial dispatch must never wait on subprocess/scan). Sync
  interpret would park the loop.
- **RestartWatch** — execution is already host-side (`restart_wake` +
  Brain single-flight coalescing, bd-8dvf metric). The Effect is
  journal-only; there is nothing to move.
- **FactMiss** — contract keeps `Effect::Audit`.

The gap is **async/detached interpret machinery**, which does not exist;
only sync `interpret_in_env` does. T4 cuts over what is interpretable
(the Audit arm + T1 leaves) and keeps the detached arms on Resonate with
these reasons — not as tech debt, as the documented boundary.

Provenance: bd-tlct ceiling inventory (comment #267), VERIFY
`doorbell_host` green at HEAD, tree untouched.

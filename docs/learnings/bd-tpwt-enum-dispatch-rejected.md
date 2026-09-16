# bd-tpwt — Enum dispatch over `dyn MultiplexerBackend`: evaluated, rejected

**Date:** 2026-09-16
**Bead:** bd-tpwt (lean simplification wave 2)
**Verdict:** do not implement. Score (LOC_saved × Confidence) / Risk < 2.0.

## Proposal

Replace `Box<dyn MultiplexerBackend>` with a closed `BackendKind` enum plus
match dispatch, deleting the trait object (and, per the bead text, the
"parity shim" and `wire.rs`).

## Evidence (measured 2026-09-16, post bd-j36x + bd-euav tree)

- The unified trait (`toron_core::backend::Backend`, re-exported as
  `MultiplexerBackend`) carries ~20 methods with Unsupported-open defaults
  (ADR-0002 §6 open-to-extension).
- 16 call sites consume `backend::multiplexer_backend(cfg)` opaquely
  (`src/main.rs` ×15, `src/durable.rs` tripwire reference ×1).
- An enum needs either a match at all 16 sites or a ~20-arm forwarding
  `impl Backend for BackendKind` (~60–100 lines ADDED, plus factory
  signature churn through `main.rs`/`durable.rs`).
- The set is NOT closed: the `superlogical` stub is reserved by explicit
  operator decision (lands "very soon"); ADR-0002 §6 keeps the surface
  open-to-extension, and tmux may un-hold upstream.
- `dyn` cost lands on cold spawn-path operations only; no hot-path
  measurement justifies it (m10-performance: profile first, and there is
  nothing to profile here — one vtable hop per backend op).
- The `agent_name`/`send` duplication the bead named is already gone:
  `src/backend/agentmail.rs` was deleted by bd-euav, leaving the single
  `ToronMailBackend` implementation.
- `wire.rs` is mail-side (toron remote facade arg builders, golden-tested)
  and `parity.rs` unit tests (error coercion, Unsupported honesty,
  prompt-landing mapping) are backend-agnostic — neither is removed by a
  multiplexer enum. The bead text over-reached on both.

## Decision

Keep `dyn MultiplexerBackend`. Revisit only if: the backend set is
actually closed (superlogical graduates AND tmux is permanently dropped
AND ADR-0002 §6 is superseded), or a profile shows the vtable hop on a
hot path.

## Principles

- laziness-protocol — the smallest change that solves it is zero lines.
- minimize-reader-load — counted the 16 call sites and 20 forwarding arms;
  the enum adds layers, it does not collapse them.

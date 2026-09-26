# bd-s5ng: a degraded-mode warning must name what it observed (2026-09-26)

**Bead:** bd-s5ng (fix landed in commit 2a76588)
**Verdict:** the doorbell health probe was a false-negative generator, and the
false negative was *load-bearing* — it is what made the real wake bug
(bd-8kn9, autohost unimplemented) hard to find.

## The measured numbers (do not re-derive these)

`doorbell serve --port 18980` answering `GET /health`:

- body ~1.4KB JSON, `protocol: "doorbell.v1"`, `mode: "hosted"`
- latency over 12 probes: **min 0.4ms, median 1.0ms, max 506ms**
- old budgets: **200ms connect / 500ms read** — sitting *on* the tail
- at old budgets: **4 of 12 healthy daemons read as dead (33%)**
- at 1s / 2s: **12 of 12 clean**

The payload was never the problem. The budget was tuned by feel, never measured,
and the tail sat exactly on it.

## Why the failure mode mattered more than the false rate

Every `?` in the probe collapsed four distinct conditions into one `None`:

| actual condition | what the operator was told | correct action |
|---|---|---|
| nothing listening | "unreachable — run `doorbell start`" | start one |
| listening, busy past budget | "unreachable — run `doorbell start`" | **re-probe; do not start a second** |
| answered with non-JSON | "unreachable — run `doorbell start`" | investigate the payload |
| answered, wrong protocol | "unreachable — run `doorbell start`" | check versions |

Two of those four had *actively wrong* advice, and the busy case is the common
one. The same status line went on to say `already running (pid 5126, port 18980)
— adopted` from a different code path, so the output contradicted itself on every
run and the false half is the half that also flipped the session into
`embedded degraded mode`.

## The transferable lesson

A degraded-mode warning is a claim about the world. If it cannot name the
condition it actually observed, it is worse than no warning: it is confidently
wrong, and it sends the operator to perform a mutation (start a daemon) that the
system does not need. `HealthProbeFailure` exists so the reason is a value the
caller must handle, not a string it can default to.

Corollary, learned the hard way here: a guard that passes is not evidence the
ledger is right. The bd-s5ng commit cites `bd-2zqk`, an id used for a toron
reservation before the bead existed; the commit guard passed because the
reservation was real. Only `flywheel ledger check` noticed, and it was right.

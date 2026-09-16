# bd-weoe — BoardAgent and the ghost `demo` project

**Date:** 2026-09-16
**Bead:** bd-weoe (P1 identity bug)

## Symptom

BoardAgent (pi session, herdr w6:p5, working the toron repo) carried pane
tokens `toron_pin=BoardAgent, toron_project=demo`. The pin routed nowhere:
`whois toron BoardAgent` and `whois flywheel BoardAgent` both failed.

## Root cause (not code)

- No `demo` manifest exists in the toron index (90 projects, none matching).
- No BoardAgent key material exists under any `projects/*/agents/` dir.
- No flywheel/toron code path defaults a project slug to `demo`
  (toron CLI documents `--project` as required with explicitly no silent
  demo default; flywheel `unwrap_or("toron")`).
- Conclusion: the session was onboarded with an explicit ghost slug
  (hand-typed `--project demo` or a harness-side placeholder), minting
  tokens with no backing registration.

## Fix applied

- `flywheel mail register toron --name BoardAgent` — fresh toron-project
  identity via the same upsert path `spawn` uses (per-project keys are the
  norm; cf. TidyBadger's distinct npubs per project).
- Created + joined shared room `steer-hi` (toron project).
- Handshake mail on thread `handshake-fwl` explaining the re-bootstrap
  step (`toron agent bootstrap --project toron --as BoardAgent`).
- Remainder for the peer/operator: re-stamp the pane tokens by
  re-bootstrapping; until then sends to BoardAgent@toron land in their
  toron inbox, not their pane.

## Principles

- fix-root-causes — proved the slug came from onboarding, not code,
  before touching anything.

# CI is dark for a billing reason, not a repo reason (bd-whie, 2026-09-29)

`gh repo view --json isPrivate` returns `isPrivate: true` for
`bangedorrunt/flywheel`. A private repo draws on the account's metered
Actions minutes, and this account is over its allowance, so GitHub refuses
every job at start. That is the whole signature: five pushes, six jobs
each, every job `failure` in 2-5 seconds with the annotation naming
payments. No code ran, so no code can be at fault.

Nothing in this repository can fix it, and a "fix" that only silences the
annotation would be worse than the dark gate: it would look green. The
local gates stand in for the duration: `bash scripts/verify-clippy.sh`
(exit 0) and `mbx +nightly-aarch64-apple-darwin test` (2084 bin tests
green). Operator decision recorded on the bead.

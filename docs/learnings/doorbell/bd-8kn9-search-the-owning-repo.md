# bd-8kn9: search the owning repo first — autohost was never missing (2026-09-26)

**Bead:** bd-8kn9 (research; RETRACTED and closed)
**Verdict:** my conclusion was wrong. Autohost is implemented, works, and had
been working the whole time. The bug was real; the layer I blamed was not.

## The error, precisely

I filed this bead asserting "autohost is unimplemented — nothing reads
`Subscription.project`", and told the operator the wake path was dead because no
host had ever implemented the consumer.

The evidence I gathered was **true** and the conclusion was **wrong**:

- `grep -rniE 'autohost|auto_host|auto-host'` across **toron** → one hit, an
  unrelated ADR line.
- Nothing in **toron** reads `Subscription.project`.

Both true. Both irrelevant. **The doorbell crates are not in toron.** They are
flywheel's own workspace members — `Cargo.toml` lists `crates/doorbell-core`,
`crates/doorbell-rt`, `crates/doorbell-cli`, and `doorbell serve` is built from
`crates/doorbell-cli/src/serve.rs`. I never grepped the repo that owns the code.

## What was actually there, in the repo I should have opened first

`crates/doorbell-cli/src/serve.rs`:

- `:829-850` — a reconcile task, `reconcile_once` every 60s.
- `:1089-1128` — `reconcile_once` lists subscriptions, computes
  `unhosted_subscribed(subs, hosted_keys(hosted))`, takes a lease per pair,
  prints `auto-hosting {project}/{workspace} (subscribed)`, then calls
  `spawn_host_loop`.
- `:939` — `spawn_host_loop` is documented *"shared by boot-time `--host` config
  and dynamic auto-host"* and is **idempotent**, so boot config and reconcile
  cannot double-ring one workspace.
- `retire_unsubscribed` (bd-mk60) drops hosts whose subscriptions went away.

Both halves I called impossible — a row *adds* a host to a running daemon, a
dead row *removes* one — exist.

## The live proof I already had and did not read

The daemon's `/health` `hosted` list contained `flywheel/flywheel-flywheel`, a
pair never passed via `--host`. I quoted that list earlier in the same session
as evidence for something else and never asked how it got there. After a daemon
restart it **re-populated all 11 pairs by itself**, within seconds, with no
`--host` flag — and the previous log's last line was
`auto-hosting flywheel/flywheel (subscribed)`.

## The transferable lesson

A negative grep is only evidence about the tree you grepped. Before concluding
"X is not implemented", establish **which repository owns X** from the build
graph, not from the name of the binary on `PATH`. `doorbell` sounds like it
belongs to toron — the mailbox project — and it does not; it is a flywheel
workspace member. I inferred ownership from the binary's name, then spent three
wrong hypotheses on top of that inference.

Corollary that actually cost the diagnosis: the status line said
`arm it: doorbell start --host …`, which reads exactly like missing
configuration. A hint that tells you to flip a flag is a claim that the flag
path works. It does not follow that the flag is the problem.

## What was genuinely wrong (and is not this bead)

The wake still did not fire, and the real cause is **bd-lmtu**: the daemon had
leaked threads until it held 4464 of a `ulimit -u` budget of 6000, could not
spawn its own watch threads, and was crash-looping its watch/reconcile loops.
`degraded: ["watch_restart_loop"]` was reporting exactly that, the whole time,
in a status line I read as background noise.

Still open and genuinely unresolved: the arrival subscription is scoped to
`swarm_label(project)` (the label `flywheel-flywheel`) while the live session
scope reported by `flywheel doorbell status` is the herdr workspace id
`adrx2728`, which is therefore never subscribed. `bd-ene2` (leases are per
workspace) depends on the same label-vs-id answer, so resolve it once and reuse
it rather than fixing it in two places.

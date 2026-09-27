---
tier: durable
status: inbox
created: 2026-09-27
provenance:
  bead: bd-5gwk
  commit: pending
  lanes:
    corpus: rg docs/learnings/ — no file states that a crew's registry rows must be read while the run is live
---

# Measure a crew's registry DURING the run, never after the reap

## The mistake

Chasing `pane dispatch: no crew pane for @oracle`, I read
`~/.local/state/flywheel/agents/<project>.json` after a `flywheel steer` run had
finished and found **one** row — an unrelated swarm pane. I concluded the crew
was never persisted, filed bd-5gwk on that premise, and started engineering a
registry write that already existed at `src/pane.rs:895`.

The row count was low because the run had been **reaped**. `spawn_crew_into`
persists every crew row at the end of its loop; the reap then tears the
workspace down and the registry stops describing it. I read the
post-teardown state and called it the spawn-time state.

Re-running the same steer and reading the registry **while the run was live**
showed all six rows, including `crew-goal-oracle`:

```
EmberOtter         profile=None       pane=w1W:p2
crew-goal-coder    profile=coder      pane=w2M:p2
crew-goal-housekee profile=housekeeper pane=w2M:p3
crew-goal-oracle   profile=oracle     pane=w2M:p4
crew-goal-planner  profile=planner    pane=w2M:p5
crew-goal-reviewer profile=reviewer   pane=w2M:p6
```

## The rule

A crew's spawn registry only describes the crew **while the run lives**. Both
the registry and `herdr pane list` are true only inside that window: after the
reap, an empty or short registry is the expected state, not evidence of a
missing write.

So when a run fails on a pane lookup, reproduce it and read the registry
*during* the run, not after:

```bash
flywheel steer "<goal>" --project <p> --crew N --retire-orphans --yes --json > out.json &
sleep 20
python3 -c "import json;d=json.load(open('$HOME/.local/state/flywheel/agents/<key>.json'));[print(e['name'],e['profile'],e['pane_id']) for e in d]"
herdr pane list   # same window: is the crew workspace listed at all?
```

Reading it afterwards produces a confident, wrong answer — the failure mode is
worse than having no answer, because it names a real file and a plausible
mechanism.

## What this cost

bd-5gwk was filed and closed as disproven on a premise that measurement had
already refuted. The bead needed a commit citing it to close, so the disproof
is recorded here instead of in prose that could be lost.

Related: bd-ietv (a registry row that exists but is missing from herdr's
listing is settled rather than bailed — the real, narrower race), bd-8c5h
(the packer folds a role onto another pane and the dispatcher still resolves
`@role` exactly).

# Collaboration recipe catalog

governed-by: ADR-0001

Runnable patterns shipped with flywheel. Agents reading `AGENTS.md` at spawn
should use this page for copy-paste commands. Verification:
`docs/decisions/_scripts/verify-0001.sh`.

---

## 1. Debate (ADR-0001 §9.1)

**Contract:** N-round A/B debate + judge; threaded; ack-gated turns.

```bash
flywheel debate "Should we feature-flag backend X?" --rounds 3 \
  --agent-a oc --agent-b pi --judge gk
```

- Thread: `debate-<ts>`
- A opens FOR; alternate counters; judge summarizes winner
- Requires mail + herdr agents already registered

---

## 2. Plan → review → implement (ADR-0001 §9.2)

**Contract:** planner drafts plan file → reviewer `APPROVE|CHANGES:` (≤2 rounds)
→ implementer (build-slot `cargo`, **paper-first branch, no merge**) → reviewer
`VERIFIED|REJECT:`. Human owns merge/bead close.

```bash
flywheel plan-review ~/workspace/flywheel "Add backend list --json stability docs" \
  --max-change-rounds 2 --bead bd-example
```

- Thread: `plan-<bead|ts>`
- Tokens must lead the reply line (`parse_review_token`)
- Plan path: `.flywheel/plans/<bead|ts>.md`

---

## 3. Peer-triggered handoffs (ADR-0001 §9.4)

**Contract:** agent A finishes a slice and posts kind 9 `@pin` (or a job /
request-release). The workspace doorbell injects `[doorbell]`; B pulls.
No `loop run` required for peer wake.

```bash
flywheel spawn myproj --profile coder --count 2
# crew-to-crew: toron channel post --mention <pin>
# captain steer still: flywheel send --to <pin>
```

- Prefer structured subjects so inbox filters stay simple
- Build-slot before cargo when multiple peers test

---

## 4. Cross-project swarm fan-out (ADR-0001 §9.5)

**Contract:** product topology + contact handshake (one-time); runtime
`[xproj]` message triggers a swarm in the receiving project; results reply
in-thread; requester reads via product search.

```bash
# one-time
flywheel mail product ensure --name trading-research
flywheel mail product link --product trading-research ~/workspace/hachiko
flywheel mail product link --product trading-research ~/workspace/flywheel
flywheel xproj request hachiko --to FlywheelOrch --to-project ~/workspace/flywheel
# receiver approves: flywheel mail contact approve flywheel --from …

# runtime
flywheel xproj send hachiko --to FlywheelOrch --to-project ~/workspace/flywheel \
  --recipe backtest-5m --workers 4 --body "strategy=CrystalRegime"
flywheel xproj parse $'[xproj]\nrecipe=backtest-5m\nworkers=4'   # orchestrator hook
flywheel mail product search --product trading-research "xproj"
```

- Contact policy stays **auto** (not open)
- Receiver: subject prefix `[xproj]` + `parse_xproj_request`

---

## Related

- ADR-0001 (spine), ADR-0002 (backends)
- `AGENTS.md` — agent contracts + product table
- E2E soak stubs: `tests/collab_patterns_e2e.rs` (`--ignored` without test session)

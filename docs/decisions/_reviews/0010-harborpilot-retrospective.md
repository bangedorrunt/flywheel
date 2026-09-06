# HarborPilot retrospective — live-use coordination findings (2026-09-07)

Thread: `adr-0010-peer-review`. Author: HarborPilot. For: KeenAuditor (bead
decomposition + ADR-0010 revision where needed), per captain instruction.

Context: HarborPilot was the FIRST agent on the firstmate-study task — no
crew, no peer, `captain` alias unbound. The mail fabric itself was flawless
(delivery, corr threading across rounds, idempotent acks, file-first review
loop). The real coordination loop was the HUMAN captain manually relaying
KeenAuditor's messages — the human became the mail bridge. Six findings:

- **F1 — `captain` alias unbound at birth.** `to: ["captain"]` hard-failed
  ("unknown recipient: register it first"); the bind
  (`toron contact captain --project <slug> --pin <pin>`) is operator-only and
  was never done for flywheel. A first-arriving agent cannot fix this and
  always fails the same way. Fix: auto-bind `captain` to the operator key at
  `ensure_project` (or first agent registration). [toron bead]
- **F2 — stale roster identities.** With no alias I guessed from
  `list_agents` and picked `captain-session` — a 9-days-stale artifact of an
  old session. Stale session identities are the same rot class as dead herdr
  panes (`cleanup_pane_identities` exists for those). Fix: surface
  `last_active` recency in `list_agents`; prune stale session identities on
  the same cadence. [toron bead]
- **F3 — nobody watched my inbox.** Open-mode collab with no dispatcher: my
  mailbox was write-only until the human steered me (twice). Pieces exist
  (`toron watch --live`; the full doorbell); missing was the contract.
  ADR-0010 D1's thesis proven live: durable record without a wake consumer
  means the human pays the relay tax. Fix: crewless collab agent arms
  `toron watch --live` or pulls `mail inbox --since last` per work unit.
  [contract — LANDED in templates]
- **F4 — "am I alone?" unanswerable at arrival.** I discovered the peer only
  when they mailed me. Toron has presence + channels; unused at arrival.
  Fix: on register-for-task, publish presence / post an arrival line.
  [contract — LANDED]
- **F5 — `mark_message_read` returns `acknowledged:false`.** Shared response
  shape makes a read receipt read as failure. Give it its own semantic
  (`read: true`). [toron-mcp bead, cosmetic but trust-eroding]
- **F6 — contact requests render as ghost mail.** A pending contact request
  surfaced in the inbox as a subjectless empty-body message, indistinguishable
  from corruption. Render typed ("contact request: <pin>") or filter from the
  projection. [toron bead]

## Mail-surface finding (observed while sending this)

Two sends to the same thread with the same `corr` but different
bodies/subjects both returned the FIRST message's event id and did not
index — apparent dedupe on (sender, corr) silently swallowing re-use of a
corr within a thread. Worked around file-first + fresh corr. Fold into the
F5/F6 bead (response/delivery honesty on the mail surface). Reported to the
harness QA channel.

## Contract side — already landed (assuming code beads land)

- `integrations/prompts/open.md.tpl` — "Identity and collaboration
  bootstrap": arrive loudly (presence + roster-with-recency), captain ladder
  (alias, else the pane operator IS the captain channel — never guess roster
  pins), arm an inbox watch, corr + ack discipline.
- `integrations/peer-aware.md.tpl` — boot-time arrival (JOIN + presence idle
  at BOOT, not first doorbell); invariant: address pins by exact identity,
  unknown pin gets whois + recency first; `captain` alias is the only
  title-based address.
- `docs/agent-instructions/orchestration.md` — "Collaboration bootstrap
  (empty-fleet edge)" section with pending-bead list.
- Skill §open-mode bullet extended with the bootstrap summary.
- Verified: every `verify-0005.sh` check covering the templates passes; tpl
  additions are strict ASCII (launch-arg gate); placeholders untouched.

## Pre-existing drift failures (verify-0005.sh, files untouched by me)

1. ADR-0005 doc lost its `### D5 — Captain with and without a loop` heading
   (check "D5 captain with/without loop missing" fails).
2. `integrations/prompts/swarm.md.tpl` no longer carries
   `Loop is bead dispatch, not wake` / `no \`loop run\` required`
   (check "swarm playbook still implies loop is required" fails).

(Third verify failure is environmental: mbx ran cargo `--offline` on a cold
cache — not drift.)

## ADR-0010 revision recommendation

No new Dns. Fold F1/F3/F4 into §12 Implementation Plan as a "Collaboration
bootstrap (empty-fleet edge)" subsection + the captain-fallback contract
(alias unbound → pane operator is the captain channel until F1 lands).
F5/F6 need one line each. Reservation holder's call.

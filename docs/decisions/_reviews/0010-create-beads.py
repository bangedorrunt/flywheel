#!/usr/bin/env python3
"""Create ADR-0010 implementation beads. Grill artifact: 0010-bead-graph.md."""
from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

ACTOR = "KeenAuditor"
VERIFY_BIN = "mbx +nightly-aarch64-apple-darwin test"

# child depends on parent (child blocked until parent closes)
DEPS: list[tuple[str, str]] = [
    ("d1", "d0"),
    ("d2", "d0"),
    ("d2", "d1"),
    ("d3", "d0"),
    ("d4", "d0"),
    ("d4", "d3"),
    ("d14", "d0"),
    ("d14", "d3"),
    ("d5", "d0"),
    ("d5", "d3"),
    ("d6", "d0"),
    ("d7", "d0"),
    ("d8", "d0"),
    ("d9", "d0"),
    ("d10", "d3"),
    ("d10", "d6"),
    ("d12", "d0"),
    ("d13", "d0"),
    ("d15", "d6"),
]


def desc(body: str) -> str:
    return body.strip() + "\n"


BEADS: list[dict] = [
    {
        "key": "d0",
        "title": "ADR-0010 D0: one live WorkspaceModel; delete poll_mail_digests as wake producer",
        "wave": 0,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d0",
        "principle": "boundary-discipline — one live WorkspaceModel; poll_mail_digests is not a Msg producer",
        "desc": desc(
            """
Governed-by: ADR-0010 D0 / Wave A.0. Read docs/decisions/0010-delivery-semantics-attention-triage-decision-lifecycle.md §4 D0.

SCOPE: Close ADR-0027 Option A debt. `cmd_internal_doorbell` remains the single live WorkspaceModel owner. Delete `poll_mail_digests` (`src/durable.rs`) as a wake/Msg producer (digest rendering may remain as a Cmd consumer of already-classified rows). No second live `WorkspaceModel::default()` outside `tick_loop_owner` and tests. Extend the workspace.rs L1769-style source scan so no second construction site survives. D5 MUST NOT land until this bead is closed.

FILES: src/durable.rs, src/main.rs, src/workspace.rs (tripwire), flywheel FAQ Dual-wake pointer.

NON-GOALS: no sibling watcher; do not invent-green that GS-030 already folded this.

## PRINCIPLES
boundary-discipline — one live WorkspaceModel; poll_mail_digests is not a Msg producer
fix-root-causes — delete the second producer instead of absorbing its symptoms

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d0
```
"""
        ),
    },
    {
        "key": "d1",
        "title": "ADR-0010 D1: doorbell payload is toron mail; re-ring ladder plus fire-and-forget skip",
        "wave": 1,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d1",
        "principle": "prove-it-works — delivery survives pane death; ack is the handling fact",
        "desc": desc(
            """
Governed-by: ADR-0010 D1. Read the ADR §4 D1.

SCOPE: `flywheel send --stage` keeps its payload; delivery target is the toron message. herdr agent.prompt shrinks to `[doorbell] inbox: <n> unacked for <pin>; latest corr=<c>; read + ack`. Peer-aware handler: fetch_inbox, process rumor order, acknowledge_message / mark_message_read. Re-ring ladder in workspace::tick: FLYWHEEL_DOORBELL_GRACE_SECS=90, RING_MAX=3; then Dead→respawn, Unknown→digest. Replaces flat 120s deaf-pane digest. `--fire-and-forget <delivery-id>` (caller-supplied id, idempotent re-enqueue) and already-acked corrs skip the ladder. `--raw` without id rings once and does not escalate. Skill-sync: Commands enum, robot_capabilities, AGENTS.md, flywheel skill §5.

FILES: src/workspace.rs, src/send.rs, src/main.rs, integrations/peer-aware.md.tpl.

AC: kill pane between send and ring → message survives, respawned crew acks; duplicate ring → zero duplicate work; fire-and-forget never climbs the ladder.

## PRINCIPLES
prove-it-works — delivery survives pane death; ack is the handling fact
make-operations-idempotent — duplicate doorbell is a no-op

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d1
```
"""
        ),
    },
    {
        "key": "d2",
        "title": "ADR-0010 D2: WakeAck generation-bound watermark",
        "wave": 1,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d2",
        "principle": "make-operations-idempotent — crash between handle and ack re-presents once more",
        "desc": desc(
            """
Governed-by: ADR-0010 D2. Depends on D1.

SCOPE: Msg::WakeAck { pin, through_seq } minted by the crew handler after handling. Presented-but-unacked wakes re-present at the next prompt. Journal records presentation seq and ack seq. flywheel loop replay asserts: no wake dropped, none handled twice.

FILES: src/workspace.rs, replay fold, integrations/peer-aware.md.tpl.

AC: kill simulated handling between presentation and ack; replay shows the wake re-presented exactly once more then acked.

## PRINCIPLES
make-operations-idempotent — crash between handle and ack re-presents once more
prove-it-works — handling is a journaled fact

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d2
```
"""
        ),
    },
    {
        "key": "d3",
        "title": "ADR-0010 D3: Busy|Idle|Unknown|Dead verdicts; kind-9 sparse verbs",
        "wave": 1,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d3",
        "principle": "type-system-discipline — Unknown is never promoted to Busy or Idle",
        "desc": desc(
            """
Governed-by: ADR-0010 D3.

SCOPE: WorkspaceModel.presence becomes Busy{source}|Idle{source}|Unknown|Dead. Unknown never promoted, never absorbs a wake, never prompts. herdr agent.get busy is evidence not authority; native idle is never trusted. Endpoint death → Dead. status prints verdict+source. Kind-9 is a wake log: peer-aware posts only blocked: / paused: (names which human) / needs-decision [key=…] / resolved [key=…]. Never prose progress. Current state = presence verdict + br show.

FILES: src/workspace.rs, status render, integrations/peer-aware.md.tpl.

AC: property test — no consumer branch reachable from Unknown; template contains the four verbs and does not coach progress posts.

## PRINCIPLES
type-system-discipline — Unknown is never promoted to Busy or Idle
boundary-discipline — kind-9 is wake, presence+br is current state

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d3
```
"""
        ),
    },
    {
        "key": "d4",
        "title": "ADR-0010 D4: reservation write-liveness plus 240/3600/3 inspect cadence",
        "wave": 1,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d4",
        "principle": "encode-lessons-in-structure — a long compile is not a dead pane",
        "desc": desc(
            """
Governed-by: ADR-0010 D4. Depends on D3 (Busy verdict).

SCOPE: Drain path (3-poll grace → Respawn+LoudMiss) gains a bounded walk over the bead's toron reservation globs (FLYWHEEL_WRITE_WALK_{PRUNE,MAXDEPTH,TIMEOUT}). File newer than quiet window defers respawn with write-evidence digest. Failed/timed-out walk does NOT defer. Walk only in about-to-respawn branch. Inspect cadence: STALE_ESCALATE_SECS=240, BUSY_TURN_MAX_SECS=3600, WEDGE_INSPECT_COUNT=3. Never auto-interrupt. Auto-kill stays --force-working. paused: uses 3600s resurface.

FILES: src/durable.rs drain branch, src/write_liveness.rs (new).

AC: touch reserved file during grace → no respawn; nothing touched → respawn; failed walk → respawn.

## PRINCIPLES
encode-lessons-in-structure — a long compile is not a dead pane
prove-it-works — write evidence is a file mtime, not a renderer poll

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d4
```
"""
        ),
    },
    {
        "key": "d14",
        "title": "ADR-0010 D14: captain-facing turn-end guard (300s beacon)",
        "wave": 1,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d14",
        "principle": "encode-lessons-in-structure — no captain-facing turn ends blind while work is in flight",
        "desc": desc(
            """
Governed-by: ADR-0010 D14. Depends on D0 (single wake owner) and D3 (in-flight metadata).

SCOPE: If beads are in_progress OR a dispatcher is required, and the wake-owner beacon is stale past FLYWHEEL_GUARD_GRACE_SECS=300, the captain-facing Stop/idle path blocks or forces one recover turn. Crew panes exempt. D5 AFK daemon with identity-matched fresh beacon may satisfy the predicate. Structural backstop, not permission to omit the live cycle.

FILES: captain/steer harness hook adapters (pi/grok/opencode), doorbell liveness beacon.

AC: in_progress + stale beacon → captain turn-end blocked or one follow-up; crew pane same conditions → no block; AFK daemon live → predicate satisfied.

## PRINCIPLES
encode-lessons-in-structure — no captain-facing turn ends blind while work is in flight
boundary-discipline — crew panes are exempt; the fence is captain-facing

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d14
```
"""
        ),
    },
    {
        "key": "d5",
        "title": "ADR-0010 D5: classify wakes in tick; doorbell afk digest (gated on D0)",
        "wave": 2,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d5",
        "principle": "guard-the-context-window — benign/routine wakes spend zero prompts",
        "desc": desc(
            """
Governed-by: ADR-0010 D5. STRUCTURAL GATE: D0 must be closed (one Model, one wake, poll_mail_digests deleted as producer). Also needs D3 Busy for absorb.

SCOPE: Wake classification is pure predicates in workspace::tick: benign (zero prompts), routine (journal+counter), captain-relevant (digest queue), urgent (immediate prompt, 5/30/3 cap unchanged). flywheel doorbell afk on batches captain-relevant per FLYWHEEL_DIGEST_SECS=300 or threshold N. Digest leads with outcomes, keeps bead ids, ends with exact commands. Wedge alarm if digest cannot be delivered. Skill-sync for `doorbell afk`.

FILES: src/workspace.rs, src/main.rs, src/doorbell*.rs.

AC: fixture replay with AFK on: benign/routine zero prompts; digest is exactly the captain-relevant set with ages; urgent bypasses batching; plus D0 gate.

NON-GOALS: do not land this while poll_mail_digests is still a producer.

## PRINCIPLES
guard-the-context-window — benign/routine wakes spend zero prompts
boundary-discipline — D5 is unfinished if a second wake producer still exists

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d5
```
"""
        ),
    },
    {
        "key": "d6",
        "title": "ADR-0010 D6: OPEN DECISIONS fold, captain-hold cache, pending_wait promotes",
        "wave": 2,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d6",
        "principle": "boundary-discipline — only answer/--verdict closes a captain call; TTL promotes never drops",
        "desc": desc(
            """
Governed-by: ADR-0010 D6. Typed br hold --kind captain is consumed when beads D11 lands; do not invent a flywheel-only column. Fold works from receipts sqlite + br JSON now.

SCOPE: OPEN DECISIONS on every status/helm/digest until resolved (cursor-backed). RECORD DIVERGENCE (closed-bead/open-corr, answered-corr/held-bead, [{id}] done without legal close) never auto-closes. open_human is a cache of held beads. Teardown/kill/corr TTL cannot close the call. pending_wait expiry (TTL 3600, cap 512) PROMOTES to OPEN DECISIONS — re-surface, never drop.

FILES: src/workspace.rs, status/helm consumers, receipts sqlite.

AC: buried decision still appears after 20 routine rows; closed-bead/open-corr in divergence; pending_wait expiry appears in OPEN DECISIONS; both clear only on answer/--verdict.

## PRINCIPLES
boundary-discipline — only answer/--verdict closes a captain call; TTL promotes never drops
never-block-on-the-human — decisions age without gating the fleet

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d6
```
"""
        ),
    },
    {
        "key": "d15",
        "title": "ADR-0010 D15: ask-user-authority; worker never self-answers",
        "wave": 2,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d15",
        "principle": "encode-lessons-in-structure — finding labels are evidence, never authority to broaden the contract",
        "desc": desc(
            """
Governed-by: ADR-0010 D15. Depends on D6. Policy, not a new store. Closes through D6/D9.

SCOPE: Worker never answers its own ask-user finding. Reconstruct accepted intent from the brief. Auto-decide unambiguous restorations of accepted behavior. Escalate expansions (new guarantee, threat model, architecture, destructive, security) with five elements: original criterion, proposed expansion, smallest compliant alternative, accept-vs-decline consequences, recommendation. Stage stops, Publishes corr, parks on WaitCorr. Unambiguous-toward-brief may be decided by the captain-facing agent; expansions stay human.

FILES: integrations/peer-aware.md.tpl, ce-execute / ce-review stage templates.

AC: restore-accepted-behavior finding auto-decides with journaled corr; expansion finding parks in OPEN DECISIONS; worker-side auto-close of ask-user is LoudMiss.

## PRINCIPLES
encode-lessons-in-structure — finding labels are evidence, never authority to broaden the contract
never-block-on-the-human — unambiguous restorations do not wait on the captain

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d15
```
"""
        ),
    },
    {
        "key": "d7",
        "title": "ADR-0010 D7: flywheel crew control plus FLYWHEEL_OP marker",
        "wave": 2,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d7",
        "principle": "boundary-discipline — text the worker reads is not a lifecycle verb",
        "desc": desc(
            """
Governed-by: ADR-0010 D7. Skill-sync for `crew control`.

SCOPE: flywheel crew control <pin> interrupt|exit|relaunch (kill/drain move under drain/reap). No arbitrary-text entry point. interrupt: composer-empty where backend can prove it, else cancel=unconfirmed (capability-gated). exit: agent gone, reservation+dirty preserved. relaunch: journaled transaction — checkpoint reservations+dirty+brief, stop, launch same pin with incarnation token, roll durable record back if launch fails; if new record published but agent unconfirmed, keep new record. Dispatcher/loop prompts prefixed U+2063 + FLYWHEEL_OP:. Captain stage payloads stay unmarked. Teardown/discard stays off this plane.

FILES: src/main.rs, control lib, handler template, AGENTS.md, flywheel skill §5.

AC: each verb postcondition asserted; marker round-trips through herdr prompt (byte-checked); relaunch-fail-after-stop restores prior durable record.

## PRINCIPLES
boundary-discipline — text the worker reads is not a lifecycle verb
make-operations-idempotent — relaunch is a journaled transaction with rollback

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d7
```
"""
        ),
    },
    {
        "key": "d8",
        "title": "ADR-0010 D8: receipts degraded state plus printed corr-preserving resend",
        "wave": 2,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d8",
        "principle": "prove-it-works — transport-unknown prints a corr-preserving resend; post-enqueue failure never invites a duplicate",
        "desc": desc(
            """
Governed-by: ADR-0010 D8. Toron addendum on receipts.rs; flywheel sweep/CLI consume it. Prefer CLI-only flags (no new MCP tool).

SCOPE: Receipts owed→acked→resulted gain: (1) transport-unknown prints `flywheel mail resend --corr <c>` — plain re-run mints a new corr and is called out as duplicating; (2) bookkeeping failure after durable enqueue records degraded(acked-unknown) and never invites a resend; (3) uncertain state prefers a rare duplicate wake over silence (D2 makes duplicates safe). catalog_freshness if MCP surface moves.

FILES: ../toron/crates/toron-core/src/mailbox/receipts.rs, flywheel sweep, CLI error paths. Toron ADR addendum required.

AC: failure-injection triad: transport-unknown prints resend; post-enqueue bookkeeping → degraded + no resend invitation; duplicate wake is a no-op through D2.

## PRINCIPLES
prove-it-works — transport-unknown prints a corr-preserving resend; post-enqueue failure never invites a duplicate
make-operations-idempotent — duplicate over silence

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d8
```
"""
        ),
    },
    {
        "key": "d9",
        "title": "ADR-0010 D9: --resolve-bead structural done-hold (drop subject match)",
        "wave": 2,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d9",
        "principle": "subtract-before-you-add — delete subject-substring matching; the bead= tag is the hold",
        "desc": desc(
            """
Governed-by: ADR-0010 D9. Clean cutover — no dual-match shim. Skill-sync on toron skill for --resolve-bead.

SCOPE: Dispatch-guard done-hold stops matching subject ending `] done`. toron mail send --resolve-bead bd-### stamps bead=<id> (corr-linked). Guard matches the tag. Subject stays decoration for humans. Durable close remains br close. Loop reporter-of-record [{id}] done keeps subject and gains the tag. CLI-only flag preferred.

FILES: ../toron/crates/toron-core/src/mailbox/send_tags.rs, flywheel dispatch-guard, toron skill, flywheel skill.

AC: tag present + arbitrary subject → hold fires; tag absent + `] done` subject → hold does NOT fire.

## PRINCIPLES
subtract-before-you-add — delete subject-substring matching; the bead= tag is the hold
type-system-discipline — close/hold is a tag, not a participle

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d9
```
"""
        ),
    },
    {
        "key": "d13",
        "title": "ADR-0010 D13: captain/steer subagent-guard (delegation-shaped deny)",
        "wave": 2,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d13",
        "principle": "encode-lessons-in-structure — untracked harness spawn is a tool-surface fence, not a prompt",
        "desc": desc(
            """
Governed-by: ADR-0010 D13. Evidence class: firstmate commit 50cc24a. The fence does not hinge on the 73-minute figure.

SCOPE: Captain/steer (dispatcher-owning primary) deny delegation-shaped harness tools: stems agent, subagent, task, workflow, spawn, dispatch, worktree, delegate. Exclude mcp__*. Allow observe/stop (taskoutput/taskstop/tasklist) and session-local todo (taskcreate/taskupdate). Crew panes exempt. Fence sits on the tool surface (PreToolUse / equivalent), because the failure is absence of spawn-registry metadata. Prompt standing order in swarm/loop/open tpls is defense-in-depth, NOT the fence.

FILES: captain/steer harness hook/deny matcher script, integrations/prompts/{swarm,loop,open}.md.tpl.

AC: delegation-shaped name on captain profile denied; observe/stop works; crew pane can still be flywheel spawn'd.

NON-GOALS: do not ship a Claude-only deny list in tracked settings that would disarm crewmates.

## PRINCIPLES
encode-lessons-in-structure — untracked harness spawn is a tool-surface fence, not a prompt
fix-root-causes — fence the tool that created untracked work

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d13
```
"""
        ),
    },
    {
        "key": "d10",
        "title": "ADR-0010 D10: flywheel-snapshot.v1 plus helm drain",
        "wave": 2,
        "priority": 1,
        "verify": f"{VERIFY_BIN} adr0010_d10",
        "principle": "subtract-before-you-add — one snapshot contract; status/view/watch/steer_brief consume it",
        "desc": desc(
            """
Governed-by: ADR-0010 D10. Depends on D3 (verdicts) and D6 (OPEN DECISIONS). Skill-sync for helm and snapshot.

SCOPE: flywheel-snapshot.v1 assembled once from workspace model, beads JSON, receipts sqlite, reservations (schema in the ADR). status/view/watch/steer_brief consume it. flywheel helm is the one session-start drain: always prints OPEN DECISIONS, UNREAD STATUS, RECORD DIVERGENCE even on empty queue (empty-state sentences allowed). loop why stays the mechanic's tool. robot_capabilities entry.

FILES: src/snapshot.rs (new), src/main.rs (helm, snapshot), consumers refactored.

AC: all four surfaces render from one snapshot call; robot JSON validates; helm on empty queue still prints the three sections.

## PRINCIPLES
subtract-before-you-add — one snapshot contract; status/view/watch/steer_brief consume it
experience-first — helm is the one returning-operator drain

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d10
```
"""
        ),
    },
    {
        "key": "d12",
        "title": "ADR-0010 D12: learnings tier/reinforced plus bounded GATE projection",
        "wave": 2,
        "priority": 2,
        "verify": f"{VERIFY_BIN} adr0010_d12",
        "principle": "guard-the-context-window — GATE is a budgeted projection of docs/learnings, not the corpus",
        "desc": desc(
            """
Governed-by: ADR-0010 D12. No vector store (ADR-0009 non-goal).

SCOPE: docs/learnings/ frontmatter gains tier: pinned|aging|perishable and reinforced: YYYY-MM-DD. ce-compound/ce-sweep: reinforce on re-verification, archive (don't delete) — aging 30d, perishable 7d and must name a checkable expiry. GATE grounding read is top-N by reinforcement recency within FLYWHEEL_GATE_BUDGET (~4k tokens), frontmatter + rg. Migration: ce-sweep backfills tier: aging + reinforced: today so GATE does not empty on day one.

FILES: docs/learnings/ (ce-sweep), GATE projection in src/stages.rs, ce-sweep rules.

AC: GATE output bounded under budget with 50 seeded learnings; un-reinforced 40d aging entry absent from projection, present in corpus.

## PRINCIPLES
guard-the-context-window — GATE is a budgeted projection of docs/learnings, not the corpus
encode-lessons-in-structure — decay is frontmatter, not a prompt

## VERIFY
```
mbx +nightly-aarch64-apple-darwin test adr0010_d12
```
"""
        ),
    },
    {
        "key": "d11",
        "title": "ADR-0010 D11: beads successor ADR for deliverable, promotes, and captain-hold",
        "wave": None,
        "priority": 2,
        "type": "docs",
        "verify": "test -f ../beads/docs/decisions/0005-scout-ship-deliverable-and-captain-hold.md && rg -q 'deliverable' ../beads/docs/decisions/0005-scout-ship-deliverable-and-captain-hold.md && rg -q 'hold --kind captain' ../beads/docs/decisions/0005-scout-ship-deliverable-and-captain-hold.md",
        "principle": "boundary-discipline — flywheel does not ship a silent beads schema column",
        "desc": desc(
            """
Governed-by: ADR-0010 D11. This bead is a REPORT: author the beads ADR. Do not implement beads schema in flywheel. Do not close via unit-test-verified.

SCOPE: In ../beads write docs/decisions/0005-scout-ship-deliverable-and-captain-hold.md (next number after 0004). It must specify: typed deliverable diff|report; report beads refuse unit-test-verified (close gate = artifact existence / result_path); br create --promotes <id>; br hold --kind captain with corr bound; teardown/kill/TTL cannot close a captain-held row. Flywheel D6 consumes the field when present.

AC: the named ADR file exists and states deliverable, promotes, and hold --kind captain. Captain of beads repo still accepts that ADR separately.

NON-GOALS: no flywheel-only column; no schema migration in this flywheel change.

## PRINCIPLES
boundary-discipline — flywheel does not ship a silent beads schema column
subtract-before-you-add — one successor ADR in the owning repo

## VERIFY
```
test -f ../beads/docs/decisions/0005-scout-ship-deliverable-and-captain-hold.md && rg -q 'deliverable' ../beads/docs/decisions/0005-scout-ship-deliverable-and-captain-hold.md && rg -q 'hold --kind captain' ../beads/docs/decisions/0005-scout-ship-deliverable-and-captain-hold.md
```
"""
        ),
    },
]


def run(argv: list[str]) -> dict | str:
    p = subprocess.run(argv, check=True, capture_output=True, text=True)
    out = p.stdout.strip()
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        return out


def create(bead: dict) -> str:
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as fh:
        fh.write(bead["desc"])
        path = fh.name
    wave = bead.get("wave")
    labels = "adr-0010" if wave is None else f"adr-0010,wave-{wave}"
    argv = [
        "br",
        "create",
        "--actor",
        ACTOR,
        bead["title"],
        "--type",
        bead.get("type", "task"),
        "-p",
        str(bead["priority"]),
        "--verify",
        bead["verify"],
        "--principle",
        bead["principle"],
        "--labels",
        labels,
        "--description-file",
        path,
        "--json",
    ]
    if wave is not None:
        argv.extend(["--wave", str(wave)])
    data = run(argv)
    if isinstance(data, list):
        rec = data[0]
    elif isinstance(data, dict):
        rec = data.get("issue", data)
    else:
        raise SystemExit(f"unexpected create output: {data!r}")
    bid = rec.get("id") or rec.get("issue_id")
    if not bid:
        raise SystemExit(f"no id in {json.dumps(rec)[:400]}")
    print(f"created {bead['key']} -> {bid}", file=sys.stderr)
    return bid


def main() -> int:
    ids: dict[str, str] = {}
    for bead in BEADS:
        ids[bead["key"]] = create(bead)
    for child, parent in DEPS:
        run(["br", "dep", "add", ids[child], ids[parent], "--type", "blocks"])
        print(f"dep {child}({ids[child]}) blocks-on {parent}({ids[parent]})", file=sys.stderr)
    Path("docs/decisions/_reviews/0010-bead-ids.json").write_text(json.dumps(ids, indent=2) + "\n")
    print(json.dumps(ids, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

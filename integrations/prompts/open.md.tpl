{{TOOL_FRONTMATTER}}
# Orchestration: open-mode

Run the /open-mode slash command: direct, uncoordinated agent mode.

## AUTHORITATIVE OBJECTIVE (GS-039 P0-2)

Arguments / `--raw` / current-pane operator text supplied by the harness for
this invocation are **AUTHORITATIVE**. They are NOT room chatter, NOT
peer-aware wake noise, and NOT optional. Execute them even when a prior
diagnose/captain TASK-ID is still visible in the scrollback.

Priority in open-mode (overrides peer-aware chatter rules):
1. Current operator/raw objective for this pane (create-file, edit, report).
2. Explicit human follow-ups in this pane.
3. Only then: captain dispatch / peer coordination if still compatible.

Do **not** classify an open-mode create-file / edit / report objective as
"non-authoritative chatter". Do **not** idle awaiting captain dispatch when
an open-mode objective is present. Peer-aware OPERATING CONTRACT does not
outrank open-mode operator text.

Open mode is for working with the current harness/pane without asking
flywheel to create a crew, run a durable bead loop, or install swarm
coordination. It is the least opinionated mode.

## Model choice

For work that will be launched through flywheel, ask the operator whether to
use the current profile default or a compatible registered model alternative.
Default model for profiles in a task is `opencode-go/muse-spark-1.3-contributor`
unless the operator overrides. Do not invent model ids or bypass registry
compatibility. For direct in-pane work, keep using the current harness/model
unless the operator requests a change.

## Arrive then brief (ADR-0011)

Session start is `flywheel arrive <project> --as <pin>` then
`flywheel briefing "<objective>" --project <p> --json`. Open-mode
stays current-pane when briefing topology is `open` or `join`.
Join-loud prints JOINING, not spawning. Do not fork a dispatcher
when one is already up; attach `toron watch --live`.

## Contract

- Work only on the task supplied in this invocation or by the human.
- Do not spawn agents, split panes, start a dispatcher, or start
  `loop-mode` unless explicitly asked.
- Do not assume a captain, crew, bead graph, wave, or steer channel exists.
- Use the current harness and its available tools directly.
- If the task names tracked work, follow the repository's local work contract;
  open mode does not automatically claim, dispatch, verify, or close it.
- If collaboration is explicitly requested, use the configured Toron mailbox
  and preserve the distinction between shared channel posts and private mail.
- If the task requires multi-agent lanes, stop and recommend `/swarm-mode`.
- If the task requires durable solo execution or crash-resume behavior, stop
  and recommend `/loop-mode`.

## Identity and collaboration bootstrap

Open mode may operate with a direct registered mailbox identity, but it does
not imply the project's Toron identity mode is `swarm`. Use the public mail
identity supplied by the environment for `toron`, presence, reservations, and
mail. Do not invent a pin or use an internal pane slug as a public identity.

If you register a mail identity for a collaboration task, bootstrap it: the
mailbox is write-only until you do, and the human must never become the relay:

- **Arrive loudly.** Check the roster first (`toron` list agents; note each
  entry's recency), then publish presence
  (`toron presence publish --state working`) so the next agent discovers you
  instead of being surprised by your mail. Roster entries can be stale
  session artifacts; treat an identity with no recent activity as absent,
  never address it.
- **Resolve the captain channel by ladder, never by guessing.** Try the
  `captain` alias; if it is unbound, the operator in THIS pane is the
  captain channel. Report outcomes in chat and keep mail for peers only.
  Do not pick a captain-looking pin from the roster (name-similarity mailed
  a 9-day-dead session once; that failure is why this ladder exists).
- **Arm an inbox watch.** No dispatcher will ring you in open mode. Run
  `toron watch --live` in the background, or pull
  `toron mail inbox --since last` after each work unit, so a peer's message
  must reach you without the human relaying it.
- **Mail discipline.** Mint a `corr` per collaboration thread and keep the
  `thread_id` across rounds; reply file-first (findings over ~20 lines go to
  a durable file, the mail carries summary + path); `acknowledge_message`
  every `ack_required` message you have acted on; the ack is the delivery
  contract, not the send.

## When to choose another mode

- `/open-mode`: one immediate task, current pane, no orchestration lifecycle.
- `/loop-mode`: one objective, one current harness, durable execution and
  optional tracked work.
- `/swarm-mode`: captain-led multi-crew execution with lanes and optional
  work-graph supervision.

Builtin playbooks are discoverable via `flywheel decide <target>` (routes to the playbook) and `flywheel workflow list --json` (lists builtins with their decide trigger).

Continue the supplied task. Report what you changed and what you verified;
do not claim bead closure or swarm completion unless those actions were
explicitly performed.

## NO HARNESS-NATIVE DELEGATION (ADR-0010 D13)

Open-mode panes can own a dispatcher (peer-aware). Do not spawn work through
harness-native delegation tools (agent/subagent/task/workflow/spawn/dispatch/
worktree/delegate-shaped) bypass the spawn registry, so the doorbell,
dispatcher, and guards see nothing. Fence: `flywheel __subagent-guard <tool>`
(exit 2 = deny); observe/stop verbs and session-local todo stay legal;
mcp__* always allowed; crew panes exempt (`--role crew` /
`FLYWHEEL_ROLE=crew`). Spawn help via `flywheel spawn`. Defense-in-depth:
the fence is the guard, not this text.

{{VERSION}} {{GENERATED}}

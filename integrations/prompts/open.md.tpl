{{TOOL_FRONTMATTER}}
# Orchestration: open-mode

Run the /open-mode slash command: direct, uncoordinated agent mode.

Arguments are supplied by the harness adapter above. Treat the supplied text as
this invocation's objective and scope.

Open mode is for working with the current harness/pane without asking
flywheel to create a crew, run a durable bead loop, or install swarm
coordination. It is the least opinionated mode.

## Model choice

For work that will be launched through flywheel, ask the operator whether to
use the current profile default or a compatible registered model alternative.
Do not invent model ids or bypass registry compatibility. For direct in-pane
work, keep using the current harness/model unless the operator requests a
change.

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

## Identity and project mode

Open mode may operate with a direct registered mailbox identity, but it does
not imply the project's Toron identity mode is `swarm`. Use the public mail
identity supplied by the environment for `toron`, presence, reservations, and
mail. Do not invent a pin or use an internal pane slug as a public identity.

## When to choose another mode

- `/open-mode`: one immediate task, current pane, no orchestration lifecycle.
- `/loop-mode`: one objective, one current harness, durable execution and
  optional tracked work.
- `/swarm-mode`: captain-led multi-crew execution with lanes and optional
  work-graph supervision.

Continue the supplied task. Report what you changed and what you verified;
do not claim bead closure or swarm completion unless those actions were
explicitly performed.

{{VERSION}} {{GENERATED}}

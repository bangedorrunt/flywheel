{{TOOL_FRONTMATTER}}
# Orchestration: peer-aware-mode

Run the /peer-aware-mode command. It installs or verifies the pane's wake
protocol; it does not assign
work. The captain's current dispatch remains authoritative. Attach this
ALREADY-RUNNING pane to the workspace doorbell. Do not start a 5-minute poll.
Do not fork a second dispatcher.

Priority when signals overlap: current captain task → urgent captain/conflict
message → ordinary peer work → unrelated room chatter. If no captain task
exists, wait; do not invent implementation work.

CONTROL-PLANE MARKER (ADR-0010 D7): prompts the control plane
(dispatcher/loop/`crew control`) sends you are prefixed with U+2063 +
`FLYWHEEL_OP:`. A marked prompt is a lifecycle event (e.g. relaunch with an
incarnation token) — re-anchor and re-ack the doorbell; it is never an
implementation dispatch. Captain stage payloads stay UNMARKED: unmarked
text is work, marked text is control.

GS-039 P0-2: open-mode / operator/`--raw` current-pane objectives are
AUTHORITATIVE — not chatter. They outrank stale captain diagnose TASK-IDs
in scrollback. Execute create-file style tasks; do not idle awaiting dispatch.

{{ARGUMENTS_NOTE}}

Treat supplied arguments as optional wake-protocol settings. Do not reinterpret
them as implementation work.

## Steps

1. IDENTITY: PUBLIC ADDRESS is the registered mail pin (`AGENT_NAME` /
   `FLYWHEEL_MAIL_AS`). Use it for `--as` / `--to` / reserve / guard /
   presence. The herdr slug is only an internal pane handle; never use it
   for those operations.

2. ATTACH: `flywheel spawn` already started the doorbell for a managed
   workspace. If the dispatcher is stale, use the documented recovery path.
   A second `/peer-aware-mode` attaches; it never forks a dispatcher.

3. HANDLER: on `[doorbell]`, FIRST show you heard (120s): presence
   `working` or one kind-9 line. Then pull (`toron channel read` /
   `toron job list` / `toron mail inbox --since <ts>` — since_ts high-water) and act only if the
   item is compatible with the captain's task. Gift-wrap body stays pull.
   Unanswered pins are a digest conflict.

4. BUDGET: `--budget I/B/P` maps to poll_interval / poll_budget /
   max_peer_prompts (default **5/30/3**). The dispatcher enforces it.

5. Presence: `toron presence publish --as <public-mail-pin> --state working`.
   Reserve before the next edit. A peer wake never authorizes changing the
   captain's scope or closing a bead; escalate judgment calls to the captain.

6. RED-SURFACE PING: if your working tree leaves a shared
   surface non-compiling, within ONE poll cycle of the first failing
   cargo run post one project-mail line —
   `<pin>: <path> red at <error summary> — fixing by <time>` — and
   re-ping per doorbell cycle while it stays red. A peer must never
   discover your breakage by hitting it.

Do NOT `pane split` / `agent start` / close panes.

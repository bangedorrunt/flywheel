{{TOOL_FRONTMATTER}}
# Orchestration: peer-aware

Attach this ALREADY-RUNNING pane to the workspace doorbell. Do not start
a 5-minute poll. Do not fork a second dispatcher.

{{ARGUMENTS_NOTE}}

## Steps

1. IDENTITY: you are the pane's pin (`AGENT_NAME` / `FLYWHEEL_MAIL_AS`).
   Use it for `--as` / `--to` / reserve / guard / presence. Never the
   herdr slug.

2. ATTACH: `flywheel spawn` already started the doorbell if this is a
   flywheel swarm. If the dispatcher pid is stale, the next spawn heals
   it. A second `/peer-aware` ATTACHES (same lock), it never forks.

3. HANDLER: on `[doorbell]`, FIRST show you heard (120s): presence
   `working` or one kind-9 line. Then pull (`toron channel read` /
   `toron job list` / `toron mail inbox --since <ts>`) and act.
   Gift-wrap body stays pull. Unanswered pins are a digest conflict.

4. BUDGET: `--budget I/B/P` maps to poll_interval / poll_budget /
   max_peer_prompts (default **5/30/3**). The dispatcher enforces it.

5. Presence: `toron presence publish --as <pin> --state working`.
   Reserve before the next edit.

6. RED-SURFACE PING (bd-kfy2): if your working tree leaves a shared
   surface non-compiling, within ONE poll cycle of the first failing
   cargo run post one project-mail line —
   `<pin>: <path> red at <error summary> — fixing by <time>` — and
   re-ping per doorbell cycle while it stays red. A peer must never
   discover your breakage by hitting it.

Do NOT `pane split` / `agent start` / close panes.

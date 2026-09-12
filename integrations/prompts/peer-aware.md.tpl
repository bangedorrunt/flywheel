{{TOOL_FRONTMATTER}}
# Orchestration: peer-aware-mode

Run the /peer-aware-mode command. It installs or verifies the pane's wake
protocol; it does not assign work. The captain's current dispatch remains
authoritative. Attach this ALREADY-RUNNING pane to the workspace doorbell.
Do not start a 5-minute poll. Do not fork a second dispatcher.

**SoT:** the handler body is `integrations/peer-aware.md.tpl` (spawn inject).
This slash-command file is thin chrome only — CI refuses a second full body.
The rendered `/peer-aware-mode` command appends that SoT at integration time.

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

3. HANDLER: on `[doorbell]`, follow the SoT handler body appended below
   (priority ladder, AUTHORITATIVE open-mode carve-out, pull-then-act).

4. BUDGET: `--budget I/B/P` maps to poll_interval / poll_budget /
   max_peer_prompts (default **5/30/3**). The dispatcher enforces it.

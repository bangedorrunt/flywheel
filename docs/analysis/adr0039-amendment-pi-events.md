# ADR-0039 amendment — pi events bridge (D8 U11, bd-4n2bn)

ADR-0039 D8 intends each harness to subscribe to toron events and turn a wake
into an inbound turn. The Claude channel bridge (bd-si4b5) uses our stdio
`flywheel mcp-events` server; opencode uses the same local server via a per-pane
config (bd-o1z4m).

## Deviation

For pi/omp seats we do **not** use the installed `pi-mcp-events` extension,
despite the "reuse the extension, write config not code" principle. Version
0.1.1 (also the latest on npm) registers only tools
(`mcp__<server>__events__<event>__{poll,stream,subscribe,…}`, scan, status). The
model must call those tools manually; the extension never calls
`pi.sendMessage` and has no `deliverAs`/`followUp`/`steer`. Pi's own
`ExtensionAPI.sendMessage(text, {deliverAs})` supports turn delivery, but the
installed extension does not use it. Therefore it cannot satisfy U11's
"mail.addressed arrives as a pi turn" verify with config alone.

We instead ship a small first-party extension at
`extensions/pi-flywheel-events/`:

- reads the bearer token from the `FLYWHEEL_TORON_TOKEN_FILE` **path** itself,
  re-reads on 401 — no plaintext in process env, MCP config or journal;
- subscribes to `events/stream` with an `events/poll` fallback and a cursor
  persisted in the seat's agentDir;
- coalesces a burst into one `pi.sendMessage(hint, {deliverAs: steer|followUp})`
  — normal mail follows up, urgent/ack-sla steers;
- carries only the bus event id/name hint, never the mail body;
- dedupes by bus event id against a bounded shared rung file
  (`wake-rung.jsonl`) the doorbell writes when it rings, so a seat reached by
  both paths gets one turn.

Spawn wires it per seat through a dedicated agentDir under the flywheel state
dir. The operator's `~/.pi` is read (to preserve model/provider settings) but
never modified. omp remains pending a set mise version.

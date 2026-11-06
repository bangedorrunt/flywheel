# Known divergences — deliberate deltas between prose and the stack

Every row names the code or ADR that governs it. If a row has no governor, it is
a bug, not a divergence — file a bead.

1. **Tool count: "38 tools" in docs vs 41 live MCP tools.** The frozen 38-tool
   surface is the AM-Rust parity contract (governor:
   `toron/docs/parity/am-rust-tool-catalog.md`); the 3 additive tools ride
   ADR-0033 (governor:
   `toron/docs/decisions/0033-additive-tools-kinds-discipline.md` — ADR +
   catalog regen + tests + versioning). Live surface is 41 = 38 + 3 (governors:
   `toron/AGENTS.md`, both skills' front matter). Rows that say "38 tools"
   (`toron/README.md` diagram, mailbox section, structure, roadmap) describe the
   parity contract, not the live surface. Parity is historical; 41 is live.

2. **Inbox reads: plane-P `fetch_inbox --since last` vs ADR-0041 `toron next`.**
   Both live, different layers. Agent-facing is the plane-P read: `toron mail
   inbox --since last` / MCP `fetch_inbox` (governor: toron skill rule 4 —
   server-side high-water, crash-proof default). `toron next` is the ADR-0041
   envelope reader verb (governor:
   `toron/docs/decisions/0041-comms-envelope-log-and-inbox-verbs.md` — commits
   cursor + Delivered in one transaction; `--peek` commits neither). Use the
   plane-P read for polling; use `next` when you need the envelope semantics.

3. **Wake: `bus/wake` + `wake_sock` are KEEP (live callers).** Live callers:
   `flywheel/src/doorbell_host.rs`, `flywheel/src/main.rs`,
   `toron/crates/toron-cli/src/{main,plane,wake_sock}.rs`,
   `toron/crates/toron-core/src/bus/wake.rs`, e2e `sc32_watch_once` (governor:
   the callers themselves). Decision: **KEEP** (`bd-wake-decommission-dsd8`,
   closed 2026-11-06); the stale "wave 3 deletes" marker is removed and the
   live contract is `watch --live` consuming the MCP serve's
   `start_wake_sidecar`. DECOMMISSION would require migrating both to the
   doorbell first. Never silently delete under a live caller.

4. **The embedded doorbell host types the ring; the no-text sink is gone.**
   `PlanePromptSink` resolves pin → pane through the spawn registry and
   prompts the selected plane (rex or hc). The old `NoTextSink` "rings,
   never types" contract silently shed every wake (live: `injects=0`), so a
   done-mail never reached an idle pane. Governor: the live wake proof
   (`bd-wave17-presence-producer-yjfol`, 2026-11-07) plus ADR-0010 D1 (the
   pane gets the `[doorbell]` line). The InboxHost remains the comms-ring
   path; if both fire, the ring line is constant per watermark.

5. **Backend default is rex-first, not hc.** Any prose naming hc as the default
   or only pane plane is stale (governor: toron ADR-0045 —
   `toron/docs/decisions/0045-rex-first-pane-plane-with-hc-transport-fallback.md`;
   resolver `toron_plane::backend::plane`). `auto` = rex when the CLI is present,
   hc otherwise, reason recorded; an explicit `rex` pin never falls back.

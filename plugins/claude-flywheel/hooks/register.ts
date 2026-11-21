// bd-9os20: first-party Claude Code module. Follows toron events through
// Claude Code's own MCP connection ($.mcp.call), so the plugin holds no token.
// One bus event -> one claim (bd-tdpix wake-rung) -> one delivery: prompt.submit
// for mail on an idle seat, session.append for urgent mail mid-turn. Wakes to
// subagent sessions are dropped. No keystrokes are ever sent to a pane.

import { claimEvent } from "../src/claim.ts";
import { decide, statusHealthy, MAX_FAILURES } from "../src/decide.ts";

const POLL_MS = 15_000;
const RUNG = process.env.FLYWHEEL_WAKE_RUNG_DIR || "";

function hint(id: string, urgent: boolean): string {
  return `[flywheel] ${urgent ? "Urgent mail" : "Mail"} event=${id}. Pull: toron next — run: toron next`;
}

export const register = (on) => {
  let failures = 0;
  let stopped = false;
  let clearTimer = null;
  let pin = "";

  const status = async ($, text) => {
    try {
      await $.ui.status(text);
    } catch {
      // status is advisory; a missing status surface never loses a wake
    }
  };

  async function poll($) {
    if (stopped || !pin || !RUNG) return;
    try {
      const frames =
        (await $.mcp.call({
          server: "toron",
          tool: "events/poll",
          arguments: { name: "mail.addressed", pin },
        })) || [];
      failures = 0;
      for (const frame of frames) {
        const id = String(frame.id ?? "");
        if (!id || !claimEvent(RUNG, id)) continue;
        const content = String(frame.content ?? "");
        const urgent =
          content.includes("plane-P urgent") ||
          content.includes("URGENT steer") ||
          content.includes("request-release");
        const idle =
          typeof $.session?.busy === "function" ? !$.session.busy() : true;
        const decision = decide({
          id,
          urgent,
          idle,
          subagent: Boolean(frame.subagent),
          claimed: true,
          failures,
        });
        if (decision.action === "submit") {
          await $.prompt.submit({ text: hint(id, urgent) });
        } else if (decision.action === "append") {
          await $.session.append({
            message: { type: "user", content: [{ type: "text", text: hint(id, urgent) }] },
          });
        }
        await status($, decision.status);
      }
    } catch {
      failures += 1;
      if (failures >= MAX_FAILURES) {
        stopped = true;
        if (clearTimer) clearTimer();
        await status($, `flywheel wake delivery stopped: ${failures} failed checks (check toron MCP)`);
      }
    }
  }

  on("session.start", async ($, event, next) => {
    pin = process.env.FLYWHEEL_PIN || "";
    failures = 0;
    stopped = false;
    await status($, statusHealthy());
    if (clearTimer) clearTimer();
    clearTimer = $.clock.every(POLL_MS, () => void poll($));
    return next(event);
  });

  // No delivery state lives here: the engine already drops a submit when the
  // operator typed first, and subagent wakes are dropped in decide().
  on("prompt.submit", async ($, event, next) => next(event));
  on("session.append", async ($, event, next) => next(event));
};

// bd-9os20/bd-gu288: first-party Claude Code module. The local
// `flywheel mcp-events` proxy owns the toron token and speaks the real
// events/* JSON-RPC methods; this module only reads its notification stream,
// claims each event, and starts a turn. No token, no typing, no body text.

import { claimEvent } from "../src/claim.ts";
import { decide } from "../src/decide.ts";
import { decodeFrame } from "../src/decode.ts";
import { wakeHint } from "../src/hint.ts";

const RUNG = process.env.FLYWHEEL_WAKE_RUNG_DIR || "";

export interface Piece {
  stream: string;
  text: string;
}
export type SpawnFn = (
  argv: readonly string[],
  env: Record<string, string | undefined>,
) => AsyncIterable<Piece>;

export interface RegisterDeps {
  spawn?: SpawnFn;
  flywheelBin?: string;
}

export const register = (on: any, deps: RegisterDeps = {}) => {
  let failures = 0;
  let stopped = false;
  let pin = "";
  let clearTimer: (() => void) | null = null;

  const status = async ($, text) => {
    try {
      await $.ui.status(text);
    } catch {
      // advisory status surface; never lose a wake because it is missing
    }
  };

  let startChild = (_$: any) => {};

  const consume = async ($, child: AsyncIterable<Piece>, subscriptionId: string) => {
    let buffer = "";
    try {
      for await (const piece of child) {
        if (piece.stream !== "stdout") continue;
        buffer += piece.text;
        let at: number;
        while ((at = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, at);
          buffer = buffer.slice(at + 1);
          const frame = decodeFrame(line, subscriptionId);
          if (!frame || !claimEvent(RUNG, frame.id)) continue;
          const idle = typeof $.session?.busy === "function" ? !$.session.busy() : true;
          const decision = decide({
            id: frame.id,
            urgent: frame.urgent,
            idle,
            subagent: false,
            claimed: true,
            failures,
          });
          const hint = wakeHint(frame, pin);
          if (decision.action === "submit") await $.prompt.submit({ text: hint });
          else if (decision.action === "append") {
            await $.session.append({
              message: { type: "user", content: [{ type: "text", text: hint }] },
            });
          }
          failures = 0;
          await status($, decision.status);
        }
      }
    } catch {
      failures += 1;
      if (failures >= 3) {
        stopped = true;
        if (clearTimer) clearTimer();
        await status($, "flywheel wake delivery stopped: 3 failed checks (check the toron connection)");
      } else if (!stopped) {
        // A dead proxy is one failed check, not the end of delivery: reconnect
        // and count again. Three consecutive failures stop with a status line.
        const retry = () => void startChild($);
        if (typeof $.clock?.after === "function") $.clock.after(0, retry);
        else setTimeout(retry, 0);
      }
    }
  };

  startChild = ($) => {
    const tokenPath = process.env.FLYWHEEL_TORON_TOKEN_FILE;
    const env = tokenPath ? { FLYWHEEL_TORON_TOKEN_FILE: tokenPath } : {};
    const argv: readonly string[] = [
      deps.flywheelBin ?? "flywheel",
      "mcp-events",
      "--subscription",
      pin,
    ];
    const child = deps.spawn
      ? deps.spawn(argv, env)
      : ($.process.spawn({ argv, env }) as AsyncIterable<Piece>);
    void consume($, child, pin);
  };

  on("session.start", async ($, event, next) => {
    pin = process.env.FLYWHEEL_PIN || "";
    failures = 0;
    stopped = false;
    if (stopped || !pin || !RUNG) return next(event);
    await status($, "flywheel wake delivery active");
    // The proxy echoes the pin as its subscription id (native_subscribe_result).
    startChild($);
    if (clearTimer) clearTimer();
    clearTimer = $.clock.every(15_000, () => void status($, "flywheel wake delivery active"));
    return next(event);
  });

  on("prompt.submit", async ($, event, next) => next(event));
  on("session.append", async ($, event, next) => next(event));
};

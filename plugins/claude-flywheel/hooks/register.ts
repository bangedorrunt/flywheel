// bd-9os20/bd-gu288/bd-krzqu: first-party Claude Code module. The local
// `flywheel mcp-events --follow` proxy owns the toron token, polls internally,
// and writes notifications/claude/channel frames to stdout. This module only
// reads them, claims each event, and starts a turn. No token, no typing, no
// body text, no stdin driving (the host's spawn input is a one-shot string).

import { claimEvent } from "../src/claim.ts";
import { decide } from "../src/decide.ts";
import { decodeFrame } from "../src/decode.ts";
import { wakeHint } from "../src/hint.ts";

const RUNG = process.env.FLYWHEEL_WAKE_RUNG_DIR || "";

export interface Piece {
  stream: string;
  text: string;
}
export interface Child {
  stdout: AsyncIterable<Piece>;
}
/** A fake may return just the stdout stream; the real spawn returns a Child. */
export type SpawnFn = (
  argv: readonly string[],
  env: Record<string, string | undefined>,
) => Child | AsyncIterable<Piece>;

export interface RegisterDeps {
  spawn?: SpawnFn;
  flywheelBin?: string;
}

const asChild = (c: Child | AsyncIterable<Piece>): Child =>
  Symbol.asyncIterator in c ? { stdout: c as AsyncIterable<Piece> } : c;

export const register = (on: any, deps: RegisterDeps = {}) => {
  let failures = 0;
  let stopped = false;
  let pin = "";
  let busy = false;
  let clearTimer: (() => void) | null = null;

  const status = async ($, text) => {
    try {
      await $.ui.status(text);
    } catch {
      // advisory status surface; never lose a wake because it is missing
    }
  };

  let startChild = (_$: any) => {};

  const fail = async ($: any) => {
    failures += 1;
    if (failures >= 3) {
      stopped = true;
      if (clearTimer) clearTimer();
      await status($, "flywheel wake delivery stopped: 3 failed checks (check the toron connection)");
    } else if (!stopped) {
      // A dead or silent proxy is one failed check, not the end of delivery:
      // reconnect and count again. Three consecutive failures stop with a line.
      const retry = () => void startChild($);
      if (typeof $.clock?.after === "function") $.clock.after(0, retry);
      else setTimeout(retry, 0);
    }
  };

  const consume = async ($, child: Child, subscriptionId: string) => {
    let buffer = "";
    try {
      for await (const piece of child.stdout) {
        if (piece.stream !== "stdout") continue;
        buffer += piece.text;
        let at: number;
        while ((at = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, at).trimEnd();
          buffer = buffer.slice(at + 1);
          if (!line) continue;
          const frame = decodeFrame(line, subscriptionId);
          if (!frame || !claimEvent(RUNG, frame.id)) continue;
          // bd-krzqu: the host has no session.busy. Track the turn ourselves
          // from the turn.start/complete/abort hooks registered below.
          const idle = !busy;
          const decision = decide({
            id: frame.id,
            urgent: frame.urgent,
            idle,
            subagent: false,
            claimed: true,
            failures,
          });
          // The proxy already rendered the hint in agent_text wording; the local
          // renderer is only a fallback if a frame omits content.
          const hint = frame.hint || wakeHint(frame, pin);
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
      // F11: a proxy that exits cleanly is a failed check, not success. Without
      // this, a closed stdout ends the stream normally and delivery stops silent.
      await fail($);
    } catch {
      await fail($);
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
      "--follow",
    ];
    const child = deps.spawn
      ? asChild(deps.spawn(argv, env))
      : asChild($.process.spawn({ argv, env }) as Child);
    void consume($, child, pin);
  };

  on("session.start", async ($, event, next) => {
    pin = process.env.FLYWHEEL_PIN || "";
    failures = 0;
    stopped = false;
    busy = false;
    if (stopped || !pin || !RUNG) return next(event);
    await status($, "flywheel wake delivery active");
    startChild($);
    if (clearTimer) clearTimer();
    clearTimer = $.clock.every(15_000, () => void status($, "flywheel wake delivery active"));
    return next(event);
  });

  // The host API has no session.busy; these three hooks are the turn state.
  on("turn.start", async (_$, event, next) => { busy = true; return next(event); });
  on("turn.complete", async (_$, event, next) => { busy = false; return next(event); });
  on("turn.abort", async (_$, event, next) => { busy = false; return next(event); });

  on("prompt.submit", async ($, event, next) => next(event));
  on("session.append", async ($, event, next) => next(event));
};

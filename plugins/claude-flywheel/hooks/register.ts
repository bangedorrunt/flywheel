// bd-9os20/bd-gu288/bd-krzqu: first-party Claude Code module. The local
// `flywheel mcp-events` proxy owns the toron token and speaks the real
// events/* JSON-RPC methods; this module drives its stdin with `events/poll`,
// reads the notifications it emits, claims each event, and starts a turn. No
// token, no typing, no body text.

import { claimEvent } from "../src/claim.ts";
import { decide } from "../src/decide.ts";
import { decodeFrame } from "../src/decode.ts";
import { wakeHint } from "../src/hint.ts";

const RUNG = process.env.FLYWHEEL_WAKE_RUNG_DIR || "";
const POLL_MS = 1000;

export interface Piece {
  stream: string;
  text: string;
}
export interface Child {
  stdout: AsyncIterable<Piece>;
  stdin?: { write(text: string): void };
}
/** A fake may return just the stdout stream; the real spawn returns a Child. */
export type SpawnFn = (
  argv: readonly string[],
  env: Record<string, string | undefined>,
  input?: AsyncIterable<string>,
) => Child | AsyncIterable<Piece>;

export interface RegisterDeps {
  spawn?: SpawnFn;
  flywheelBin?: string;
}

const asChild = (c: Child | AsyncIterable<Piece>): Child =>
  Symbol.asyncIterator in c ? { stdout: c as AsyncIterable<Piece> } : (c as Child);

/** A stdio poll stream that ends when its child does (bd-krzqu F3). */
function pollInput() {
  const queue: string[] = [];
  let done = false;
  let wake: (() => void) | null = null;
  const stream = (async function* (): AsyncGenerator<string> {
    while (!done) {
      while (queue.length) yield queue.shift()!;
      if (done) break;
      await new Promise<void>((r) => { wake = r; });
    }
  })();
  return {
    stream,
    poll: () => {
      queue.push(JSON.stringify({
        jsonrpc: "2.0", id: Date.now(), method: "events/poll", params: {},
      }) + "\n");
      wake?.();
      wake = null;
    },
    end: () => {
      done = true;
      wake?.();
      wake = null;
    },
  };
}

export const register = (on: any, deps: RegisterDeps = {}) => {
  let failures = 0;
  let stopped = false;
  let pin = "";
  let clearTimer: (() => void) | null = null;
  let stopChild: (() => void) | null = null;

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
    try {
      for await (const piece of child.stdout) {
        if (piece.stream !== "stdout") continue;
        const line = piece.text.trimEnd();
        if (!line) continue;
        const nl = line.indexOf("\n");
        const frame = decodeFrame(nl >= 0 ? line.slice(0, nl) : line, subscriptionId);
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
      // F11: a proxy that exits cleanly is a failed check, not success. Without
      // this, a closed stdin ends the stream normally and delivery stops silent.
      await fail($);
    } catch {
      await fail($);
    } finally {
      // This child is gone: close its poll stream so the stdin driver ends too.
      stopChild?.();
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
    stopChild?.();
    const polls = pollInput();
    const timer = setInterval(polls.poll, POLL_MS);
    stopChild = () => { clearInterval(timer); polls.end(); };
    polls.poll();
    const child = deps.spawn
      ? asChild(deps.spawn(argv, env, polls.stream))
      : asChild($.process.spawn({ argv, env, input: polls.stream }) as Child);
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

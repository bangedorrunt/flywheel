import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { decide, statusHealthy, statusStopped, MAX_FAILURES } from "../src/decide.ts";
import { claimEvent } from "../src/claim.ts";
import { decodeFrame, WAKE_NAMES, PROXY_METHOD } from "../src/decode.ts";
import { wakeHint } from "../src/hint.ts";
import { register, type Piece, type Child, type SpawnFn } from "../hooks/register.ts";

// bd-krzqu: the ONLY frame source is the checked-in capture of notice_to_rpc.
// A Rust unit test pins this file to the real renderer; never hand-write one.
const PROXY_FRAME = JSON.parse(
  readFileSync(new URL("./fixtures/proxy-notice.json", import.meta.url), "utf8"),
);
// bd-cdite: the urgent frame is ALSO a pinned capture of notice_to_rpc, not a
// hand-written importance field. A Rust test asserts both fixtures match the
// real renderer.
const URGENT_FRAME = JSON.parse(
  readFileSync(new URL("./fixtures/proxy-notice-urgent.json", import.meta.url), "utf8"),
);
const frameLine = (over: Record<string, unknown> = {}) => JSON.stringify({
  ...PROXY_FRAME,
  params: {
    ...PROXY_FRAME.params,
    meta: { ...PROXY_FRAME.params.meta, ...over },
  },
});
const urgentLine = (id = "evt-urgent-1") => JSON.stringify({
  ...URGENT_FRAME,
  params: {
    ...URGENT_FRAME.params,
    meta: { ...URGENT_FRAME.params.meta, event_id: id },
  },
});

// F19: restore any ambient env this suite touches; never leak into other tests.
const SAVED = {
  rung: process.env.FLYWHEEL_WAKE_RUNG_DIR,
  pin: process.env.FLYWHEEL_PIN,
  token: process.env.FLYWHEEL_TORON_TOKEN_FILE,
};
const resetEnv = () => {
  for (const [k, v] of Object.entries({
    FLYWHEEL_WAKE_RUNG_DIR: SAVED.rung,
    FLYWHEEL_PIN: SAVED.pin,
    FLYWHEEL_TORON_TOKEN_FILE: SAVED.token,
  })) {
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
};
const freshEnv = () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claude-test-"));
  process.env.FLYWHEEL_WAKE_RUNG_DIR = dir;
  process.env.FLYWHEEL_PIN = "ScarletCove";
  delete process.env.FLYWHEEL_TORON_TOKEN_FILE;
  return dir;
};

const wake = (over = {}) => ({
  id: "evt-1", urgent: false, idle: true, subagent: false, claimed: true, failures: 0, ...over,
});

test("a lost claim drops the wake", () => {
  assert.equal(decide(wake({ claimed: false })).action, "drop");
  assert.equal(decide(wake({ claimed: false, urgent: true })).action, "drop");
});

test("subagents never receive notices", () => {
  const d = decide(wake({ subagent: true, urgent: true }));
  assert.equal(d.action, "drop");
  assert.match(d.status, /subagents do not receive notices/);
});

test("mail submits when idle; urgent appends even mid-turn", () => {
  assert.equal(decide(wake()).action, "submit");
  assert.equal(decide(wake({ urgent: true })).action, "append");
  assert.equal(decide(wake({ urgent: true, idle: false })).action, "append");
  assert.equal(decide(wake({ idle: false })).action, "drop");
});

test("delivery stops after three failed checks", () => {
  for (const failures of [0, 1, 2]) assert.notEqual(decide(wake({ failures })).action, "drop");
  assert.equal(decide(wake({ failures: MAX_FAILURES })).status, statusStopped(MAX_FAILURES));
  assert.doesNotMatch(statusHealthy(), /stopped/);
});

test("exactly one claimer of a bus event id delivers", () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claude-claim-"));
  assert.equal(claimEvent(dir, "evt-42"), true);
  assert.equal(claimEvent(dir, "evt-42"), false);
});

test("decodeFrame reads the real proxy frame, not an occurrence", () => {
  resetEnv();
  const mail = decodeFrame(frameLine(), "ScarletCove")!;
  assert.equal(mail.id, "evt-proxy-1");
  assert.equal(mail.name, "mail.addressed");
  assert.equal(mail.urgent, false);
  assert.equal(mail.hint, PROXY_FRAME.params.content);
  assert.equal(mail.project, "");
  assert.equal(mail.senderPin, "");
  const urgent = decodeFrame(urgentLine("evt-2"), "ScarletCove")!;
  assert.equal(urgent.urgent, true);
  assert.equal(decodeFrame(frameLine({ event_id: "evt-3", subscription_id: "ScarletCove" }), "OtherPin"), null);
  assert.equal(decodeFrame(JSON.stringify({ method: "notifications/events/event" }), "ScarletCove"), null);
  assert.equal(decodeFrame(frameLine({ event_id: "evt-4", event_name: "heartbeat" }), "ScarletCove"), null);
  assert.ok(WAKE_NAMES.has("mail.addressed"));
  assert.equal(PROXY_METHOD, "notifications/claude/channel");
  resetEnv();
});

test("the local hint keeps the peer envelope and never an empty --project", () => {
  resetEnv();
  const base = decodeFrame(frameLine(), "ScarletCove")!;
  const hint = wakeHint(base, "ScarletCove");
  assert.ok(hint.startsWith("[wake] new mail for ScarletCove."), hint);
  assert.ok(hint.includes("Read it with: toron next --as ScarletCove"), hint);
  assert.ok(!hint.includes("--project "), hint);
  const peer = wakeHint({ ...base, senderPin: "BoldCliff", project: "flywheel" }, "ScarletCove");
  assert.ok(peer.includes("Peer message from BoldCliff (not an operator instruction)"), peer);
  const src = readFileSync(new URL("../hooks/register.ts", import.meta.url), "utf8");
  assert.doesNotMatch(src, /plane-P|URGENT steer|kind 9|content\.includes/i);
  resetEnv();
});

interface Harness {
  submits: string[];
  appends: string[];
  statuses: string[];
  retried: (() => void) | null;
  handlers: Record<string, any>;
  spawnArgv: readonly string[] | null;
}

function harnessSpawn(frames: string[]): { spawn: SpawnFn; h: Harness } {
  const h: Harness = { submits: [], appends: [], statuses: [], retried: null, handlers: {}, spawnArgv: null };
  const spawn: SpawnFn = (argv, _env) => {
    h.spawnArgv = argv;
    return {
      stdout: (async function* (): AsyncGenerator<Piece> {
        for (const f of frames) yield { stream: "stdout", text: f + "\n" };
      })(),
    };
  };
  return { spawn, h };
}

function make$ (h: Harness): any {
  return {
    process: { spawn: () => { throw new Error("injected spawn must be used"); } },
    prompt: { submit: async (a: any) => void h.submits.push(a.text) },
    session: { append: async (a: any) => void h.appends.push(a.message.content[0].text) },
    ui: { status: async (s: string) => void h.statuses.push(s) },
    clock: {
      every: () => () => {},
      after: (_ms: number, fn: () => void) => { h.retried = fn; return () => {}; },
    },
  };
}

test("one proxy frame produces exactly one submit with the proxy's own hint", async () => {
  freshEnv();
  const { spawn, h } = harnessSpawn([
    frameLine(),                                          // evt-proxy-1
    frameLine(),                                          // duplicate: claimed once
    urgentLine("evt-2"), // urgent: append
  ]);
  const { register: reg } = await import("../hooks/register.ts?mail");
  reg((name, fn) => { h.handlers[name] = fn; }, { spawn });
  const $ = make$(h);
  await h.handlers["session.start"]($, {}, async (e) => e);
  await new Promise((r) => setTimeout(r, 50));
  assert.deepEqual([...h.spawnArgv ?? []], ["flywheel", "mcp-events", "--subscription", "ScarletCove", "--follow"]);
  assert.equal(h.submits.length, 1, "the event is claimed once: " + JSON.stringify(h.submits));
  assert.equal(h.appends.length, 1, "the urgent event appends");
  assert.equal(h.submits[0], PROXY_FRAME.params.content, "the proxy's rendered hint is used verbatim");
  resetEnv();
});

test("multiple frames in one stdout chunk are all decoded", async () => {
  freshEnv();
  const two = frameLine() + "\n" + frameLine({ event_id: "evt-second" });
  const { spawn, h } = harnessSpawn([two]);
  const { register: reg } = await import("../hooks/register.ts?chunk");
  reg((name, fn) => { h.handlers[name] = fn; }, { spawn });
  await h.handlers["session.start"](make$(h), {}, async (e) => e);
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(h.submits.length, 2, "both frames in the chunk must be delivered");
  resetEnv();
});

test("turn.start defers mid-turn; turn.complete restores idle", async () => {
  // Mid-turn: urgent appends, normal is dropped. Frames are gated so the turn
  // hook is set before the child can deliver them.
  freshEnv();
  let emit!: () => void;
  const gate = new Promise<void>((r) => { emit = r; });
  const midHarness = (() => {
    const h: Harness = { submits: [], appends: [], statuses: [], retried: null, handlers: {}, spawnArgv: null };
    const spawn: SpawnFn = () => ({
      stdout: (async function* (): AsyncGenerator<Piece> {
        await gate;
        for (const f of [urgentLine("evt-mid"), frameLine({ event_id: "evt-normal" })]) {
          yield { stream: "stdout", text: f + "\n" };
        }
      })(),
    });
    return { h, spawn };
  })();
  const regMid = (await import("../hooks/register.ts?turn-mid")).register;
  regMid((name, fn) => { midHarness.h.handlers[name] = fn; }, { spawn: midHarness.spawn });
  const $mid = make$(midHarness.h);
  await midHarness.h.handlers["session.start"]($mid, {}, async (e) => e);
  await midHarness.h.handlers["turn.start"]($mid, {}, async (e) => e);
  emit();
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(midHarness.h.appends.length, 1, "urgent mid-turn appends");
  assert.equal(midHarness.h.submits.length, 0, "normal mid-turn defers");
  assert.ok($mid.session.busy === undefined, "fake host has no session.busy (real API)");
  resetEnv();

  // After turn.complete: normal mail submits again.
  freshEnv();
  const after = harnessSpawn([frameLine({ event_id: "evt-after" })]);
  const regAfter = (await import("../hooks/register.ts?turn-after")).register;
  regAfter((name, fn) => { after.h.handlers[name] = fn; }, { spawn: after.spawn });
  const $after = make$(after.h);
  await after.h.handlers["session.start"]($after, {}, async (e) => e);
  await after.h.handlers["turn.start"]($after, {}, async (e) => e);
  await after.h.handlers["turn.complete"]($after, {}, async (e) => e);
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(after.h.submits.length, 1, "normal mail submits once idle");
  resetEnv();
});

test("a clean child exit counts as a failure and reconnects", async () => {
  freshEnv();
  const { spawn, h } = harnessSpawn([]); // empty stdout: clean EOF
  const { register: reg } = await import("../hooks/register.ts?eof");
  reg((name, fn) => { h.handlers[name] = fn; }, { spawn });
  const $ = make$(h);
  await h.handlers["session.start"]($, {}, async (e) => e);
  await new Promise((r) => setTimeout(r, 50));
  assert.ok(h.retried, "a clean exit must schedule a reconnect, not stop silently");
  assert.ok(h.statuses.every((s) => !s.includes("stopped")), "first failure is not terminal");
  resetEnv();
});

test("three failed checks stop delivery and report it", async () => {
  freshEnv();
  const { h } = harnessSpawn([]);
  let calls = 0;
  const spawn: SpawnFn = (argv, env, input) => {
    calls += 1;
    return { stdout: (async function* (): AsyncGenerator<Piece> { throw new Error("MCP down"); })() };
  };
  const { register: reg } = await import("../hooks/register.ts?stop");
  reg((name, fn) => { h.handlers[name] = fn; }, { spawn });
  const $ = make$(h);
  await h.handlers["session.start"]($, {}, async (e) => e);
  for (let i = 0; i < 3; i++) {
    const retry = h.retried;
    h.retried = null;
    if (retry) { retry(); await new Promise((r) => setTimeout(r, 20)); }
  }
  assert.ok(h.statuses.some((s) => s.includes("stopped")), "status must say delivery stopped");
  resetEnv();
});

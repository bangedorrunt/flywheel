import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { decide, statusHealthy, statusStopped, MAX_FAILURES } from "../src/decide.ts";
import { claimEvent } from "../src/claim.ts";
import { decodeFrame, WAKE_NAMES } from "../src/decode.ts";
import { wakeHint } from "../src/hint.ts";
import { register, type Piece, type SpawnFn } from "../hooks/register.ts";

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

function occurrence(eventId: string, name: string, urgent = false, project = "flywheel") {
  return JSON.stringify({
    jsonrpc: "2.0",
    method: "notifications/events/event",
    params: {
      _meta: { "io.modelcontextprotocol/subscriptionId": "ScarletCove" },
      eventId,
      name,
      timestamp: "2026-11-21T00:00:00.000Z",
      data: { project, senderPin: "BoldCliff", correlationId: "c-1", ...(urgent ? { importance: "urgent" } : {}) },
      cursor: "7",
    },
  });
}

test("decodeFrame reads the real occurrence shape, not body text", () => {
  const mail = decodeFrame(occurrence("evt-1", "mail.addressed"), "ScarletCove")!;
  assert.equal(mail.id, "evt-1");
  assert.equal(mail.name, "mail.addressed");
  assert.equal(mail.project, "flywheel");
  assert.equal(mail.senderPin, "BoldCliff");
  assert.equal(mail.urgent, false);
  const urgent = decodeFrame(occurrence("evt-2", "mail.addressed", true), "ScarletCove")!;
  assert.equal(urgent.urgent, true);
  assert.equal(decodeFrame(occurrence("evt-3", "mail.addressed"), "OtherPin"), null);
  assert.equal(decodeFrame(occurrence("evt-4", "heartbeat"), "ScarletCove"), null);
  assert.ok(WAKE_NAMES.has("mail.addressed"));
});

test("the hint is plain English in agent_text wording", () => {
  const frame = decodeFrame(occurrence("evt-1", "mail.addressed"), "ScarletCove")!;
  const hint = wakeHint(frame, "ScarletCove");
  assert.ok(hint.startsWith("[wake] new mail for ScarletCove in project flywheel."), hint);
  assert.ok(hint.includes("Read it with: toron next --project flywheel --as ScarletCove"), hint);
  const src = readFileSync(new URL("../hooks/register.ts", import.meta.url), "utf8");
  assert.doesNotMatch(src, /plane-P|URGENT steer|kind 9|content\.includes/i);
});

test("one mail.addressed produces exactly one submit with the agent_text hint", async () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claude-rung-"));
  process.env.FLYWHEEL_WAKE_RUNG_DIR = dir;
  process.env.FLYWHEEL_PIN = "ScarletCove";
  const lines = [
    occurrence("evt-1", "mail.addressed"),
    occurrence("evt-1", "mail.addressed"),
    occurrence("evt-2", "mail.addressed", true),
  ];
  const spawn: SpawnFn = (argv, env) => {
    assert.deepEqual([...argv], ["flywheel", "mcp-events", "--subscription", "ScarletCove"]);
    assert.deepEqual(env, {}); // no token path -> proxy uses the seat's own env
    return (async function* (): AsyncGenerator<Piece> {
      for (const line of lines) yield { stream: "stdout", text: line + "\n" };
    })();
  };
  const { register: reg } = await import("../hooks/register.ts?mail");
  const handlers: Record<string, any> = {};
  reg((name, fn) => { handlers[name] = fn; }, { spawn });
  const submits: string[] = [];
  const appends: string[] = [];
  const $: any = {
    process: { spawn: () => { throw new Error("injected spawn must be used"); } },
    prompt: { submit: async (a) => void submits.push(a.text) },
    session: { append: async (a) => void appends.push(a.message.content[0].text), busy: () => false },
    ui: { status: async () => {} },
    clock: { every: () => () => {}, after: (_ms, fn) => { fn(); return () => {}; } },
  };
  await handlers["session.start"]($, {}, async (e) => e);
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(submits.length, 1, "evt-1 is claimed once: " + JSON.stringify(submits));
  assert.equal(appends.length, 1, "the urgent event appends");
  assert.ok(submits[0].startsWith("[wake] new mail for ScarletCove in project flywheel."));
  assert.ok(appends[0].includes("Read it with: toron next --project flywheel --as ScarletCove"));
});

test("three failed MCP checks stop delivery and report it", async () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claude-stop-"));
  process.env.FLYWHEEL_WAKE_RUNG_DIR = dir;
  process.env.FLYWHEEL_PIN = "ScarletCove";
  const spawn: SpawnFn = () => (async function* (): AsyncGenerator<Piece> {
    throw new Error("MCP down");
  })();
  const statuses: string[] = [];
  const $: any = {
    process: { spawn: () => { throw new Error("unused"); } },
    prompt: { submit: async () => {} },
    session: { append: async () => {}, busy: () => false },
    ui: { status: async (s) => void statuses.push(s) },
    clock: { every: () => () => {}, after: (_ms, fn) => { fn(); return () => {}; } },
  };
  const handlers: Record<string, any> = {};
  const { register: reg } = await import("../hooks/register.ts?stop");
  reg((name, fn) => { handlers[name] = fn; }, { spawn });
  await handlers["session.start"]($, {}, async (e) => e);
  await new Promise((r) => setTimeout(r, 50));
  assert.ok(statuses.some((s) => s.includes("stopped")), "status must say delivery stopped");
});

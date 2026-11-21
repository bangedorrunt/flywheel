import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { decide, statusHealthy, statusStopped, MAX_FAILURES } from "../src/decide.ts";
import { claimEvent } from "../src/claim.ts";

const wake = (over = {}) => ({
  id: "bus-1",
  urgent: false,
  idle: true,
  subagent: false,
  claimed: true,
  failures: 0,
  ...over,
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
  // normal mail while busy is left to the doorbell queue
  assert.equal(decide(wake({ idle: false })).action, "drop");
});

test("delivery stops after three failed checks", () => {
  for (const failures of [0, 1, 2]) {
    assert.notEqual(decide(wake({ failures })).action, "drop");
  }
  const stopped = decide(wake({ failures: MAX_FAILURES }));
  assert.equal(stopped.action, "drop");
  assert.equal(stopped.status, statusStopped(MAX_FAILURES));
  assert.doesNotMatch(statusHealthy(), /stopped/);
});

test("exactly one claimer of a bus event id delivers", () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claude-claim-"));
  assert.equal(claimEvent(dir, "bus-42"), true);
  assert.equal(claimEvent(dir, "bus-42"), false);
  assert.equal(claimEvent(dir, "bus-43"), true);
});

test("the module turns one claimed event into one submit and never reads a token", async () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claude-rung-"));
  process.env.FLYWHEEL_WAKE_RUNG_DIR = dir;
  process.env.FLYWHEEL_PIN = "ScarletCove";
  const source = readFileSync(new URL("../hooks/register.ts", import.meta.url), "utf8");
  assert.doesNotMatch(source, /FLYWHEEL_TORON_TOKEN_FILE|authorization/i);
  const origRead = process.env.FLYWHEEL_TORON_TOKEN_FILE;
  process.env.FLYWHEEL_TORON_TOKEN_FILE = "/must/not/be/read";

  const { register } = await import("../hooks/register.ts");
  const handlers: Record<string, any> = {};
  register((name, fn) => {
    handlers[name] = fn;
  });

  const polls = [
    [{ id: "evt-1", content: "addressed mail" }],
    [{ id: "evt-1", content: "addressed mail" }, { id: "evt-2", content: "URGENT steer" }],
  ];
  const submits: string[] = [];
  const appends: string[] = [];
  const statuses: string[] = [];
  let tick = () => {};
  const $: any = {
    mcp: { call: async () => polls.shift() || [] },
    prompt: { submit: async (a) => void submits.push(a.text) },
    session: { append: async (a) => void appends.push(a.message.content[0].text), busy: () => false },
    ui: { status: async (s) => void statuses.push(s) },
    clock: { every: (_ms, fn) => { tick = fn; return () => { tick = () => {}; }; } },
  };

  await handlers["session.start"]($, {}, async (e) => e);
  await tick();
  await tick();

  assert.equal(submits.length, 1, "evt-1 submits once even when it appears twice");
  assert.equal(appends.length, 1, "the urgent event appends");
  assert.match(appends[0], /event=evt-2/);
  assert.equal(process.env.FLYWHEEL_TORON_TOKEN_FILE, "/must/not/be/read");
  if (origRead === undefined) delete process.env.FLYWHEEL_TORON_TOKEN_FILE;
  else process.env.FLYWHEEL_TORON_TOKEN_FILE = origRead;
});

test("three failed MCP checks stop delivery and report it", async () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claude-stop-"));
  process.env.FLYWHEEL_WAKE_RUNG_DIR = dir;
  const { register } = await import("../hooks/register.ts?stop");
  const handlers: Record<string, any> = {};
  register((name, fn) => { handlers[name] = fn; });
  let tick = () => {};
  const statuses: string[] = [];
  const $: any = {
    mcp: { call: async () => { throw new Error("MCP down"); } },
    prompt: { submit: async () => {} },
    session: { append: async () => {}, busy: () => false },
    ui: { status: async (s) => void statuses.push(s) },
    clock: { every: (_ms, fn) => { tick = fn; return () => { tick = () => {}; }; } },
  };
  await handlers["session.start"]($, {}, async (e) => e);
  for (let i = 0; i < MAX_FAILURES; i++) await tick();
  assert.ok(statuses.some((s) => s.includes("stopped")), "status must say delivery stopped");
});

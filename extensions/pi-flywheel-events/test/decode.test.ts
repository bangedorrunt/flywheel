import test from "node:test";
import assert from "node:assert/strict";
import { HINT, coalesce, decodeFrame } from "../src/decode.ts";

const SUB = "sub-1";
const frame = (name: string, extra: object = {}) => JSON.stringify({
  method: "notifications/events/event",
  params: {
    _meta: { "io.modelcontextprotocol/subscriptionId": SUB },
    eventId: `${name}-id`, name, timestamp: "2026-07-28T00:00:00Z",
    data: { body: "SECRET MAIL BODY", ...extra },
  },
});

test("a wake event decodes to id/name only, never the body", () => {
  const f = decodeFrame(frame("mail.addressed"), SUB);
  assert.deepEqual(f, { id: "mail.addressed-id", name: "mail.addressed", urgent: false });
  assert.ok(!JSON.stringify(f).includes("SECRET"));
});

test("frames for another subscription and non-wake kinds are ignored", () => {
  assert.equal(decodeFrame(frame("mail.addressed"), "other"), null);
  assert.equal(decodeFrame(frame("heartbeat"), SUB), null);
  assert.equal(decodeFrame("not json", SUB), null);
});

test("urgent mail and ack-sla steer; normal mail follows up", () => {
  assert.equal(coalesce([decodeFrame(frame("mail.addressed"), SUB)!])!.deliverAs, "followUp");
  assert.equal(
    coalesce([decodeFrame(frame("mail.addressed", { priority: "urgent" }), SUB)!])!.deliverAs,
    "steer",
  );
  assert.equal(coalesce([decodeFrame(frame("ack_sla_expired"), SUB)!])!.deliverAs, "steer");
});

test("a burst coalesces to one hint carrying count and first id", () => {
  const frames = [
    decodeFrame(frame("mail.addressed"), SUB)!,
    decodeFrame(frame("role.changed"), SUB)!,
  ];
  const notice = coalesce(frames)!;
  assert.equal(notice.hint, HINT);
  assert.equal(notice.count, 2);
  assert.equal(notice.firstId, "mail.addressed-id");
  assert.equal(notice.name, "mixed");
  assert.ok(!JSON.stringify(notice).includes("SECRET"));
});

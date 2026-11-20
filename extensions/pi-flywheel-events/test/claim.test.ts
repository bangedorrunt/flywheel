import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, existsSync, statSync, utimesSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { claimEvent, markerPath, sweepRung } from "../src/claim.ts";

test("exactly one claimer wins for a bus event id", () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claim-"));
  assert.equal(claimEvent(dir, "bus-event-42"), true);
  assert.equal(claimEvent(dir, "bus-event-42"), false);
  assert.equal(claimEvent(dir, "bus-event-43"), true);
  assert.ok(existsSync(markerPath(dir, "bus-event-42")));
});

test("marker names are sanitized and bounded like the Rust side", () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claim-"));
  const p = markerPath(dir, "a/b:c-1_2");
  assert.ok(p.endsWith("a_b_c-1_2.rung"));
});

test("a bounded age sweep removes only old markers", () => {
  const dir = mkdtempSync(join(tmpdir(), "fw-claim-"));
  claimEvent(dir, "old");
  claimEvent(dir, "fresh");
  const old = markerPath(dir, "old");
  const past = new Date(Date.now() - 7200_000);
  utimesSync(old, past, past);
  const removed = sweepRung(dir, 3600, Math.floor(Date.now() / 1000));
  assert.equal(removed, 1);
  assert.ok(!existsSync(old));
  assert.ok(existsSync(markerPath(dir, "fresh")));
});

import test from "node:test";
import assert from "node:assert/strict";
import { Deduper, unpulled } from "../src/dedupe.ts";

test("a repeated bus event id yields one hint", () => {
  const d = new Deduper(8);
  assert.equal(d.isNew("evt-1"), true);
  assert.equal(d.isNew("evt-1"), false);
  assert.equal(d.isNew("evt-2"), true);
});

test("the ring set from the doorbell suppresses a bus event id", () => {
  const frames = [{ id: "a" }, { id: "b" }, { id: "c" }];
  assert.deepEqual(unpulled(frames, new Set(["b"])), ["a", "c"]);
  assert.deepEqual(unpulled(frames, new Set(["a", "b", "c"])), []);
});

test("the deduper evicts oldest at capacity, never the whole set", () => {
  const d = new Deduper(2);
  d.isNew("a"); d.isNew("b"); d.isNew("c");
  assert.equal(d.has("a"), false);
  assert.equal(d.has("b"), true);
  assert.equal(d.has("c"), true);
});

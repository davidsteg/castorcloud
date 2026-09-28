import assert from "node:assert/strict";
import { test } from "node:test";
import { parseStatus, parseUsage } from "./storage.ts";
import { datasetKey } from "./zfs.ts";

test("parses helper status lines", () => {
  assert.deepEqual(parseStatus("open 120 102400 102400\n"), { state: "open", usedMb: 120, sizeMb: 102400, volumeMb: 102400 });
  assert.deepEqual(parseStatus("closed"), { state: "closed" });
  assert.deepEqual(parseStatus("missing"), { state: "missing" });
});

test("parses usage lines and ignores anything else", () => {
  assert.deepEqual(parseUsage("photos 12\ndb 3\nclosed\n\n"), { photos: 12, db: 3 });
});

test("dataset keys are 32 bytes as hex, deterministic per volume key", () => {
  const vk = Buffer.alloc(64, 7);
  const k = datasetKey(vk).toString();
  assert.match(k, /^[0-9a-f]{64}\n$/);
  assert.equal(datasetKey(vk).toString(), k);
  assert.notEqual(datasetKey(Buffer.alloc(64, 8)).toString(), k);
});

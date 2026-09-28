import assert from "node:assert/strict";
import { test } from "node:test";
import { nodeLayout, parseEdge } from "./layout.ts";

test("TrueNAS keeps everything under <pool>/castor, macOS uses its own places", () => {
  assert.deepEqual(nodeLayout("TRUENAS", "tank"), { root: "/mnt/tank/castor/node", mountRoot: "/mnt/tank/castor/t" });
  assert.deepEqual(nodeLayout("MAC", "castor"), { root: "/Users/Shared/castor", mountRoot: "/Volumes/castor/t" });
});

test("edge host with and without a port", () => {
  assert.deepEqual(parseEdge("10.0.0.5:8480"), { host: "10.0.0.5", port: 8480, url: "http://10.0.0.5:8480" });
  assert.deepEqual(parseEdge("10.0.0.31"), { host: "10.0.0.31", port: 80, url: "http://10.0.0.31" });
  assert.throws(() => parseEdge("10.0.0.5:99999"));
  assert.throws(() => parseEdge("http://x"));
});

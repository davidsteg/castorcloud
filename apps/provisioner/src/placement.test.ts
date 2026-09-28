import assert from "node:assert/strict";
import { test } from "node:test";
import { pickNode, unfit, type PlacementNode } from "./placement.ts";

const now = new Date("2026-09-28T12:00:00Z");
const GB = 1024;

function mac(name: string, over: Partial<PlacementNode> = {}): PlacementNode {
  return {
    id: name, name, kind: "MAC", enabled: true, capacity: 20,
    poolSizeMb: 2000 * GB, poolUsedMb: 100 * GB, poolHealth: "ONLINE",
    memoryMb: 64 * GB, lastSeenAt: new Date(now.getTime() - 60_000), tenants: [],
    ...over,
  };
}

test("picks the node with the most free booked space", () => {
  const a = mac("a", { tenants: [{ storageUnits: 10 }] });
  const b = mac("b", { tenants: [{ storageUnits: 2 }, { storageUnits: 2 }] });
  const res = pickNode([a, b], 1, now);
  assert.equal(res.node?.name, "b");
});

test("prefers Mac nodes over the local development host", () => {
  const local = mac("local", { kind: "LOCAL", poolSizeMb: null, poolUsedMb: null, poolHealth: null, lastSeenAt: null });
  assert.equal(pickNode([local, mac("a")], 1, now).node?.name, "a");
  assert.equal(pickNode([local], 1, now).node?.name, "local");
});

test("skips draining, stale, degraded, full and fully booked nodes, with reasons", () => {
  const nodes = [
    mac("draining", { enabled: false }),
    mac("stale", { lastSeenAt: new Date(now.getTime() - 60 * 60_000) }),
    mac("degraded", { poolHealth: "DEGRADED" }),
    mac("full", { poolUsedMb: 1700 * GB }),
    mac("booked", { poolSizeMb: 1000 * GB, tenants: Array.from({ length: 19 }, () => ({ storageUnits: 1 })) }),
    mac("capacity", { capacity: 1, tenants: [{ storageUnits: 1 }] }),
  ];
  const res = pickNode(nodes, 2, now);
  assert.equal(res.node, null);
  assert.deepEqual("reasons" in res ? res.reasons : [], [
    "draining: draining",
    "stale: not seen recently",
    "degraded: pool DEGRADED",
    "full: pool almost full",
    "booked: pool fully booked",
    "capacity: at capacity",
  ]);
});

test("memory limits the number of stacks", () => {
  // 16 GB: 8 reserved leaves room for two 3 GB stacks
  assert.equal(unfit(mac("m", { memoryMb: 16 * GB, tenants: [{ storageUnits: 1 }] }), 1, now), null);
  assert.equal(unfit(mac("m", { memoryMb: 16 * GB, tenants: [{ storageUnits: 1 }, { storageUnits: 1 }] }), 1, now), "not enough memory");
});

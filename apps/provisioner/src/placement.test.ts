import assert from "node:assert/strict";
import { test } from "node:test";
import { pickNode, pickStacksNode, unfit, unfitForStacks, type PlacementNode, type StacksPlacementNode } from "./placement.ts";

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

test("tenants never go to a stacks node", () => {
  assert.equal(unfit(mac("s", { role: "STACKS" }), 1, now), "stacks node");
  assert.equal(pickNode([mac("s", { role: "STACKS" }), mac("a")], 1, now).node?.name, "a");
});

function stacksNode(name: string, over: Partial<StacksPlacementNode> = {}): StacksPlacementNode {
  return {
    id: name, name, role: "STACKS", enabled: true, poolSizeMb: 1000 * GB, poolUsedMb: 10 * GB, poolHealth: "ONLINE",
    memoryMb: 16 * GB, uidBase: 100000, lastSeenAt: new Date(now.getTime() - 60_000), stackTenants: [], ...over,
  };
}

test("stacks go to the stacks node with the most free memory, never overcommitted", () => {
  const need = { memoryUnits: 8, cpuUnits: 2, storageUnits: 1 }; // 2 GB
  const busy = stacksNode("busy", { stackTenants: [{ stackMemoryUnits: 40, stackStorageUnits: 0 }] }); // 10 GB used
  const idle = stacksNode("idle");
  assert.equal(pickStacksNode([busy, idle, stacksNode("tenants", { role: "TENANTS" })], need, now).node?.name, "idle");
  const full = stacksNode("full", { stackTenants: [{ stackMemoryUnits: 48, stackStorageUnits: 0 }] }); // 12 + 2 + 2 reserved > 16
  assert.equal(unfitForStacks(full, { ...need, memoryUnits: 9 }, now), "not enough memory");
  assert.equal(unfitForStacks(stacksNode("x", { uidBase: null }), need, now), "no userns-remap");
  assert.equal(unfitForStacks(stacksNode("t", { role: "TENANTS" }), need, now), "not a stacks node");
});

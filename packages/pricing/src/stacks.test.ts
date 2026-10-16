import assert from "node:assert/strict";
import { test } from "node:test";
import { addUnits, fitsUnits, stackUnits } from "./stacks.ts";

test("stack units round up per resource", () => {
  assert.deepEqual(stackUnits({ memoryMb: 300, cpus: 0.3, storageGb: 1 }), { memoryUnits: 2, cpuUnits: 2, storageUnits: 1 });
  assert.deepEqual(stackUnits({ memoryMb: 512, cpus: 0.1 + 0.2 + 0.2, storageGb: 20 }), { memoryUnits: 2, cpuUnits: 2, storageUnits: 2 });
  assert.deepEqual(stackUnits({ memoryMb: 0, cpus: 0, storageGb: 0 }), { memoryUnits: 0, cpuUnits: 0, storageUnits: 0 });
});

test("fitsUnits compares every resource", () => {
  const booked = { memoryUnits: 4, cpuUnits: 2, storageUnits: 1 };
  assert.ok(fitsUnits(booked, booked));
  assert.ok(!fitsUnits(addUnits(booked, { memoryUnits: 0, cpuUnits: 1, storageUnits: 0 }), booked));
});

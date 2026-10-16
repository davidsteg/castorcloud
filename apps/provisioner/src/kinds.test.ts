import assert from "node:assert/strict";
import { test } from "node:test";
import { workerKinds, workerTasks } from "./kinds.ts";

test("the TypeScript worker leaves castord's kinds alone", () => {
  assert.deepEqual(workerKinds(["PROVISION", "SUSPEND", "DELETE"], undefined), ["PROVISION", "SUSPEND", "DELETE"]);
  assert.deepEqual(workerKinds(["PROVISION", "SUSPEND", "DELETE"], " SUSPEND , DELETE"), ["PROVISION"]);
  assert.throws(() => workerKinds(["PROVISION", "SUSPEND"], "SUSPENDED"), /unknown job kind SUSPENDED/);
  assert.deepEqual(workerKinds(["SUSPEND"], "SUSPEND"), []);
});

test("and castord's tasks", () => {
  assert.deepEqual(workerTasks(["watch-nodes", "measure-storage"], "watch-nodes"), ["measure-storage"]);
  assert.throws(() => workerTasks(["watch-nodes"], "watch-node"), /CASTORD_TASKS: unknown task watch-node/);
});

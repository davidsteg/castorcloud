import assert from "node:assert/strict";
import { test } from "node:test";
import { workerKinds } from "./kinds.ts";

test("the TypeScript worker leaves castord's kinds alone", () => {
  assert.deepEqual(workerKinds(["PROVISION", "SUSPEND", "DELETE"], undefined), ["PROVISION", "SUSPEND", "DELETE"]);
  assert.deepEqual(workerKinds(["PROVISION", "SUSPEND", "DELETE"], " SUSPEND , DELETE"), ["PROVISION"]);
  assert.throws(() => workerKinds(["PROVISION", "SUSPEND"], "SUSPENDED"), /unknown job kind SUSPENDED/);
  assert.throws(() => workerKinds(["SUSPEND"], "SUSPEND"), /nothing left/);
});

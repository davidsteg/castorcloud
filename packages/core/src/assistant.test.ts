import assert from "node:assert/strict";
import { test } from "node:test";
import { chatEvents, newPairingCode, pairingCodeHash, toolService, translate } from "./assistant.ts";

const sse = (text: string) => new Blob([text]).stream();

test("tool names map to services", () => {
  assert.equal(toolService("mcp__docs__search_documents"), "docs");
  assert.equal(toolService("memory"), "memory");
});

test("a used-up budget is reported as a credit error", () => {
  assert.deepEqual(translate("assistant.completed", { content: "Provider said: HTTP 429: Budget has been exceeded! Current cost: 1" }), { type: "error", code: "credit" });
  assert.deepEqual(translate("run.failed", { error: "boom" }), { type: "error", code: "failed" });
});

test("the SSE stream becomes deltas, tools and one final answer", async () => {
  const body = sse(
    'event: run.started\ndata: {}\n\n' +
    'event: tool.started\ndata: {"tool_name": "mcp__flights__list_flights", "args": {"airport": "LIS"}}\n\n' +
    'event: assistant.delta\ndata: {"delta": "You flew"}\n\n' +
    'event: assistant.completed\ndata: {"content": "You flew to Lisbon."}\n\n' +
    'event: run.completed\ndata: {}\n\n',
  );
  const out = [];
  for await (const e of chatEvents(body)) out.push(e);
  assert.deepEqual(out, [
    { type: "tool", service: "flights" },
    { type: "delta", text: "You flew" },
    { type: "done", text: "You flew to Lisbon." },
  ]);
});

test("a stream that ends without an answer is an error", async () => {
  const out = [];
  for await (const e of chatEvents(sse('event: run.started\ndata: {}\n\n'))) out.push(e);
  assert.deepEqual(out, [{ type: "error", code: "failed" }]);
});

test("pairing codes hash the same however they are typed", () => {
  const code = newPairingCode();
  assert.match(code, /^[A-HJ-NP-Z2-9]{8}$/);
  assert.equal(pairingCodeHash(code), pairingCodeHash(` ${code.slice(0, 4).toLowerCase()}-${code.slice(4)} `));
  assert.notEqual(pairingCodeHash(code), pairingCodeHash("AAAAAAAA"));
});

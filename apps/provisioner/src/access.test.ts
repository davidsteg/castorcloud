import assert from "node:assert/strict";
import { test } from "node:test";
import { ASSISTANT_KEY_NAME, accessCommands, execArgs, strongest } from "./access.ts";

const who = { email: "o'hara@example.com", sub: "sub-1", before: new Date("2026-09-28T10:00:00Z") };
const all = ["photos", "docs", "places", "flights"] as const;

test("the person's e-mail and sub reach the apps only as variables, never inside SQL or code", () => {
  for (const cmd of accessCommands([...all], who, "DEVICES")) {
    assert.ok(!(cmd.input ?? "").includes(who.email), cmd.app);
    assert.ok(!(cmd.input ?? "").includes(who.sub), cmd.app);
    assert.ok(!cmd.command.slice(0, 3).join(" ").includes(who.email), cmd.app);
  }
  const [photos] = accessCommands(["photos"], who, "WEB");
  assert.ok(photos.command.includes(`email=${who.email}`));
});

test("WEB keeps phone apps and keys; DEVICES keeps only the assistant's key; REMOVED keeps nothing", () => {
  const vars = (scope: "WEB" | "DEVICES" | "REMOVED", app: "photos" | "flights") =>
    accessCommands([app], who, scope)[0].command;
  assert.ok(vars("WEB", "photos").includes("web=true"));
  assert.ok(vars("DEVICES", "photos").includes("web=false"));
  assert.ok(vars("DEVICES", "photos").includes(`keep=${ASSISTANT_KEY_NAME}`));
  assert.ok(vars("REMOVED", "flights").includes("keep="));
  assert.equal(accessCommands(["docs"], who, "WEB")[0].env?.PC_SCOPE, "WEB");
});

test("one run per person uses the strongest request", () => {
  assert.equal(strongest(["WEB", "DEVICES"]), "DEVICES");
  assert.equal(strongest(["DEVICES", "REMOVED", "WEB"]), "REMOVED");
  assert.equal(strongest(["WEB"]), "WEB");
});

test("commands run in their service with the environment set", () => {
  const [docs] = accessCommands(["docs"], who, "DEVICES");
  assert.deepEqual(execArgs(docs).slice(0, 6), ["exec", "-T", "-e", `PC_EMAIL=${who.email}`, "-e", "PC_SCOPE=DEVICES"]);
  assert.equal(execArgs(docs)[6], "paperless");
});

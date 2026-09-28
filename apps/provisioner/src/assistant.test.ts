import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";
import { assistantFiles, profileName, type AssistantMember } from "./assistant.ts";
import { assistantToolsEnv } from "./tenant.ts";

const anna: AssistantMember = { id: "cm1anna", email: "anna@x.ch", authentikSub: "sub-a", name: "Anna", locale: "de" };
const ben: AssistantMember = { id: "cm2ben", email: "ben@x.ch", authentikSub: "sub-b", name: null, locale: "fr" };

const files = assistantFiles({
  brandName: "Castor Cloud",
  services: ["photos", "docs"],
  adminKey: "admin-key",
  llmKey: "sk-castor-x",
  members: [
    { member: anna, apiKey: "anna-key", toolsToken: "anna-tools" },
    { member: ben, apiKey: "ben-key", toolsToken: "ben-tools" },
  ],
});

test("one Hermes profile per member, each with its own keys", () => {
  assert.match(files["hermes/profiles/cm1anna/.env"], /^API_SERVER_KEY=anna-key$/m);
  assert.match(files["hermes/profiles/cm1anna/.env"], /^CASTOR_TOOLS_TOKEN=anna-tools$/m);
  assert.match(files["hermes/profiles/cm2ben/.env"], /^API_SERVER_KEY=ben-key$/m);
  assert.doesNotMatch(files["hermes/profiles/cm2ben/.env"], /anna/);
  // The default profile only hosts the listener.
  assert.match(files["hermes/config.yaml"], /multiplex_profiles: true/);
  assert.doesNotMatch(files["hermes/config.yaml"], /mcp_servers/);
});

test("profiles only get tools for booked services, and no shell, files or web", () => {
  const cfg = files["hermes/profiles/cm1anna/config.yaml"];
  assert.match(cfg, /url: http:\/\/assistant-tools:8080\/mcp\/photos/);
  assert.match(cfg, /url: http:\/\/assistant-tools:8080\/mcp\/docs/);
  assert.doesNotMatch(cfg, /mcp\/places|mcp\/flights/);
  assert.match(cfg, /api_server: \[memory, session_search, photos, docs\]/);
  for (const t of ["terminal", "file", "web", "browser", "code_execution"]) assert.match(cfg, new RegExp(`disabled_toolsets: \\[.*\\b${t}\\b`));
  // Only the node's gateway, with the tenant's budgeted key.
  assert.match(cfg, /api: http:\/\/pc-node-llm-gateway:4000\/v1/);
  assert.match(files["hermes/profiles/cm1anna/.env"], /^LLM_API_KEY=sk-castor-x$/m);
});

test("the tools container only learns token hashes", () => {
  const m = JSON.parse(files["members.json"]).members;
  assert.equal(m.length, 2);
  assert.equal(m[0].tokenSha256, createHash("sha256").update("anna-tools").digest("hex"));
  assert.doesNotMatch(files["members.json"], /anna-tools|anna-key/);
});

test("the persona speaks the member's language", () => {
  assert.match(files["hermes/profiles/cm1anna/SOUL.md"], /Answer in German/);
  assert.match(files["hermes/profiles/cm2ben/SOUL.md"], /Answer in French/);
});

test("profile names are URL-safe member ids", () => {
  assert.equal(profileName("cm1anna"), "cm1anna");
  assert.throws(() => profileName("../etc"));
});

test("tool endpoints are only set for booked services", () => {
  const env = assistantToolsEnv(["docs"], { IMMICH_DB_PASSWORD: "p" });
  assert.equal(env.ASSISTANT_PAPERLESS_URL, "http://paperless:8000");
  assert.equal(env.ASSISTANT_IMMICH_URL, "");
  assert.equal(env.ASSISTANT_IMMICH_DB_URL, "");
});

// Writes render.json: what apps/provisioner/src/tenant.ts renders for fixed
// inputs; castord's render module must produce the same.
//   node --experimental-strip-types gen-render.mts > render.json
import { readFileSync } from "node:fs";
import { aiBudgetUsd } from "../../../../../packages/pricing/src/ai-budget.ts";
import { assistantFiles } from "../../../../../apps/provisioner/src/assistant.ts";
import {
  SECRET_KEYS, STACK_SERVICES, edgeRoute, immichConfig, parseEnvFile, redirectUris, tenantEnv, volumesOverride,
} from "../../../../../apps/provisioner/src/tenant.ts";

const p = { baseDomain: "castor.test", scheme: "https" as const, brandName: "Castor Cloud", timeZone: "Europe/Zurich", authHostIp: "10.0.0.1" };
const secrets = Object.fromEntries(SECRET_KEYS.map((k, i) => [k, `s${i}${"0".repeat(8)}`]));
const versions = parseEnvFile(readFileSync(new URL("../../../../../packages/stack-templates/versions.env", import.meta.url), "utf8"));
const env = (assistant: boolean) =>
  tenantEnv({
    slug: "anna", p, secrets, versions, configDir: "/n/tenants/anna", brandDir: "/n/files/brand", caDir: "/n/files/ca",
    assistantServices: assistant ? ["photos", "flights"] : [],
  });
console.log(JSON.stringify({
  comment: "Rendered by apps/provisioner/src/tenant.ts. Regenerate with gen-render.mts",
  platform: p,
  secrets,
  versions,
  immich: JSON.stringify(immichConfig("anna", p, secrets), null, 2),
  env: env(false),
  env_assistant: env(true),
  edge: edgeRoute("anna", p, "http://10.0.0.5:8480"),
  volumes: volumesOverride("/mnt/tank/castor/t/anna/data"),
  assistant: assistantFiles({
    brandName: p.brandName, services: ["photos", "flights"], adminKey: "admin1", llmKey: "sk-llm",
    members: [
      { member: { id: "cmanna", email: "anna@example.ch", authentikSub: "sub-a", name: "Anna", locale: "de" }, apiKey: "ka", toolsToken: "ta" },
      { member: { id: "cmbert", email: "bert@example.ch", authentikSub: "sub-b", name: null, locale: "rm" }, apiKey: "kb", toolsToken: "tb" },
    ],
  }),
  budgets: [0, 5, 10, 25, 49, 100].map((c) => [c, aiBudgetUsd(c, 1.1)]),
  redirects: Object.fromEntries(STACK_SERVICES.map((s) => [s, redirectUris("anna", p, s)])),
}, null, 2));

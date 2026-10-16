// Writes render.json: what apps/provisioner/src/tenant.ts renders for fixed
// inputs; castord's render module must produce the same.
//   node --experimental-strip-types gen-render.mts > render.json
import { readFileSync } from "node:fs";
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
  redirects: Object.fromEntries(STACK_SERVICES.map((s) => [s, redirectUris("anna", p, s)])),
}, null, 2));

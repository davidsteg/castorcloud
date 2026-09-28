// The tenant assistant (Hermes Agent): what the provisioner renders for it.
//
// One Hermes container per tenant, one Hermes profile per household member.
// A profile has its own memory, its own API key (used by the portal to chat
// on the member's behalf) and its own token for the read-only data tools, so
// a member's assistant only ever sees that member's data.
//
// Files are rendered into <configDir>/assistant/ and copied into the Hermes
// data volume (on the tenant's encrypted volume) by the assistant-init
// container, see packages/stack-templates/tenant/compose.yml.
import { createHash, randomBytes } from "node:crypto";
import type { ServiceKey } from "./tenant.ts";

export type AssistantMember = {
  /** Member row id; also the Hermes profile name. */
  id: string;
  email: string;
  authentikSub: string;
  name: string | null;
  locale: string;
};

/** Per-member secrets, stored encrypted in the Secret table like all others. */
export const memberKeyName = (memberId: string) => `ASSISTANT_KEY_${memberId}`;
export const memberToolsTokenName = (memberId: string) => `ASSISTANT_TOOLS_${memberId}`;
export const isMemberSecret = (key: string) => /^ASSISTANT_(KEY|TOOLS)_/.test(key);

/** Hermes refuses API keys shorter than 16 characters; use 256 bits. */
export const newAssistantSecret = () => randomBytes(32).toString("hex");

/** Hermes profile names become URL segments (/p/<profile>/...). */
export function profileName(memberId: string) {
  if (!/^[a-z0-9][a-z0-9-]{0,63}$/.test(memberId)) throw new Error(`member id not usable as profile name: ${memberId}`);
  return memberId;
}

/** Where the portal reaches a member's assistant (via the skin proxy). */
export const assistantPath = (memberId: string) => `/p/${profileName(memberId)}`;

const GATEWAY_URL = "http://pc-node-llm-gateway:4000/v1";

/**
 * Everything Hermes could do beyond talking and remembering. The assistant
 * has no shell, files, browser or web access: its only tools are our
 * read-only MCP servers, memory and search over its own past sessions.
 */
const DISABLED_TOOLSETS = [
  "terminal", "file", "web", "search", "browser", "code_execution", "delegation", "cronjob",
  "image_gen", "video_gen", "vision", "video", "tts", "stt", "x_search", "skills", "todo", "clarify",
  "kanban", "computer_use", "homeassistant", "connections", "messaging",
];

const LANGUAGE: Record<string, string> = { de: "German", fr: "French", it: "Italian", en: "English" };

/** Shared by the default profile and every member profile. */
function baseConfig(mcp: string[]) {
  const lines = [
    "# Rendered by the Castor provisioner; changes are overwritten on the next update.",
    "model:",
    "  provider: castor",
    "  default: assistant",
    "providers:",
    "  castor:",
    `    api: ${GATEWAY_URL}`,
    "    api_key: ${LLM_API_KEY}",
    "platform_toolsets:",
    `  api_server: [${["memory", "session_search", ...mcp].join(", ")}]`,
    "agent:",
    "  max_turns: 12",
    `  disabled_toolsets: [${DISABLED_TOOLSETS.join(", ")}]`,
    "memory:",
    "  memory_enabled: true",
    "  user_profile_enabled: true",
    // A handful of small tools: give the model their schemas directly instead
    // of a search step (fewer round trips, less spend).
    "tools:",
    "  tool_search:",
    "    enabled: off",
  ];
  if (mcp.length) {
    lines.push("mcp_servers:");
    for (const s of mcp) {
      lines.push(
        `  ${s}:`,
        `    url: http://assistant-tools:8080/mcp/${s}`,
        "    headers:",
        "      Authorization: Bearer ${CASTOR_TOOLS_TOKEN}",
        "    timeout: 60",
        "    trust: untrusted", // anything not marked read-only would need approval
        "    tools:",
        "      resources: false",
        "      prompts: false",
      );
    }
  }
  return lines;
}

/**
 * Default profile: owns the API listener that serves every member profile at
 * /p/<member>/. Nobody chats with it; its key stays in the stack.
 */
export function defaultConfig() {
  return [
    ...baseConfig([]),
    "gateway:",
    "  multiplex_profiles: true",
    "  api_server:",
    "    enabled: true",
    "    host: 0.0.0.0",
    "    port: 8642",
    "",
  ].join("\n");
}

export function profileConfig(services: ServiceKey[]) {
  return [...baseConfig(services), ""].join("\n");
}

const envFile = (vars: Record<string, string>) =>
  Object.entries(vars).map(([k, v]) => `${k}=${v}`).join("\n") + "\n";

export function defaultEnv(adminKey: string, llmKey: string) {
  return envFile({ API_SERVER_KEY: adminKey, LLM_API_KEY: llmKey });
}

export function profileEnv(apiKey: string, llmKey: string, toolsToken: string) {
  return envFile({ API_SERVER_KEY: apiKey, LLM_API_KEY: llmKey, CASTOR_TOOLS_TOKEN: toolsToken });
}

const SERVICE_WORDS: Record<ServiceKey, string> = {
  photos: "photos and videos (Photos)",
  docs: "scanned documents (Documents)",
  places: "location history (Timeline)",
  flights: "flights (Flights)",
};

/** The assistant's personality and rules (Hermes' SOUL.md). */
export function soul(opts: { brandName: string; member: AssistantMember; services: ServiceKey[] }) {
  const { brandName, member, services } = opts;
  const language = LANGUAGE[member.locale] ?? "the user's language";
  const data = services.length ? services.map((s) => SERVICE_WORDS[s]).join(", ") : "no data services yet";
  return `# ${brandName} assistant

You are the personal assistant inside ${brandName}, a private cloud hosted in
Switzerland. You help ${member.name || "the user"} find things in their own data:
${data}.

- Answer in ${language} unless the user writes in another language.
- Use the tools to look things up; never invent dates, places, amounts or names.
  If the tools find nothing, say so.
- Ask for as little data as needed: search narrowly, read a document only when
  its excerpt is not enough.
- Every result has a link. Include the links that matter, so the user can open
  the photo, document, map or flight.
- You cannot change, delete, share or upload anything, and you have no internet
  access. Say so when asked.
- The data belongs to this user alone. Other household members have their own
  assistant; you cannot see their data.
`;
}

export type MembersFile = {
  members: { id: string; email: string; sub: string; name?: string; tokenSha256: string }[];
};

/** For the tools container: who each tools token belongs to (hashes only). */
export function membersFile(members: { member: AssistantMember; toolsToken: string }[]): string {
  const file: MembersFile = {
    members: members.map(({ member, toolsToken }) => ({
      id: member.id,
      email: member.email,
      sub: member.authentikSub,
      ...(member.name ? { name: member.name } : {}),
      tokenSha256: createHash("sha256").update(toolsToken).digest("hex"),
    })),
  };
  return JSON.stringify(file, null, 2) + "\n";
}

/**
 * Run by assistant-init before Hermes starts: installs the rendered config
 * and removes the profiles (and with them the memory) of members who left.
 */
export const SYNC_SCRIPT = `#!/bin/sh
# Rendered by the Castor provisioner.
set -eu
src=/rendered/hermes
dst=/opt/data
mkdir -p "$dst/profiles"
cp "$src/config.yaml" "$src/.env" "$dst/"
for p in "$dst"/profiles/*/; do
  [ -d "$p" ] || continue
  name=$(basename "$p")
  [ -d "$src/profiles/$name" ] || rm -rf "$p"
done
for p in "$src"/profiles/*/; do
  [ -d "$p" ] || continue
  name=$(basename "$p")
  mkdir -p "$dst/profiles/$name"
  cp "$p/config.yaml" "$p/.env" "$p/SOUL.md" "$dst/profiles/$name/"
done
chown -R 10000:10000 "$dst"
`;

/** All files below <configDir>/assistant/, keyed by relative path. */
export function assistantFiles(opts: {
  brandName: string;
  services: ServiceKey[];
  adminKey: string;
  llmKey: string;
  members: { member: AssistantMember; apiKey: string; toolsToken: string }[];
}): Record<string, string> {
  const files: Record<string, string> = {
    "sync.sh": SYNC_SCRIPT,
    "members.json": membersFile(opts.members),
    "hermes/config.yaml": defaultConfig(),
    "hermes/.env": defaultEnv(opts.adminKey, opts.llmKey),
  };
  for (const { member, apiKey, toolsToken } of opts.members) {
    const dir = `hermes/profiles/${profileName(member.id)}`;
    files[`${dir}/config.yaml`] = profileConfig(opts.services);
    files[`${dir}/.env`] = profileEnv(apiKey, opts.llmKey, toolsToken);
    files[`${dir}/SOUL.md`] = soul({ brandName: opts.brandName, member, services: opts.services });
  }
  return files;
}

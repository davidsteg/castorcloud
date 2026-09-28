// Read-only MCP tools over one tenant's services, for that tenant's Hermes.
// One MCP endpoint per booked service (/mcp/photos, /mcp/docs, ...), so
// Hermes shows them as separate servers. Stateless Streamable HTTP: every
// request is authenticated on its own and gets tools bound to its member.
//
// Runs only on the tenant's private network; never exposed through Traefik.
import http from "node:http";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { loadMembers, memberForToken, type Member } from "./members.ts";
import type { ServiceKey, ServiceModule } from "./service.ts";
import { docs } from "./services/docs.ts";
import { flights } from "./services/flights.ts";
import { photos } from "./services/photos.ts";
import { places } from "./services/places.ts";

const MEMBERS_FILE = process.env.MEMBERS_FILE ?? "/config/members.json";
const PORTAL_ORIGIN = process.env.PORTAL_ORIGIN ?? "";
const PORT = Number(process.env.PORT ?? 8080);

const modules = new Map<ServiceKey, ServiceModule>(
  [photos, docs, places, flights].filter((m) => m.configured).map((m) => [m.key, m]),
);

// Re-read on every request: the provisioner rewrites it when members change.
function members(): Member[] {
  try {
    return loadMembers(MEMBERS_FILE);
  } catch (err) {
    console.error(`cannot read ${MEMBERS_FILE}:`, (err as Error).message);
    return [];
  }
}

async function readBody(req: http.IncomingMessage, limit = 256 * 1024): Promise<unknown> {
  let size = 0;
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    size += (chunk as Buffer).length;
    if (size > limit) throw new Error("body too large");
    chunks.push(chunk as Buffer);
  }
  return chunks.length ? JSON.parse(Buffer.concat(chunks).toString("utf8")) : undefined;
}

function send(res: http.ServerResponse, status: number, body: object) {
  res.writeHead(status, { "Content-Type": "application/json" }).end(JSON.stringify(body));
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", "http://tools");
  if (url.pathname === "/healthz") return send(res, 200, { ok: true, services: [...modules.keys()] });

  const service = url.pathname.match(/^\/mcp\/([a-z]+)\/?$/)?.[1] as ServiceKey | undefined;
  const module = service && modules.get(service);
  if (!module) return send(res, 404, { error: "unknown service" });
  if (req.method !== "POST") return send(res, 405, { error: "stateless server: POST only" });

  const member = memberForToken(members(), req.headers.authorization);
  if (!member) return send(res, 401, { error: "unauthorized" });

  let body: unknown;
  try {
    body = await readBody(req);
  } catch {
    return send(res, 400, { error: "invalid body" });
  }

  const mcp = new McpServer({ name: `castor-${module.key}`, version: "1.0.0" });
  module.register(mcp, { member, portalOrigin: PORTAL_ORIGIN });
  const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });
  res.on("close", () => {
    void transport.close();
    void mcp.close();
  });
  try {
    await mcp.connect(transport);
    await transport.handleRequest(req, res, body);
  } catch (err) {
    console.error(err);
    if (!res.headersSent) send(res, 500, { error: "internal error" });
  }
});

server.listen(PORT, () => console.log(`assistant tools on :${PORT} (${[...modules.keys()].join(", ") || "no services"})`));

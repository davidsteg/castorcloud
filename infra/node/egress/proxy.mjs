// Egress proxy of one custom stack. The stack's containers have no route to
// anywhere (internal network); this proxy is their only way out, and it lets
// them reach exactly the hosts the stack lists (EGRESS_ALLOW), over HTTPS
// (CONNECT to port 443) or plain HTTP (port 80). Never the LAN, the node or
// the control plane: every address a name resolves to is checked, and the
// proxy connects to the address it checked (no DNS rebinding).
import dns from "node:dns/promises";
import http from "node:http";
import net from "node:net";

const PORT = Number(process.env.EGRESS_PORT ?? 3128);
const MAX_SOCKETS = 256;
const IDLE_MS = 5 * 60_000;

/** "api.example.com", "*.example.org" (subdomains only). */
export function parseAllow(list) {
  return (list ?? "").split(",").map((h) => h.trim().toLowerCase()).filter(Boolean);
}

export function allowed(host, allow) {
  const h = host.toLowerCase().replace(/\.$/, "");
  return allow.some((a) => (a.startsWith("*.") ? h.endsWith(a.slice(1)) && h.length > a.length - 1 : h === a));
}

// Everything that isn't the public internet.
const blocked = new net.BlockList();
for (const [net4, prefix] of [
  ["0.0.0.0", 8], ["10.0.0.0", 8], ["100.64.0.0", 10], ["127.0.0.0", 8], ["169.254.0.0", 16], ["172.16.0.0", 12],
  ["192.0.0.0", 24], ["192.0.2.0", 24], ["192.88.99.0", 24], ["192.168.0.0", 16], ["198.18.0.0", 15],
  ["198.51.100.0", 24], ["203.0.113.0", 24], ["224.0.0.0", 4], ["240.0.0.0", 4],
]) blocked.addSubnet(net4, prefix, "ipv4");
for (const [net6, prefix] of [
  ["::", 128], ["::1", 128], ["64:ff9b::", 96], ["64:ff9b:1::", 48], ["100::", 64], ["2001::", 23],
  ["2001:db8::", 32], ["2002::", 16], ["fc00::", 7], ["fe80::", 10], ["ff00::", 8],
]) blocked.addSubnet(net6, prefix, "ipv6");

export function isPublic(address) {
  // IPv4-mapped IPv6 (::ffff:10.0.0.1): judge the IPv4 address.
  const mapped = /^::ffff:(\d+\.\d+\.\d+\.\d+)$/i.exec(address);
  if (mapped) return isPublic(mapped[1]);
  const family = net.isIPv4(address) ? "ipv4" : net.isIPv6(address) ? "ipv6" : null;
  return family !== null && !blocked.check(address, family);
}

/** The first address of `host` if every address it has is public; else null. */
async function resolve(host) {
  if (net.isIP(host)) return null; // names only: an allowlist of names says nothing about raw IPs
  let addrs;
  try {
    addrs = await dns.lookup(host, { all: true, verbatim: true });
  } catch {
    return null;
  }
  if (!addrs.length || !addrs.every((a) => isPublic(a.address))) return null;
  return addrs[0].address;
}

function deny(what, why) {
  console.log(JSON.stringify({ denied: what.slice(0, 255), why }));
}

export function startProxy(allow, port = PORT) {
  const server = http.createServer(async (req, res) => {
    // Plain HTTP through the proxy: absolute URI, port 80 only.
    let url;
    try {
      url = new URL(req.url ?? "");
    } catch {
      res.writeHead(400).end();
      return;
    }
    const port = url.port ? Number(url.port) : 80;
    if (url.protocol !== "http:" || port !== 80 || !allowed(url.hostname, allow)) {
      deny(url.hostname, "not allowed");
      res.writeHead(403).end("egress not allowed\n");
      return;
    }
    const address = await resolve(url.hostname);
    if (!address) {
      deny(url.hostname, "not a public address");
      res.writeHead(403).end("egress not allowed\n");
      return;
    }
    const headers = { ...req.headers, host: url.host };
    delete headers["proxy-connection"];
    delete headers["proxy-authorization"];
    const upstream = http.request({ host: address, port, method: req.method, path: url.pathname + url.search, headers, timeout: IDLE_MS }, (up) => {
      res.writeHead(up.statusCode ?? 502, up.headers);
      up.pipe(res);
    });
    upstream.on("error", () => res.headersSent ? res.destroy() : res.writeHead(502).end());
    upstream.on("timeout", () => upstream.destroy());
    req.pipe(upstream);
  });

  server.on("connect", async (req, client, head) => {
    const m = (req.url ?? "").match(/^([A-Za-z0-9.-]{1,253}):(\d{1,5})$/);
    const host = m?.[1] ?? "";
    if (!m || Number(m[2]) !== 443 || !allowed(host, allow)) {
      deny(req.url ?? "", "not allowed");
      client.end("HTTP/1.1 403 Forbidden\r\n\r\n");
      return;
    }
    const address = await resolve(host);
    if (!address) {
      deny(host, "not a public address");
      client.end("HTTP/1.1 403 Forbidden\r\n\r\n");
      return;
    }
    const upstream = net.connect({ host: address, port: 443 });
    upstream.setTimeout(IDLE_MS, () => upstream.destroy());
    client.setTimeout(IDLE_MS, () => client.destroy());
    upstream.on("connect", () => {
      client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      if (head?.length) upstream.write(head);
      upstream.pipe(client);
      client.pipe(upstream);
    });
    upstream.on("error", () => client.destroy());
    client.on("error", () => upstream.destroy());
  });

  server.maxConnections = MAX_SOCKETS;
  server.headersTimeout = 30_000;
  server.requestTimeout = 0; // tunnels and long downloads; idle timeouts above
  return server.listen(port);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const allow = parseAllow(process.env.EGRESS_ALLOW);
  startProxy(allow);
  console.log(JSON.stringify({ listening: PORT, allow }));
}

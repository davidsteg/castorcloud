import assert from "node:assert/strict";
import { test } from "node:test";
import { allowed, isPublic, parseAllow } from "./proxy.mjs";

test("allowlist: exact names and subdomain wildcards", () => {
  const allow = parseAllow("api.example.com, *.example.org");
  assert.ok(allowed("api.example.com", allow));
  assert.ok(allowed("API.example.com.", allow));
  assert.ok(allowed("a.b.example.org", allow));
  assert.ok(!allowed("example.org", allow));
  assert.ok(!allowed("evilexample.org", allow));
  assert.ok(!allowed("api.example.com.evil.net", allow));
  assert.ok(!allowed("anything", []));
});

test("only public addresses", () => {
  for (const a of ["10.1.2.3", "192.168.1.1", "172.20.0.1", "127.0.0.1", "169.254.169.254", "100.64.0.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "::ffff:a00:1", "0.0.0.0"]) {
    assert.equal(isPublic(a), false, a);
  }
  for (const a of ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"]) assert.equal(isPublic(a), true, a);
  assert.equal(isPublic("example.com"), false);
});

test("an allowed name that resolves to the LAN or the node is refused", async () => {
  const { startProxy } = await import("./proxy.mjs");
  const server = startProxy(["localhost"], 0);
  await new Promise((r) => server.once("listening", r));
  const { port } = server.address();
  const net = await import("node:net");
  const reply = await new Promise((resolve) => {
    const s = net.connect(port, "127.0.0.1", () => s.write("CONNECT localhost:443 HTTP/1.1\r\nHost: localhost:443\r\n\r\n"));
    s.once("data", (d) => { resolve(String(d)); s.destroy(); });
  });
  assert.match(reply, /^HTTP\/1\.1 403/);
  const plain = await fetch(`http://127.0.0.1:${port}/`, { headers: {} }).then((r) => r.status);
  assert.equal(plain, 400);
  server.close();
});

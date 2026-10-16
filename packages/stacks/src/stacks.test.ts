import assert from "node:assert/strict";
import { test } from "node:test";
import { CATALOG } from "./catalog.ts";
import { digestFor, parseImageRef, pinned } from "./image.ts";
import { controlPlaneRoutes, nodeRoute, renderCompose, stackNames } from "./render.ts";
import { SpecError, parseMemory, parseStack, secretNames, specUnits } from "./spec.ts";

const DIGEST = `sha256:${"a".repeat(64)}`;

const problems = (source: string) => {
  try {
    parseStack(source);
  } catch (err) {
    if (err instanceof SpecError) return err.problems.join("\n");
    throw err;
  }
  return "";
};

const minimal = `
services:
  app:
    image: nginx:1.29-alpine
    mem_limit: 256m
    cpus: 0.5
x-castor:
  web: app:80
`;

test("a minimal stack parses", () => {
  const spec = parseStack(minimal);
  assert.equal(spec.services.app.image, "docker.io/library/nginx:1.29-alpine");
  assert.deepEqual(spec.web, { service: "app", port: 80 });
  assert.deepEqual(specUnits(spec), { memoryUnits: 1, cpuUnits: 2, storageUnits: 0 });
});

test("every catalog app is a valid stack pinned by digest", () => {
  for (const app of CATALOG) {
    const spec = parseStack(app.source);
    for (const svc of Object.values(spec.services)) assert.match(svc.image, /@sha256:[a-f0-9]{64}$/, app.key);
  }
});

test("dangerous compose keys are refused with a reason", () => {
  for (const [key, value] of [
    ["privileged", "true"], ["cap_add", "[SYS_ADMIN]"], ["network_mode", "host"], ["pid", "host"],
    ["devices", "['/dev/kvm:/dev/kvm']"], ["labels", "{traefik.enable: 'true'}"], ["ports", "['80:80']"],
    ["security_opt", "['seccomp=unconfined']"], ["build", "."], ["sysctls", "{a: 1}"], ["userns_mode", "host"],
  ]) {
    const src = minimal.replace("    cpus: 0.5", `    cpus: 0.5\n    ${key}: ${value}`);
    assert.match(problems(src), new RegExp(`services\\.app`), key);
  }
  assert.match(problems(`${minimal}networks:\n  x: {}\n`), /networks are managed/);
});

test("host paths, sockets and system paths can't be mounted", () => {
  const withVol = (mount: string) => minimal.replace("    cpus: 0.5", `    cpus: 0.5\n    volumes:\n      - ${mount}`) + "volumes:\n  data:\n";
  assert.match(problems(withVol("/var/run/docker.sock:/var/run/docker.sock")), /host paths are not allowed/);
  assert.match(problems(withVol("./x:/x")), /host paths are not allowed/);
  assert.match(problems(withVol("data:/etc")), /can't mount over \/etc/);
  assert.match(problems(withVol("data:/srv/../etc")), /invalid container path/);
  assert.match(problems(withVol("{type: bind, source: /, target: /host}")), /only named volumes/);
  assert.match(problems(withVol("other:/data")), /not declared/);
  // Volumes need storage booked.
  assert.match(problems(withVol("data:/data")), /storage_gb/);
  assert.equal(problems(withVol("data:/data").replace("  web: app:80", "  web: app:80\n  storage_gb: 10")), "");
});

test("limits are required and bounded", () => {
  assert.match(problems(minimal.replace("    mem_limit: 256m\n", "")), /memory limit/);
  assert.match(problems(minimal.replace("    cpus: 0.5\n", "")), /CPU limit/);
  assert.match(problems(minimal.replace("256m", "64g")), /memory must be/);
  assert.match(problems(minimal.replace("cpus: 0.5", "cpus: 64")), /cpus must be/);
  const deploy = minimal.replace("    mem_limit: 256m\n    cpus: 0.5\n", "    deploy:\n      resources:\n        limits: {memory: 1g, cpus: '1.5'}\n");
  assert.deepEqual(specUnits(parseStack(deploy)), { memoryUnits: 4, cpuUnits: 6, storageUnits: 0 });
  assert.equal(parseMemory("1.5GB"), 1536);
  assert.equal(parseMemory("abc"), null);
});

test("only allowed registries, and proxy variables are ours", () => {
  assert.match(problems(minimal.replace("nginx:1.29-alpine", "10.0.0.5:5000/evil")), /registry 10\.0\.0\.5:5000 is not allowed/);
  assert.match(problems(minimal.replace("nginx:1.29-alpine", "localhost/evil")), /registry localhost is not allowed/);
  assert.match(problems(minimal.replace("    cpus: 0.5", "    cpus: 0.5\n    environment:\n      HTTPS_PROXY: http://x")), /set by Castor Cloud/);
});

test("YAML bombs and duplicate keys are refused", () => {
  const bomb = ["a: &a [x,x,x,x,x,x,x,x,x]", ...Array.from({ length: 30 }, (_, i) => `${String.fromCharCode(98 + (i % 20))}${i}: &l${i} [${"*" + (i === 0 ? "a" : `l${i - 1}`)},${"*" + (i === 0 ? "a" : `l${i - 1}`)}]`)].join("\n");
  assert.match(problems(bomb), /not valid YAML|must be a map/);
  assert.match(problems(minimal.replace("    cpus: 0.5", "    cpus: 0.5\n    cpus: 1")), /not valid YAML/);
  assert.match(problems("x".repeat(70 * 1024)), /larger than/);
});

test("egress hosts are host names only", () => {
  const withEgress = (hosts: string) => minimal.replace("  web: app:80", `  web: app:80\n  egress: ${hosts}`);
  assert.deepEqual(parseStack(withEgress("[api.example.com, '*.example.org']")).egress, ["api.example.com", "*.example.org"]);
  assert.match(problems(withEgress("['10.0.0.1']")), /invalid host/);
  assert.match(problems(withEgress("['example.com:25']")), /invalid host/);
  assert.match(problems(withEgress("['*']")), /invalid host/);
});

test("image references", () => {
  const r = parseImageRef("ghcr.io/immich-app/immich-server:v2");
  assert.ok(!("error" in r));
  if ("error" in r) return;
  assert.equal(r.canonical, "ghcr.io/immich-app/immich-server:v2");
  assert.equal(pinned(r, DIGEST), `ghcr.io/immich-app/immich-server:v2@${DIGEST}`);
  assert.equal(digestFor(r, [`ghcr.io/other/x@${DIGEST}`]), null);
  assert.equal(digestFor(r, [`ghcr.io/immich-app/immich-server@${DIGEST}`]), DIGEST);
  const hub = parseImageRef("nginx");
  assert.ok(!("error" in hub) && hub.canonical === "docker.io/library/nginx:latest");
  if (!("error" in hub)) assert.equal(digestFor(hub, [`nginx@${DIGEST}`]), DIGEST);
  assert.ok("error" in parseImageRef("Nginx"));
  assert.ok("error" in parseImageRef("nginx@sha256:1234"));
  assert.ok("error" in parseImageRef("evil.example/x"));
});

const ctx = {
  slug: "muster", stack: "links", origin: "https://links-muster.castorapps.ch", dataDir: "/Volumes/castor/s/muster/links/data",
  images: { app: `docker.io/library/nginx:1.29-alpine@${DIGEST}` }, secrets: { db: "s3cr3t" }, egressImage: "castor.local/egress:1",
};

test("render hardens every container and escapes customer strings", () => {
  const spec = parseStack(`
services:
  app:
    image: nginx:1.29-alpine
    command: ["sh", "-c", "echo $HOME"]
    environment:
      PASSWORD: \${secret:db}
      ORIGIN: \${castor:origin}
      PRICE: "5$"
      REF: \${HOME}
    volumes: [data:/data]
    mem_limit: 256m
    cpus: 0.5
volumes:
  data:
x-castor:
  web: app:80
  storage_gb: 10
`);
  assert.deepEqual(secretNames(spec), ["db"]);
  const out = renderCompose(spec, ctx) as { name: string; services: Record<string, Record<string, unknown>>; networks: Record<string, Record<string, unknown>> };
  const app = out.services.app;
  assert.equal(out.name, "pc-s-muster-links");
  assert.deepEqual(app.cap_drop, ["ALL"]);
  assert.deepEqual(app.security_opt, ["no-new-privileges:true"]);
  assert.equal(app.mem_limit, "256m");
  assert.equal(app.memswap_limit, "256m");
  assert.equal(app.pids_limit, 512);
  assert.equal(app.image, ctx.images.app);
  assert.deepEqual(app.command, ["sh", "-c", "echo $$HOME"]);
  assert.deepEqual(app.environment, { PASSWORD: "s3cr3t", ORIGIN: ctx.origin, PRICE: "5$$", REF: "$${HOME}" });
  assert.deepEqual(app.volumes, [{ type: "bind", source: `${ctx.dataDir}/data`, target: "/data", read_only: false, bind: { create_host_path: false } }]);
  assert.deepEqual(app.networks, { app: {}, edge: { aliases: ["pc-s-muster-links-web"] } });
  assert.equal(out.networks.app.internal, true);
  assert.equal(out.networks.edge.internal, true);
  assert.equal(out.networks.out, undefined, "no way out without egress hosts");
  assert.equal(out.services["castor-egress"], undefined);
});

test("egress goes through the proxy only", () => {
  const spec = parseStack(minimal.replace("  web: app:80", "  web: app:80\n  egress: [api.example.com]"));
  const out = renderCompose(spec, { ...ctx, runtime: "runsc" }) as { services: Record<string, Record<string, unknown>>; networks: Record<string, Record<string, unknown>> };
  const env = out.services.app.environment as Record<string, string>;
  assert.equal(env.HTTPS_PROXY, "http://castor-egress:3128");
  assert.equal(out.services.app.runtime, "runsc");
  const proxy = out.services["castor-egress"];
  assert.deepEqual(proxy.networks, { app: {}, out: {} });
  assert.deepEqual((proxy.environment as Record<string, string>).EGRESS_ALLOW, "api.example.com");
  assert.deepEqual(proxy.cap_add, []);
  assert.equal(out.networks.out.internal, undefined);
  // Only the proxy is on the outbound network.
  assert.deepEqual(Object.keys(out.services.app.networks as object), ["app", "edge"]);
});

test("render refuses unpinned images", () => {
  assert.throws(() => renderCompose(parseStack(minimal), { ...ctx, images: { app: "nginx:latest" } }), /not pinned/);
});

test("routes: the node only accepts the control plane, the control plane signs in first", () => {
  const node = nodeRoute({ slug: "muster", stack: "links", host: "links-muster.castorapps.ch", port: 9090 });
  assert.match(node, /middlewares: \[castor-control-plane\]/);
  assert.match(node, /http:\/\/pc-s-muster-links-web:9090/);
  const cp = controlPlaneRoutes({ slug: "muster", stacks: [{ stack: "links", host: "links-muster.castorapps.ch" }], edgeUrl: "http://10.0.0.40", stacksDomain: "castorapps.ch", certResolver: "le" });
  assert.match(cp, /middlewares: \[castor-stack-strip@file, castor-stack-auth@file\]/);
  assert.match(cp, /PathPrefix\(`\/outpost\.goauthentik\.io\/`\)/);
  assert.match(cp, /certResolver: le/);
  assert.equal(stackNames("muster", "links").authSlug, "stack-muster-links");
});

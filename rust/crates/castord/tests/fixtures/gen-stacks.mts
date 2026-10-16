// Writes stacks.json: what @pc/stacks and apps/provisioner/src/stacks.ts
// decide for fixed inputs; castord's stacks module must decide the same.
//   node --experimental-strip-types gen-stacks.mts > stacks.json
import { parse as parseYaml } from "yaml";
import { coveredStacks, parseIds, resolveUser, uncoveredVolumes } from "../../../../../apps/provisioner/src/stacks.ts";
import {
  CATALOG, SpecError, checkStack, controlPlaneRoutes, digestFor, nodeMiddlewares, nodeRoute, parseImageRef, parseMemory, pinned,
  renderCompose, secretNames, specUnits, stackNames, storedDefinition,
} from "../../../../../packages/stacks/src/index.ts";

const DIGEST = `sha256:${"a".repeat(64)}`;
const minimal = `
services:
  app:
    image: nginx:1.29-alpine
    mem_limit: 256m
    cpus: 0.5
x-castor:
  web: app:80
`;
const withSvc = (extra: string) => minimal.replace("    cpus: 0.5", `    cpus: 0.5\n${extra}`);
const withVol = (mount: string) => withSvc(`    volumes:\n      - ${mount}`) + "volumes:\n  data:\n";
const withCastor = (extra: string) => minimal.replace("  web: app:80", `  web: app:80\n${extra}`);
const rendered = `
services:
  app:
    image: nginx:1.29-alpine
    command: ["sh", "-c", "echo $HOME"]
    entrypoint: /init
    working_dir: /srv
    user: "1000:1000"
    environment:
      PASSWORD: \${secret:db}
      TOKEN: x\${secret:api_key}y\${secret:db}
      ORIGIN: \${castor:origin}
      HOST: \${castor:host}
      PRICE: "5$"
      REF: \${HOME}
      PORT: 8080
      RATIO: 0.5
      ON: true
    volumes: [data:/data, {type: volume, source: cache, target: /cache, read_only: true}]
    tmpfs: /var/cache/nginx/
    healthcheck:
      test: wget -q -O- localhost
      interval: 1m30s
      retries: 5
    depends_on:
      db: {condition: service_healthy}
    mem_limit: 256m
    cpus: 0.5
  db:
    image: postgres:17@${DIGEST}
    environment: ["POSTGRES_PASSWORD=\${secret:db}", "A=b=c"]
    volumes: [pg:/var/lib/postgresql/data]
    healthcheck: {test: [CMD, pg_isready], timeout: 5s, start_period: 30s}
    deploy: {resources: {limits: {memory: 1.5GB, cpus: "1.25"}}}
    restart: always
volumes:
  data:
  cache: {}
  pg:
x-castor:
  web: app:80
  egress: [API.example.com, "*.example.org", api.example.com]
  storage_gb: 25
  label: Demo
`;

// Stack definitions and what checkStack makes of them (the parsed document,
// as the YAML parser gave it, is what castord sees).
const sources: [string, string][] = [
  ["minimal", minimal],
  ["rendered", rendered],
  ...CATALOG.map((a) => [`catalog-${a.key}`, a.source] as [string, string]),
  ...[
    ["privileged", "true"], ["cap_add", "[SYS_ADMIN]"], ["network_mode", "host"], ["pid", "host"],
    ["devices", "['/dev/kvm:/dev/kvm']"], ["labels", "{traefik.enable: 'true'}"], ["ports", "['80:80']"],
    ["security_opt", "['seccomp=unconfined']"], ["build", "."], ["sysctls", "{a: 1}"], ["userns_mode", "host"],
    ["read_only", "false"], ["tmpfs", "[/a, /b, /c, /d, /e]"], ["tmpfs", "[/proc]"], ["restart", "[x]"],
    ["user", "root;id"], ["user", "[1000]"], ["user", "1000"], ["user", "{a: 1}"], ["working_dir", "srv"],
    ["command", "[a, 1, 2.5]"], ["command", "{a: 1}"], ["entrypoint", "\"a\\u0000b\""],
    ["environment", "[NOEQUALS]"], ["environment", "{1BAD: x, OK: null, OBJ: {a: 1}, HTTPS_PROXY: x}"], ["environment", "x"],
    ["environment", "{\"LONG\": \"" + "x".repeat(5000) + "\"}"], ["environment", "{CTRL: \"a\\u0007b\", TAB: \"a\\tb\"}"],
    ["healthcheck", "{test: [NOPE, x]}"], ["healthcheck", "{test: x, interval: 1s, timeout: 1h, retries: 0, start_period: 2h, extra: 1}"],
    ["healthcheck", "{test: x, retries: \"7\", interval: 90}"], ["healthcheck", "x"],
    ["depends_on", "[ghost]"], ["depends_on", "[app]"], ["depends_on", "x"],
    ["deploy", "x"], ["deploy", "{replicas: 2, resources: {reservations: {}, limits: {memory: 1g, pids: 5}}}"],
    ["volumes", "x"], ["volumes", "[data]"],
  ].map(([k, v], i) => [`svc-${i}-${k}`, withSvc(`    ${k}: ${v}`)] as [string, string]),
  ["no-mem", minimal.replace("    mem_limit: 256m\n", "")],
  ["no-cpu", minimal.replace("    cpus: 0.5\n", "")],
  ["big-mem", minimal.replace("256m", "64g")],
  ["tiny-mem", minimal.replace("256m", "1000k")],
  ["mem-bytes", minimal.replace("256m", "536870912")],
  ["mem-unit-space", minimal.replace("256m", "' 1.5 GB '")],
  ["mem-bad", minimal.replace("256m", "abc")],
  ["many-cpus", minimal.replace("cpus: 0.5", "cpus: 64")],
  ["cpu-string", minimal.replace("cpus: 0.5", "cpus: ' 0.333 '")],
  ["cpu-true", minimal.replace("cpus: 0.5", "cpus: true")],
  ["cpu-hex", minimal.replace("cpus: 0.5", "cpus: '0x2'")],
  ["cpu-list", minimal.replace("cpus: 0.5", "cpus: ['1']")],
  ["image-upper", minimal.replace("nginx:1.29-alpine", "Nginx")],
  ["registry-lan", minimal.replace("nginx:1.29-alpine", "10.0.0.5:5000/evil")],
  ["registry-localhost", minimal.replace("nginx:1.29-alpine", "localhost/evil")],
  ["registry-index", minimal.replace("nginx:1.29-alpine", "index.docker.io/library/nginx")],
  ["proxy-env", withSvc("    environment:\n      HTTPS_PROXY: http://x")],
  ["host-socket", withVol("/var/run/docker.sock:/var/run/docker.sock")],
  ["host-relative", withVol("./x:/x")],
  ["over-etc", withVol("data:/etc/")],
  ["dotdot", withVol("data:/srv/../etc")],
  ["bind", withVol("{type: bind, source: /, target: /host}")],
  ["undeclared", withVol("other:/data")],
  ["no-storage", withVol("data:/data")],
  ["storage", withVol("data:/data:ro").replace("  web: app:80", "  web: app:80\n  storage_gb: 10")],
  ["mount-bad-mode", withVol("data:/data:rx")],
  ["egress", withCastor("  egress: [api.example.com, '*.example.org']")],
  ["egress-ip", withCastor("  egress: ['10.0.0.1']")],
  ["egress-port", withCastor("  egress: ['example.com:25']")],
  ["egress-star", withCastor("  egress: ['*']")],
  ["egress-many", withCastor(`  egress: [${Array.from({ length: 21 }, (_, i) => `h${i}.example.com`).join(", ")}]`)],
  ["storage-frac", withCastor("  storage_gb: 1.5")],
  ["storage-str", withCastor("  storage_gb: '20'")],
  ["storage-big", withCastor("  storage_gb: 3000")],
  ["castor-extra", withCastor("  ports: [80]")],
  ["web-port-only", minimal.replace("web: app:80", "web: 8080")],
  ["web-port-string", minimal.replace("web: app:80", "web: '8080'")],
  ["web-bad", minimal.replace("web: app:80", "web: app")],
  ["web-unknown", minimal.replace("web: app:80", "web: db:80")],
  ["web-zero", minimal.replace("web: app:80", "web: app:0")],
  ["web-big", minimal.replace("web: app:80", "web: app:99999")],
  ["no-castor", minimal.replace(/x-castor:\n  web: app:80\n/, "")],
  ["two-no-web", minimal.replace(/x-castor:\n  web: app:80\n/, "x-castor: {}\n").replace("services:\n", "services:\n  b: {image: nginx, mem_limit: 64m, cpus: 0.1}\n")],
  ["networks", `${minimal}networks:\n  x: {}\n`],
  ["top-secrets", `${minimal}secrets: {}\nconfigs: {}\nfoo: 1\n`],
  ["not-a-map", "- a\n- b\n"],
  ["no-services", "x-castor: {web: app:80}\n"],
  ["svc-not-map", "services:\n  app: x\nx-castor: {web: app:80}\n"],
  ["svc-names", minimal.replace("services:\n", "services:\n  pc-x: {}\n  castor-y: {}\n  castor: {}\n  bad-: {}\n  Bad: {}\n")],
  ["too-many", minimal.replace("services:\n", `services:\n${Array.from({ length: 8 }, (_, i) => `  s${i}: {image: nginx, mem_limit: 64m, cpus: 0.1}\n`).join("")}`)],
  ["vol-driver", `${minimal}volumes:\n  data: {driver: local}\n  Bad: null\n`],
  ["vol-not-map", `${minimal}volumes: [a]\n`],
  ["vol-shared", minimal.replace("services:\n", "services:\n  b: {image: nginx, mem_limit: 64m, cpus: 0.1, volumes: [data:/b]}\n").replace("    cpus: 0.5", "    cpus: 0.5\n    volumes: [data:/a]") + "volumes: {data: }\n"],
  ["numeric-keys", minimal.replace("x-castor:", "\"2\": 1\n\"1\": 1\nzz: 1\nx-castor:")],
  ["version-name", `version: "3.8"\nname: demo\n${minimal}`],
];

const docs = sources.map(([name, source]) => {
  const doc = parseYaml(source, { schema: "core", maxAliasCount: 20, uniqueKeys: true, prettyErrors: false });
  try {
    const spec = checkStack(doc);
    return { name, doc, spec, units: specUnits(spec), secrets: secretNames(spec) };
  } catch (err) {
    if (!(err instanceof SpecError)) throw err;
    return { name, doc, problems: err.problems };
  }
});

const images = [
  "nginx", "Nginx", "nginx:1.29-alpine", "library/nginx", "docker.io/nginx", "index.docker.io/library/nginx:1",
  "registry-1.docker.io/a/b", "ghcr.io/immich-app/immich-server:v2", `ghcr.io/a/b@${DIGEST}`, `ghcr.io/a/b:v1@${DIGEST}`,
  "nginx@sha256:1234", "evil.example/x", "localhost/x", "localhost:5000/x", "10.0.0.5:5000/evil", " quay.io/a/b:c ",
  "a/b/c/d/e/f", "a/b/c/d/e", "a__b/c--d.e_f", "a_/b", "a:" + "t".repeat(128), "a:" + "t".repeat(129), "a:-x", "a:", "",
  "x".repeat(256), "lscr.io/linuxserver/x", "mcr.microsoft.com/dotnet/aspnet:8.0", "gcr.io/x/y", "codeberg.org/a/b",
  "registry.gitlab.com/a/b/c", "docker.io/library/nginx:1@sha256:" + "A".repeat(64),
].map((input) => [input, parseImageRef(input)]);

const hub = parseImageRef("nginx");
const ghcr = parseImageRef("ghcr.io/immich-app/immich-server:v2");
if ("error" in hub || "error" in ghcr) throw new Error("unexpected");
const digests = [
  [hub, [`nginx@${DIGEST}`]], [hub, [`library/nginx@${DIGEST}`]], [hub, [`docker.io/library/nginx@${DIGEST}`]],
  [hub, ["nginx@sha256:12"]], [ghcr, [`ghcr.io/other/x@${DIGEST}`]], [ghcr, [`ghcr.io/immich-app/immich-server@${DIGEST}`]],
  [ghcr, [`immich-app/immich-server@${DIGEST}`]],
].map(([ref, list]) => ({ ref, list, digest: digestFor(ref as typeof hub, list as string[]) }));

const ctx = {
  slug: "muster", stack: "links", origin: "https://links-muster.castorapps.ch", dataDir: "/Volumes/castor/s/muster/links/data",
  images: { app: `docker.io/library/nginx:1.29-alpine@${DIGEST}`, db: `docker.io/library/postgres:17@${DIGEST}` },
  secrets: { db: "s3cr$t", api_key: "k" }, egressImage: "castor.local/egress:1",
};
const spec = (name: string) => {
  const d = docs.find((x) => x.name === name);
  if (!d || !("spec" in d)) throw new Error(name);
  return d.spec!;
};

const passwd = "root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1001::/home/node:/bin/sh\nwww-data:x:33:33::/var/www:/usr/sbin/nologin\nodd:x:1x:2\nnog:x:7:abc\n";
const group = "root:x:0:\nnode:x:1001:\nwww-data:x:33:\nstaff:x:50:\n";
const user = (u: string, files?: { passwd: string; group: string }) => {
  try {
    return resolveUser(u, files);
  } catch (err) {
    return { error: (err as Error).message };
  }
};
const s = (id: string, memoryUnits: number, cpuUnits = 1, storageUnits = 0) => ({ id, units: { memoryUnits, cpuUnits, storageUnits } });
const cover = (stacks: ReturnType<typeof s>[], booked: ReturnType<typeof s>["units"]) => {
  const r = coveredStacks(stacks, booked);
  return { stacks, booked, covered: r.covered.map((x) => x.id), over: r.over.map((x) => x.id) };
};

console.log(JSON.stringify({
  comment: "Decided by @pc/stacks and apps/provisioner/src/stacks.ts. Regenerate with gen-stacks.mts",
  digest: DIGEST,
  docs,
  images,
  pinned: pinned(ghcr, DIGEST),
  digests,
  memory: ["1.5GB", "abc", "512", "1k", "0", "0.0000001", " 2 g ", "1e3m"].map((m) => [m, parseMemory(m)]),
  render: [
    { name: "rendered", ctx, out: renderCompose(spec("rendered"), ctx) },
    { name: "minimal", ctx: { ...ctx, runtime: "runsc" }, out: renderCompose(spec("minimal"), { ...ctx, runtime: "runsc" }) },
    { name: "catalog-linkding", ctx, out: renderCompose(spec("catalog-linkding"), ctx) },
  ],
  names: [stackNames("muster", "links"), stackNames("a-b", "c")],
  node_route: nodeRoute({ slug: "muster", stack: "links", host: "links-muster.castorapps.ch", port: 9090 }),
  middlewares: nodeMiddlewares(["10.0.0.1", "10.0.0.2"]),
  control_plane: [
    controlPlaneRoutes({ slug: "muster", stacks: [{ stack: "links", host: "links-muster.castorapps.ch" }, { stack: "tools", host: "tools-muster.castorapps.ch" }], edgeUrl: "http://10.0.0.40:8480", stacksDomain: "castorapps.ch", certResolver: "le" }),
    controlPlaneRoutes({ slug: "muster", stacks: [{ stack: "links", host: "links-muster.castor.test" }], edgeUrl: "http://10.0.0.40:8480", stacksDomain: "castor.test" }),
  ],
  passwd,
  group,
  ids: Object.fromEntries(parseIds(passwd)),
  users: [
    ["", user("")], ["1000", user("1000")], ["1000:50", user("1000:50")], ["node", user("node", { passwd, group })],
    ["www-data:staff", user("www-data:staff", { passwd, group })], ["nobody", user("nobody", { passwd, group })],
    ["node:nogroup", user("node:nogroup", { passwd, group })], ["70000", user("70000")], ["1:70000", user("1:70000")],
    ["nog", user("nog", { passwd, group })], ["7", user("7", { passwd, group })], [":5", user(":5")], ["node", user("node")],
  ],
  uncovered: [
    [["/data", "/var/lib/x/"], ["/data", "/tmp"]], [["/srv/app/data"], ["/srv/app"]], [["/database"], ["/data"]],
    [["/x"], ["/"]], [["/"], ["/x"]], [["//a//"], ["/a"]],
  ].map(([declared, mounts]) => ({ declared, mounts, open: uncoveredVolumes(declared, mounts) })),
  // What the portal stores (and castord reads) for a definition.
  stored: [minimal, rendered, ...CATALOG.map((a) => a.source)].map((source) => storedDefinition(source)),
  cover: [
    cover([s("a", 2), s("b", 4), s("c", 1)], { memoryUnits: 3, cpuUnits: 3, storageUnits: 0 }),
    cover([s("a", 1, 1, 1), s("b", 1, 1, 1)], { memoryUnits: 9, cpuUnits: 9, storageUnits: 1 }),
    cover([], { memoryUnits: 0, cpuUnits: 0, storageUnits: 0 }),
  ],
}, null, 2));

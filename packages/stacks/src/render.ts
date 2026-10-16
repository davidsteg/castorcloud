// Turns a validated StackSpec into the compose file Docker runs, plus the
// Traefik routes in front of it. Every security setting is decided here, not
// by the customer:
//
//   - images pinned to the digest pulled at deploy
//   - all capabilities dropped except a short list; no-new-privileges
//   - memory, CPU and process limits on every container; logs capped
//   - volumes are bind mounts into the stack's own encrypted dataset only
//   - networks: <project> (internal, no way out), <project>-edge (internal;
//     only the web service and the node's edge proxy), <project>-out (only
//     the egress proxy, and only when the stack lists egress hosts)
//   - the gVisor runtime when the node has it
//
// Every customer string is escaped for Compose ($ -> $$), so nothing a
// customer writes is interpolated.
import { CASTOR_PLACEHOLDER_RE, SECRET_PLACEHOLDER_RE, type StackSpec } from "./spec.ts";

/** Capabilities a container keeps (Docker's default set minus raw sockets, mknod, chroot, setfcap, audit). */
export const KEPT_CAPABILITIES = ["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID", "NET_BIND_SERVICE", "KILL"];
export const PIDS_LIMIT = 512;
export const EGRESS_SERVICE = "castor-egress";
export const EGRESS_PORT = 3128;
/** Size of each tmpfs mount (charged to the container's memory limit). */
export const TMPFS_MB = 64;
/** Writable in-memory paths every container gets. */
export const DEFAULT_TMPFS = ["/tmp", "/run", "/var/tmp"];

/**
 * Names on the node. "--" separates slug and stack: slugs never contain it and
 * stack names have no hyphen at all, so no tenant can produce another
 * tenant's project, network or router name (slug "a-b" + stack "c" vs slug
 * "a" + stack "b-c" can't both exist, and "-edge" can't be faked by a stack
 * called "edge").
 */
export function stackNames(slug: string, stack: string) {
  const project = `pc-s-${slug}--${stack}`;
  return {
    project,
    appNetwork: project,
    edgeNetwork: `${project}-edge`,
    outNetwork: `${project}-out`,
    /**
     * The web service's name on the edge network, where the node's proxy finds
     * it. It has a dot, which service names can't: no stack can answer to it.
     */
    webAlias: `web.${project}`,
    /** Authentik application/provider slug. */
    authSlug: `stack-${slug}--${stack}`,
  };
}

/** Edge networks of stacks (prepareStacksNode reconnects the node's proxy to these). */
export const EDGE_NETWORK_RE = /^pc-s-[a-z0-9]+(?:-[a-z0-9]+)*--[a-z0-9]+-edge$/;

export const stackHost = (slug: string, stack: string, stacksDomain: string) => `${stack}-${slug}.${stacksDomain}`;

export type RenderContext = {
  slug: string;
  stack: string;
  /** https://<stack>-<slug>.<stacks domain> */
  origin: string;
  /** Where the stack's volumes live on the node: <dataDir>/<volume>. */
  dataDir: string;
  /** Service -> image pinned to a digest. */
  images: Record<string, string>;
  /** Generated values for ${secret:name}. */
  secrets: Record<string, string>;
  /** "runsc" (gVisor) when the node has it. */
  runtime?: string;
  /** Image of the egress proxy (infra/node/egress), needed when the stack lists egress hosts. */
  egressImage: string;
};

const esc = (s: string) => s.replace(/\$/g, "$$$$");

function fillPlaceholders(value: string, ctx: RenderContext): string {
  return value
    .replace(SECRET_PLACEHOLDER_RE, (_, name: string) => {
      const v = ctx.secrets[name];
      if (!v) throw new Error(`secret ${name} missing`);
      return v;
    })
    .replace(CASTOR_PLACEHOLDER_RE, (_, what: string) => (what === "origin" ? ctx.origin : new URL(ctx.origin).host));
}

/** Strings outside environment values are taken literally (no placeholders). */
const literal = (v: string | string[] | undefined) => (v === undefined ? undefined : Array.isArray(v) ? v.map(esc) : esc(v));

const hardening = (memoryMb: number, cpus: number, runtime?: string) => ({
  cap_drop: ["ALL"],
  cap_add: KEPT_CAPABILITIES,
  security_opt: ["no-new-privileges:true"],
  mem_limit: `${memoryMb}m`,
  memswap_limit: `${memoryMb}m`,
  cpus,
  pids_limit: PIDS_LIMIT,
  restart: "unless-stopped",
  logging: { driver: "json-file", options: { "max-size": "10m", "max-file": "3" } },
  ...(runtime ? { runtime } : {}),
});

/** The compose file for one stack, as an object (written as JSON, which Compose reads as YAML). */
export function renderCompose(spec: StackSpec, ctx: RenderContext) {
  const n = stackNames(ctx.slug, ctx.stack);
  const withEgress = spec.egress.length > 0;
  const proxy = `http://${EGRESS_SERVICE}:${EGRESS_PORT}`;
  const serviceNames = Object.keys(spec.services);
  const services: Record<string, unknown> = {};

  for (const [name, svc] of Object.entries(spec.services)) {
    const image = ctx.images[name];
    if (!image || !/@sha256:[a-f0-9]{64}$/.test(image)) throw new Error(`${name}: image not pinned to a digest`);
    const environment: Record<string, string> = {};
    for (const [k, v] of Object.entries(svc.environment)) environment[k] = esc(fillPlaceholders(v, ctx));
    if (withEgress) {
      const noProxy = ["localhost", "127.0.0.1", ...serviceNames].join(",");
      Object.assign(environment, { HTTP_PROXY: proxy, HTTPS_PROXY: proxy, http_proxy: proxy, https_proxy: proxy, NO_PROXY: noProxy, no_proxy: noProxy });
    }
    const isWeb = spec.web.service === name;
    services[name] = {
      image,
      ...(svc.command !== undefined ? { command: literal(svc.command) } : {}),
      ...(svc.entrypoint !== undefined ? { entrypoint: literal(svc.entrypoint) } : {}),
      environment,
      ...(svc.workingDir ? { working_dir: esc(svc.workingDir) } : {}),
      ...(svc.user ? { user: svc.user } : {}),
      // Always: the writable layer is outside every quota. Writes go to the
      // stack's volumes (its dataset) or to tmpfs, which counts as memory.
      read_only: true,
      tmpfs: [...new Set([...DEFAULT_TMPFS, ...svc.tmpfs])].map((p) => `${p}:size=${TMPFS_MB}m`),
      volumes: svc.volumes.map((m) => ({
        type: "bind",
        source: `${ctx.dataDir}/${m.volume}`,
        target: m.target,
        read_only: m.readOnly,
        // The directory is created by castor-zfs inside the dataset; never let Docker create one elsewhere.
        bind: { create_host_path: false },
      })),
      ...hardening(svc.memoryMb, svc.cpus, ctx.runtime),
      ...(svc.healthcheck ? {
        healthcheck: {
          test: svc.healthcheck.test.map(esc),
          interval: `${svc.healthcheck.intervalS}s`,
          timeout: `${svc.healthcheck.timeoutS}s`,
          retries: svc.healthcheck.retries,
          start_period: `${svc.healthcheck.startPeriodS}s`,
        },
      } : {}),
      ...(svc.dependsOn.length ? { depends_on: svc.dependsOn } : {}),
      networks: {
        app: {},
        ...(isWeb ? { edge: { aliases: [n.webAlias] } } : {}),
      },
      labels: { "ch.castorcloud.stack": `${ctx.slug}/${ctx.stack}` },
    };
  }

  if (withEgress) {
    services[EGRESS_SERVICE] = {
      image: ctx.egressImage,
      environment: { EGRESS_ALLOW: spec.egress.join(","), EGRESS_PORT: String(EGRESS_PORT) },
      user: "65534:65534",
      read_only: true,
      ...hardening(96, 0.25, ctx.runtime),
      cap_add: [],
      networks: { app: {}, out: {} },
      labels: { "ch.castorcloud.stack": `${ctx.slug}/${ctx.stack}` },
    };
  }

  return {
    name: n.project,
    services,
    networks: {
      app: { name: n.appNetwork, internal: true },
      edge: { name: n.edgeNetwork, internal: true },
      ...(withEgress ? { out: { name: n.outNetwork } } : {}),
    },
  };
}

/** Traefik route on the stack node: control plane -> the stack's web service. */
export function nodeRoute(opts: { slug: string; stack: string; host: string; port: number }): string {
  const n = stackNames(opts.slug, opts.stack);
  return [
    `# Generated by the provisioner: stack ${opts.stack} of ${opts.slug}.`,
    "http:",
    "  routers:",
    `    ${n.project}:`,
    `      rule: 'Host(\`${opts.host}\`)'`,
    "      entryPoints: [web]",
    // Only the control plane may call it: it did the sign-in check.
    "      middlewares: [castor-control-plane]",
    `      service: ${n.project}`,
    "  services:",
    `    ${n.project}:`,
    "      loadBalancer:",
    "        passHostHeader: true",
    "        servers:",
    `          - url: "http://${n.webAlias}:${opts.port}"`,
    "",
  ].join("\n");
}

/** The node's allowlist middleware (written once per node, see stacks-edge.yml). */
export function nodeMiddlewares(controlPlaneIps: string[]): string {
  if (!controlPlaneIps.length) throw new Error("no control-plane address for the stack node");
  return [
    "# Generated by the provisioner: requests must come from the control plane.",
    "http:",
    "  middlewares:",
    "    castor-control-plane:",
    "      ipAllowList:",
    `        sourceRange: [${controlPlaneIps.map((ip) => `"${ip}"`).join(", ")}]`,
    "",
  ].join("\n");
}

/**
 * Control-plane Traefik routes for a tenant's stacks: TLS, then Authentik's
 * forward auth (only the tenant's members get through), then the stack node.
 * Authentik's outpost paths on each host go to Authentik itself.
 */
export function controlPlaneRoutes(opts: {
  slug: string;
  stacks: { stack: string; host: string }[];
  edgeUrl: string;
  /** Production: the ACME resolver that holds the stacks domain's wildcard. */
  certResolver?: string;
  stacksDomain: string;
}): string {
  const tls = opts.certResolver
    ? [
        "      tls:",
        `        certResolver: ${opts.certResolver}`,
        "        domains:",
        `          - main: '${opts.stacksDomain}'`,
        `            sans: ['*.${opts.stacksDomain}']`,
      ]
    : [];
  const service = `pc-s-${opts.slug}--node`;
  const lines = [`# Generated by the provisioner: ${opts.slug}'s custom stacks, on the node at ${opts.edgeUrl}.`, "http:", "  routers:"];
  for (const s of opts.stacks) {
    const n = stackNames(opts.slug, s.stack);
    lines.push(
      `    ${n.project}:`,
      `      rule: 'Host(\`${s.host}\`)'`,
      "      entryPoints: [websecure]",
      "      middlewares: [castor-stack-strip@file, castor-stack-auth@file]",
      `      service: ${service}`,
      ...tls,
      `    ${n.project}-outpost:`,
      `      rule: 'Host(\`${s.host}\`) && PathPrefix(\`/outpost.goauthentik.io/\`)'`,
      "      entryPoints: [websecure]",
      "      priority: 10000",
      "      service: authentik@docker",
      ...tls,
    );
  }
  lines.push(
    "  services:",
    `    ${service}:`,
    "      loadBalancer:",
    "        passHostHeader: true",
    "        servers:",
    `          - url: "${opts.edgeUrl}"`,
    "",
  );
  return lines.join("\n");
}

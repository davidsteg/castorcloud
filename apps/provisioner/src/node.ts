// A data-plane node: where tenant stacks run.
//   LocalNode - the Docker host the provisioner runs on (development,
//               single-server setups); LUKS volumes via pc-cryptd.
//   SshNode   - a Mac mini: Docker (Colima) driven over ssh://, tenant data
//               on encrypted ZFS datasets via castor-zfs (infra/node/macos).
import { execFileSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import type { TenantStorage } from "./storage.ts";
import { TenantVolume } from "./volume.ts";
import { ZfsDataset, type HostExec } from "./zfs.ts";
import { nodeLayout, parseEdge, type RemoteKind } from "./layout.ts";
import { installImage } from "./images.ts";

export interface NodeExec {
  readonly name: string;
  /** Runs docker with args; `input` is passed on stdin (used for keys). */
  docker(args: string[], opts?: DockerOpts): string;
  /**
   * Tenant files read by the docker CLI (env file, compose override) live on
   * the control plane; files the containers bind-mount live on the node.
   */
  configDir(slug: string): string;
  writeConfig(slug: string, file: string, content: string): string;
  /** Writes a file tenant containers mount (owner-only unless `mode` is 0o644); returns its path on the node. */
  writeNodeFile(slug: string, file: string, content: string, mode?: 0o600 | 0o644): string;
  /** Removes a directory below the tenant's node files (writeNodeFile). */
  removeNodeFiles(slug: string, dir: string): void;
  /** TENANT_DIR in the stack: where writeNodeFile puts files. */
  nodeConfigDir(slug: string): string;
  removeConfig(slug: string): void;
  /** Node preparation before stacks are started (idempotent). */
  prepare(): void;
  readonly paths: {
    /** Compose files, read by the docker CLI on the control plane. */
    templates: string;
    /** The same templates on the node: relative bind mounts resolve here. */
    nodeTemplates: string;
    /** infra/node (shared services, LLM gateway config) on the control plane and on the node. */
    infra: string;
    nodeInfra: string;
    brand: string;
    ca: string;
  };
  /** Overrides the platform's AUTH_HOST_IP for stacks on this node. */
  readonly authHostIp?: string;
  /**
   * Where the control plane's Traefik reaches the node's edge proxy. Unset for
   * the local node, whose stacks sit on the control plane Traefik's network.
   */
  readonly edgeUrl?: string;
  /** LLM_GATEWAY_BIND for the node's shared services: where the gateway's admin API listens. */
  readonly gatewayBind?: string;
  storage(tenantId: string, slug: string, wrappedKey: string): TenantStorage;
}

type DockerOpts = { input?: Buffer; allowFail?: boolean; env?: NodeJS.ProcessEnv };

const posix = (p: string) => p.replace(/\\/g, "/");

function run(cmd: string, args: string[], opts: { input?: Buffer; allowFail?: boolean; env?: NodeJS.ProcessEnv } = {}) {
  try {
    return execFileSync(cmd, args, {
      input: opts.input,
      stdio: ["pipe", "pipe", "pipe"],
      encoding: "utf8",
      // Git Bash / MSYS must not rewrite /srv/... into Windows paths.
      env: { ...process.env, MSYS_NO_PATHCONV: "1", ...opts.env },
      maxBuffer: 32 * 1024 * 1024,
    });
  } catch (err) {
    if (opts.allowFail) return "";
    const e = err as { stderr?: string; message?: string };
    throw new Error(`${cmd} ${args.slice(0, 4).join(" ")} … failed: ${(e.stderr || e.message || "").trim().slice(-2000)}`);
  }
}

/** Tenant files the docker CLI reads, on the control plane (gitignored). */
class LocalConfig {
  constructor(private readonly root: string) {}

  dir(slug: string) {
    return posix(path.join(this.root, ".tenants", slug));
  }

  write(slug: string, file: string, content: string, mode = 0o600) {
    const target = path.join(this.dir(slug), file);
    mkdirSync(path.dirname(target), { recursive: true });
    writeFileSync(target, content, { mode });
    chmodSync(target, mode); // writeFileSync keeps the mode of an existing file
    return posix(target);
  }

  remove(slug: string, dir = "") {
    rmSync(path.join(this.dir(slug), dir), { recursive: true, force: true });
  }
}

export class LocalNode implements NodeExec {
  readonly name = "local";
  readonly paths: NodeExec["paths"];
  private readonly config: LocalConfig;

  constructor(private readonly root: string) {
    this.config = new LocalConfig(root);
    const templates = path.join(root, "packages/stack-templates");
    this.paths = {
      templates,
      nodeTemplates: posix(templates),
      infra: path.join(root, "infra/node"),
      nodeInfra: posix(path.join(root, "infra/node")),
      brand: posix(path.join(root, "packages/brand")),
      ca: posix(path.join(root, ".dev-certs")),
    };
  }

  docker(args: string[], opts: DockerOpts = {}) {
    return run("docker", args, opts);
  }

  configDir(slug: string) {
    return this.config.dir(slug);
  }

  writeConfig(slug: string, file: string, content: string) {
    return this.config.write(slug, file, content);
  }

  writeNodeFile(slug: string, file: string, content: string, mode: 0o600 | 0o644 = 0o600) {
    return this.config.write(slug, file, content, mode);
  }

  removeNodeFiles(slug: string, dir: string) {
    this.config.remove(slug, dir);
  }

  nodeConfigDir(slug: string) {
    return this.config.dir(slug);
  }

  removeConfig(slug: string) {
    this.config.remove(slug);
  }

  storage(tenantId: string, slug: string, wrappedKey: string) {
    return new TenantVolume(this, tenantId, slug, wrappedKey);
  }

  prepare() {
    // /srv/pc must be a shared mount so volumes opened by the cryptd helper
    // are visible to tenant containers. On production nodes this is part of
    // the node bootstrap; Docker Desktop's VM loses it on restart.
    this.docker([
      "run", "--rm", "--privileged", "--pid=host", "alpine:3.22", "nsenter", "-t", "1", "-m", "--", "sh", "-c",
      "mkdir -p /srv/pc/volumes /srv/pc/tenants && (findmnt -no PROPAGATION /srv/pc | grep -q shared || (mount --bind /srv/pc /srv/pc && mount --make-rshared /srv/pc))",
    ]);
    installImage(this, "cryptd", CRYPTD_IMAGE, [path.join(this.root, "infra/node/cryptd")]);
    installAssistantTools(this, this.root);
  }
}

export const CRYPTD_IMAGE = "pc-cryptd:1";
/** Same tag as ASSISTANT_TOOLS_IMAGE in packages/stack-templates/versions.env. */
export const ASSISTANT_TOOLS_IMAGE = "pc-assistant-tools:1";

/** The assistant's tools server, on every node (pulled and verified, or built; see images.ts). */
function installAssistantTools(node: NodeExec, root: string) {
  installImage(node, "assistant-tools", ASSISTANT_TOOLS_IMAGE, ["-f", path.join(root, "agent/tools/Dockerfile"), root]);
}

// Words sent over SSH: castor-ssh splits on spaces and allows only these.
const SSH_WORD = /^[A-Za-z0-9._/=-]+$/;

export type SshNodeConfig = {
  name: string;
  kind: RemoteKind;
  /** user@host, as in the provisioner's ~/.ssh/config */
  target: string;
  pool: string;
  authHostIp?: string;
  /**
   * host[:port] on the LAN where the node's edge proxy listens: the Colima
   * VM's address on a Mac (port 80), the NAS's address on TrueNAS (port 8480,
   * since the TrueNAS UI has 80 and 443).
   */
  edgeHost?: string;
};

/**
 * A remote node: TrueNAS SCALE (native ZFS and Docker) or a Mac mini (OpenZFS
 * on macOS, Docker in Colima). Every call goes over SSH to the castor-ssh
 * gate: the Docker CLI's ssh:// transport for docker/compose, and castor-*
 * commands for datasets and config files. Both platforms behave the same;
 * only the paths differ (nodeLayout). The provisioner's key needs an entry in
 * ~/.ssh/config (or the agent).
 */
export class SshNode implements NodeExec, HostExec {
  readonly name: string;
  readonly paths: NodeExec["paths"];
  readonly authHostIp?: string;
  readonly edgeUrl?: string;
  readonly gatewayBind?: string;
  private readonly target: string;
  private readonly layout: ReturnType<typeof nodeLayout>;
  private readonly edgePort: number;
  private readonly config: LocalConfig;

  constructor(private readonly root: string, cfg: SshNodeConfig) {
    this.config = new LocalConfig(root);
    if (!/^[A-Za-z0-9._-]+@[A-Za-z0-9.-]+$/.test(cfg.target)) throw new Error(`invalid ssh target: ${cfg.target}`);
    if (!/^[A-Za-z0-9_-]+$/.test(cfg.pool)) throw new Error(`invalid pool name: ${cfg.pool}`);
    this.name = cfg.name;
    this.target = cfg.target;
    this.layout = nodeLayout(cfg.kind, cfg.pool);
    this.authHostIp = cfg.authHostIp;
    const edge = cfg.edgeHost === undefined ? undefined : parseEdge(cfg.edgeHost);
    this.edgePort = edge?.port ?? 80;
    this.edgeUrl = edge?.url;
    // The gateway's admin API listens on the node's LAN address, next to the edge proxy.
    this.gatewayBind = edge?.host;
    this.paths = {
      templates: path.join(root, "packages/stack-templates"),
      nodeTemplates: `${this.layout.root}/files/templates`,
      infra: path.join(root, "infra/node"),
      nodeInfra: `${this.layout.root}/files/node`,
      brand: `${this.layout.root}/files/brand`,
      ca: `${this.layout.root}/files/ca`,
    };
  }

  docker(args: string[], opts: DockerOpts = {}) {
    return run("docker", args, { ...opts, env: { ...opts.env, DOCKER_HOST: `ssh://${this.target}`, DOCKER_CONTEXT: "" } });
  }

  host(args: string[], opts: { input?: Buffer; allowFail?: boolean } = {}) {
    for (const a of args) if (!SSH_WORD.test(a)) throw new Error(`argument not allowed over ssh: ${a}`);
    return run("ssh", ["-o", "BatchMode=yes", "-T", this.target, ...args], opts);
  }

  configDir(slug: string) {
    return this.config.dir(slug);
  }

  /** Stays on the control plane: tenant.env holds every secret of the stack. */
  writeConfig(slug: string, file: string, content: string) {
    return this.config.write(slug, file, content);
  }

  writeNodeFile(slug: string, file: string, content: string, mode: 0o600 | 0o644 = 0o600) {
    this.host(["castor-put", `tenants/${slug}/${file}`, ...(mode === 0o644 ? ["644"] : [])], { input: Buffer.from(content) });
    return `${this.nodeConfigDir(slug)}/${file}`;
  }

  removeNodeFiles(slug: string, dir: string) {
    this.host(["castor-rm", `tenants/${slug}/${dir}`]);
  }

  nodeConfigDir(slug: string) {
    return `${this.layout.root}/tenants/${slug}`;
  }

  removeConfig(slug: string) {
    this.host(["castor-rm", `tenants/${slug}`]);
    this.config.remove(slug);
  }

  storage(tenantId: string, slug: string, wrappedKey: string) {
    return new ZfsDataset(this, tenantId, slug, wrappedKey, this.layout.mountRoot);
  }

  /** Pool size/usage/health and host facts, for registration and health checks. */
  probe() {
    const pool = this.host(["castor-zfs", "pool"]).trim().split(/\s+/);
    const info = this.host(["castor-info"]).trim().split(/\s+/);
    if (pool[0] !== "pool" || info[0] !== "info") throw new Error(`unexpected probe output from ${this.name}`);
    return {
      poolSizeMb: Number(pool[1]),
      poolUsedMb: Number(pool[2]),
      poolHealth: pool[3],
      osVersion: info[1],
      memoryMb: Number(info[2]),
      cpus: Number(info[3]),
      dockerVersion: this.docker(["version", "--format", "{{.Server.Version}}"]).trim(),
    };
  }

  /**
   * Copies templates, brand assets and the CA bundle to the node (tenant
   * containers bind-mount them), then starts the node's edge Traefik.
   * Only ca-bundle.pem leaves the control plane, never a CA key.
   */
  prepare() {
    const caDir = process.env.CA_DIR ?? path.join(this.root, ".dev-certs");
    const sync = (name: "templates" | "node" | "brand" | "ca", dir: string, files = ["."]) => {
      if (!existsSync(dir)) throw new Error(`${dir} missing`);
      const tar = execFileSync("tar", ["-C", dir, "-cf", "-", ...files], { maxBuffer: 256 * 1024 * 1024 });
      this.host(["castor-untar", name], { input: tar });
    };
    sync("templates", this.paths.templates);
    // Relative bind mounts of the shared services (the LLM gateway's config).
    sync("node", this.paths.infra, ["compose.yml", "litellm"]);
    // What the skin proxy serves under /_brand/ (not the design sources).
    sync("brand", path.join(this.root, "packages/brand"), ["tokens.css", "theme.css", "bridge.js", "logo.svg", "icon.svg", "services"]);
    sync("ca", caDir, ["ca-bundle.pem"]);

    this.docker(["network", "inspect", "pc-edge"], { allowFail: true }) ||
      this.docker(["network", "create", "pc-edge"]);
    this.docker([
      "compose", "-p", "pc-edge",
      "-f", path.join(this.root, "infra/node/edge.yml"),
      "up", "-d", "--remove-orphans",
    ], { env: { EDGE_PORT: String(this.edgePort) } });
    installAssistantTools(this, this.root);
  }
}

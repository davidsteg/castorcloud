// What a custom stack is allowed to be. Customers describe a stack as a
// small subset of Docker Compose (docs/stacks.md); this file parses it into a
// StackSpec and refuses everything outside that subset. The provisioner
// re-validates the stored spec and renders the actual compose file itself
// (render.ts), so nothing the customer writes reaches Docker unchecked.
//
// The rule is an allowlist: a key we don't know is an error, never ignored,
// so a new Compose feature can't slip through.
import { parse as parseYaml } from "yaml";
import { stackUnits, type StackUnits } from "@pc/pricing";
import { parseImageRef } from "./image.ts";

export type HealthSpec = { test: string[]; intervalS: number; timeoutS: number; retries: number; startPeriodS: number };

export type MountSpec = { volume: string; target: string; readOnly: boolean };

export type ServiceSpec = {
  /** Image reference as written (tag and/or digest); pinned to a digest at deploy. */
  image: string;
  command?: string | string[];
  entrypoint?: string | string[];
  environment: Record<string, string>;
  workingDir?: string;
  /** "uid", "uid:gid", "name" or "name:group" inside the image; default: the image's user. */
  user?: string;
  /** Extra writable in-memory paths (the root filesystem is always read-only). */
  tmpfs: string[];
  volumes: MountSpec[];
  memoryMb: number;
  cpus: number;
  healthcheck?: HealthSpec;
  dependsOn: string[];
};

export type StackSpec = {
  services: Record<string, ServiceSpec>;
  volumes: string[];
  /** The one HTTP port the stack serves, reached at https://<stack>-<slug>.<stacks domain>. */
  web: { service: string; port: number };
  /** Hostnames the stack may reach (HTTP/HTTPS through the egress proxy); "*.example.com" allowed. */
  egress: string[];
  /** Quota of the stack's dataset. 0 = no persistent storage. */
  storageGb: number;
};

export class SpecError extends Error {
  readonly problems: string[];
  constructor(problems: string[]) {
    super(problems.join("; "));
    this.problems = problems;
  }
}

// ------------------------------------------------------------------ limits

export const LIMITS = {
  sourceBytes: 64 * 1024,
  services: 8,
  volumes: 8,
  envVars: 100,
  envValueBytes: 4096,
  argBytes: 4096,
  args: 64,
  egressHosts: 20,
  tmpfs: 4,
  memoryMb: { min: 64, max: 16384 },
  cpus: { min: 0.05, max: 8 },
  storageGb: { min: 0, max: 2000 },
} as const;

export const STACK_NAME_RE = /^[a-z][a-z0-9]{1,19}$/;
const NAME_RE = /^[a-z][a-z0-9-]{0,29}$/;
const ENV_KEY_RE = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
/** Absolute container path; no "..", no empty segments, no odd characters. */
const PATH_RE = /^\/(?:[A-Za-z0-9._@+-]+\/?)*$/;
const USER_RE = /^(?:[0-9]{1,10}|[a-z_][a-z0-9_-]{0,31})(?::(?:[0-9]{1,10}|[a-z_][a-z0-9_-]{0,31}))?$/;
const HOST_RE = /^(?:\*\.)?(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/;

/**
 * Stack names become host labels (<stack>-<slug>) and project names. No
 * hyphen, so "<stack>-<slug>" always splits at the first one. These are
 * refused because they would read like our own service hosts.
 */
export const RESERVED_STACK_NAMES = new Set([
  "photos", "docs", "places", "flights", "assistant", "auth", "app", "www", "api", "mail", "admin", "castor", "portal", "status",
]);

/** Names we use inside every stack (the egress proxy). */
const RESERVED_SERVICE_NAMES = new Set(["castor-egress", "castor"]);
/** Our own name prefixes on the node: a service must never look like one. */
const RESERVED_SERVICE_PREFIXES = ["pc-", "castor-"];

/** Set by the renderer; a stack can't override them (e.g. to point at another proxy). */
export const RESERVED_ENV = new Set(["HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "no_proxy", "ALL_PROXY", "all_proxy"]);

/** Container paths a volume may not cover (would shadow the runtime). */
const FORBIDDEN_TARGETS = ["/", "/proc", "/sys", "/dev", "/etc", "/bin", "/sbin", "/usr", "/lib", "/lib64"];

/** `${secret:name}` in an environment value: a random value generated once per stack. */
export const SECRET_PLACEHOLDER_RE = /\$\{secret:([a-z0-9_]{1,32})\}/g;
/** `${castor:origin}` / `${castor:host}`: where the stack is reached. */
export const CASTOR_PLACEHOLDER_RE = /\$\{castor:(origin|host)\}/g;

// ------------------------------------------------------------------ parse

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v);

class Collector {
  readonly problems: string[] = [];
  add(where: string, what: string) {
    if (this.problems.length < 50) this.problems.push(`${where}: ${what}`);
  }
}

function onlyKeys(c: Collector, where: string, obj: Obj, allowed: string[], hints: Record<string, string> = {}) {
  for (const k of Object.keys(obj)) {
    if (!allowed.includes(k)) c.add(where, hints[k] ?? `"${k}" is not supported`);
  }
}

/** "512m", "1g", "1.5GB", 536870912 -> MB (rounded up). */
export function parseMemory(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v) && v > 0) return Math.ceil(v / (1024 * 1024));
  if (typeof v !== "string") return null;
  const m = v.trim().toLowerCase().match(/^(\d+(?:\.\d+)?)\s*(b|k|kb|m|mb|g|gb)?$/);
  if (!m) return null;
  const n = Number(m[1]);
  const unit = (m[2] ?? "b")[0];
  const mb = unit === "g" ? n * 1024 : unit === "m" ? n : unit === "k" ? n / 1024 : n / (1024 * 1024);
  return mb > 0 ? Math.ceil(mb) : null;
}

/** "10s", "1m30s", 5 -> seconds. */
function parseDuration(v: unknown): number | null {
  if (typeof v === "number" && Number.isInteger(v) && v >= 0) return v;
  if (typeof v !== "string") return null;
  const m = v.trim().match(/^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/);
  if (!m || !v.trim()) return null;
  return Number(m[1] ?? 0) * 3600 + Number(m[2] ?? 0) * 60 + Number(m[3] ?? 0);
}

function stringOrArgs(c: Collector, where: string, v: unknown): string | string[] | undefined {
  if (v === undefined || v === null) return undefined;
  if (typeof v === "string") {
    if (Buffer.byteLength(v) > LIMITS.argBytes) c.add(where, "too long");
    return v;
  }
  if (Array.isArray(v) && v.length <= LIMITS.args && v.every((a) => typeof a === "string" || typeof a === "number")) {
    const args = v.map(String);
    if (args.some((a) => Buffer.byteLength(a) > LIMITS.argBytes)) c.add(where, "argument too long");
    return args;
  }
  c.add(where, "must be a string or a list of strings");
  return undefined;
}

function noControlChars(c: Collector, where: string, s: string) {
  // NUL and other control characters (except tab/newline) have no business in config.
  if (/[\u0000-\u0008\u000b-\u001f\u007f]/.test(s)) c.add(where, "contains control characters");
}

function parseEnvironment(c: Collector, where: string, v: unknown): Record<string, string> {
  const out: Record<string, string> = {};
  const set = (key: string, value: unknown) => {
    if (!ENV_KEY_RE.test(key)) return c.add(where, `invalid variable name "${key.slice(0, 40)}"`);
    if (RESERVED_ENV.has(key)) return c.add(where, `${key} is set by Castor Cloud (egress proxy)`);
    if (value === null || value === undefined) return c.add(where, `${key} needs a value`);
    if (typeof value === "object") return c.add(where, `${key} must be a string`);
    const s = String(value);
    if (Buffer.byteLength(s) > LIMITS.envValueBytes) return c.add(where, `${key} is too long`);
    noControlChars(c, `${where}.${key}`, s);
    out[key] = s;
  };
  if (v === undefined || v === null) return out;
  if (Array.isArray(v)) {
    for (const item of v) {
      if (typeof item !== "string" || !item.includes("=")) { c.add(where, "list entries must be KEY=value"); continue; }
      const i = item.indexOf("=");
      set(item.slice(0, i), item.slice(i + 1));
    }
  } else if (isObj(v)) {
    for (const [k, val] of Object.entries(v)) set(k, val);
  } else {
    c.add(where, "must be a map or a list");
  }
  if (Object.keys(out).length > LIMITS.envVars) c.add(where, `at most ${LIMITS.envVars} variables`);
  return out;
}

function checkTarget(c: Collector, where: string, target: string) {
  if (!PATH_RE.test(target) || target.split("/").some((s) => s === "." || s === "..")) {
    c.add(where, `invalid container path "${target.slice(0, 80)}"`);
    return;
  }
  const t = target.replace(/\/+$/, "") || "/";
  if (FORBIDDEN_TARGETS.includes(t)) c.add(where, `can't mount over ${t}`);
}

function parseMounts(c: Collector, where: string, v: unknown): MountSpec[] {
  if (v === undefined || v === null) return [];
  if (!Array.isArray(v)) { c.add(where, "must be a list"); return []; }
  const out: MountSpec[] = [];
  for (const item of v) {
    let volume: string | undefined, target: string | undefined, readOnly = false;
    if (typeof item === "string") {
      const parts = item.split(":");
      if (parts.length < 2 || parts.length > 3 || (parts[2] !== undefined && parts[2] !== "ro" && parts[2] !== "rw")) {
        c.add(where, `"${item.slice(0, 80)}": use volume-name:/path[:ro]`);
        continue;
      }
      [volume, target] = parts;
      readOnly = parts[2] === "ro";
    } else if (isObj(item)) {
      onlyKeys(c, where, item, ["type", "source", "target", "read_only"]);
      if (item.type !== undefined && item.type !== "volume") { c.add(where, `type "${String(item.type)}" is not supported (only named volumes)`); continue; }
      volume = typeof item.source === "string" ? item.source : undefined;
      target = typeof item.target === "string" ? item.target : undefined;
      readOnly = item.read_only === true;
    }
    if (!volume || !target) { c.add(where, "each entry needs a volume name and a container path"); continue; }
    if (volume.startsWith("/") || volume.startsWith(".") || volume.startsWith("~")) {
      c.add(where, `"${volume.slice(0, 80)}": host paths are not allowed, use a named volume`);
      continue;
    }
    if (!NAME_RE.test(volume)) { c.add(where, `invalid volume name "${volume.slice(0, 40)}"`); continue; }
    checkTarget(c, where, target);
    out.push({ volume, target, readOnly });
  }
  return out;
}

function parseHealth(c: Collector, where: string, v: unknown): HealthSpec | undefined {
  if (v === undefined || v === null) return undefined;
  if (!isObj(v)) { c.add(where, "must be a map"); return undefined; }
  onlyKeys(c, where, v, ["test", "interval", "timeout", "retries", "start_period"]);
  let test: string[] = [];
  if (typeof v.test === "string") test = ["CMD-SHELL", v.test];
  else if (Array.isArray(v.test) && v.test.every((a) => typeof a === "string") && ["CMD", "CMD-SHELL"].includes(v.test[0] as string)) {
    test = v.test as string[];
  } else c.add(where, "test must be a string or [CMD|CMD-SHELL, ...]");
  if (test.some((a) => Buffer.byteLength(a) > LIMITS.argBytes) || test.length > LIMITS.args) c.add(where, "test too long");
  const dur = (key: string, def: number, min: number, max: number) => {
    if (v[key] === undefined) return def;
    const s = parseDuration(v[key]);
    if (s === null || s < min || s > max) { c.add(where, `${key} must be between ${min}s and ${max}s`); return def; }
    return s;
  };
  const retries = v.retries === undefined ? 3 : Number(v.retries);
  if (!Number.isInteger(retries) || retries < 1 || retries > 20) c.add(where, "retries must be 1-20");
  return {
    test,
    intervalS: dur("interval", 30, 5, 3600),
    timeoutS: dur("timeout", 10, 1, 300),
    retries: Number.isInteger(retries) ? retries : 3,
    startPeriodS: dur("start_period", 0, 0, 3600),
  };
}

function parseResources(c: Collector, where: string, svc: Obj): { memoryMb: number; cpus: number } {
  let mem: unknown = svc.mem_limit;
  let cpus: unknown = svc.cpus;
  if (svc.deploy !== undefined) {
    if (!isObj(svc.deploy)) c.add(`${where}.deploy`, "must be a map");
    else {
      onlyKeys(c, `${where}.deploy`, svc.deploy, ["resources"]);
      const res = svc.deploy.resources;
      if (res !== undefined) {
        if (!isObj(res)) c.add(`${where}.deploy.resources`, "must be a map");
        else {
          onlyKeys(c, `${where}.deploy.resources`, res, ["limits"]);
          if (isObj(res.limits)) {
            onlyKeys(c, `${where}.deploy.resources.limits`, res.limits, ["memory", "cpus"]);
            mem ??= res.limits.memory;
            cpus ??= res.limits.cpus;
          }
        }
      }
    }
  }
  const memoryMb = mem === undefined ? null : parseMemory(mem);
  if (memoryMb === null) c.add(where, "needs a memory limit (mem_limit: 512m)");
  else if (memoryMb < LIMITS.memoryMb.min || memoryMb > LIMITS.memoryMb.max) {
    c.add(where, `memory must be between ${LIMITS.memoryMb.min} MB and ${LIMITS.memoryMb.max / 1024} GB`);
  }
  const n = cpus === undefined ? NaN : Number(cpus);
  if (!Number.isFinite(n)) c.add(where, "needs a CPU limit (cpus: 0.5)");
  else if (n < LIMITS.cpus.min || n > LIMITS.cpus.max) c.add(where, `cpus must be between ${LIMITS.cpus.min} and ${LIMITS.cpus.max}`);
  return { memoryMb: memoryMb ?? 0, cpus: Number.isFinite(n) ? Math.round(n * 100) / 100 : 0 };
}

const SERVICE_HINTS: Record<string, string> = {
  ports: "ports are not published; set x-castor.web to the service and port to serve",
  networks: "networks are managed by Castor Cloud",
  network_mode: "networks are managed by Castor Cloud",
  privileged: "privileged containers are not allowed",
  cap_add: "extra capabilities are not allowed",
  devices: "devices are not allowed",
  labels: "labels are set by Castor Cloud",
  build: "builds are not supported; use a published image",
  container_name: "container names are set by Castor Cloud",
  volumes_from: "use named volumes",
  pid: "not allowed",
  ipc: "not allowed",
  security_opt: "security options are set by Castor Cloud",
  sysctls: "not allowed",
  extra_hosts: "not allowed",
  dns: "not allowed",
};

function parseService(c: Collector, name: string, v: unknown): ServiceSpec | null {
  const where = `services.${name}`;
  if (!isObj(v)) { c.add(where, "must be a map"); return null; }
  onlyKeys(c, where, v, [
    "image", "command", "entrypoint", "environment", "working_dir", "user", "read_only", "tmpfs", "volumes",
    "mem_limit", "cpus", "deploy", "healthcheck", "depends_on", "restart",
  ], SERVICE_HINTS);

  let image = "";
  if (typeof v.image !== "string") c.add(where, "needs an image");
  else {
    const ref = parseImageRef(v.image);
    if ("error" in ref) c.add(`${where}.image`, ref.error);
    else image = ref.canonical;
  }

  let workingDir: string | undefined;
  if (v.working_dir !== undefined) {
    if (typeof v.working_dir !== "string" || !PATH_RE.test(v.working_dir)) c.add(`${where}.working_dir`, "must be an absolute path");
    else workingDir = v.working_dir;
  }

  let user: string | undefined;
  if (v.user !== undefined) {
    const u = String(v.user);
    if (!USER_RE.test(u)) c.add(`${where}.user`, "must be uid[:gid] or name[:group]");
    else user = u;
  }

  if (v.read_only !== undefined && v.read_only !== true) {
    c.add(`${where}.read_only`, "the root filesystem is always read-only; put writable paths on a volume or under tmpfs:");
  }
  const tmpfs: string[] = [];
  if (v.tmpfs !== undefined) {
    const list = typeof v.tmpfs === "string" ? [v.tmpfs] : v.tmpfs;
    if (!Array.isArray(list) || !list.every((p) => typeof p === "string") || list.length > LIMITS.tmpfs) {
      c.add(`${where}.tmpfs`, `must be a list of up to ${LIMITS.tmpfs} container paths`);
    } else {
      for (const p of list as string[]) {
        checkTarget(c, `${where}.tmpfs`, p);
        tmpfs.push(p.replace(/\/+$/, ""));
      }
    }
  }
  if (v.restart !== undefined && typeof v.restart !== "string") c.add(`${where}.restart`, "must be a string");

  let dependsOn: string[] = [];
  if (v.depends_on !== undefined) {
    if (Array.isArray(v.depends_on) && v.depends_on.every((d) => typeof d === "string")) dependsOn = v.depends_on as string[];
    else if (isObj(v.depends_on)) dependsOn = Object.keys(v.depends_on); // conditions are dropped: healthchecks + restarts instead
    else c.add(`${where}.depends_on`, "must be a list of service names");
  }

  const command = stringOrArgs(c, `${where}.command`, v.command);
  const entrypoint = stringOrArgs(c, `${where}.entrypoint`, v.entrypoint);
  for (const s of [command, entrypoint].flat()) if (typeof s === "string") noControlChars(c, where, s);

  return {
    image,
    command,
    entrypoint,
    environment: parseEnvironment(c, `${where}.environment`, v.environment),
    workingDir,
    user,
    tmpfs,
    volumes: parseMounts(c, `${where}.volumes`, v.volumes),
    ...parseResources(c, where, v),
    healthcheck: parseHealth(c, `${where}.healthcheck`, v.healthcheck),
    dependsOn,
  };
}

function parseCastor(c: Collector, v: unknown, services: string[]) {
  const where = "x-castor";
  let web = { service: services.length === 1 ? services[0] : "", port: 0 };
  let egress: string[] = [];
  let storageGb = 0;
  if (!isObj(v)) {
    c.add(where, "missing: set at least x-castor.web (service:port)");
    return { web, egress, storageGb };
  }
  onlyKeys(c, where, v, ["web", "egress", "storage_gb", "label", "description", "homepage"]);

  if (typeof v.web === "string" && /^[a-z][a-z0-9-]{0,29}:\d{1,5}$/.test(v.web)) {
    const [s, p] = v.web.split(":");
    web = { service: s, port: Number(p) };
  } else if (typeof v.web === "number" || (typeof v.web === "string" && /^\d{1,5}$/.test(v.web))) {
    web = { ...web, port: Number(v.web) };
  } else {
    c.add(`${where}.web`, "must be service:port, e.g. app:8080");
  }
  if (!services.includes(web.service)) c.add(`${where}.web`, `unknown service "${web.service}"`);
  if (!(web.port >= 1 && web.port <= 65535)) c.add(`${where}.web`, "invalid port");

  if (v.egress !== undefined) {
    if (!Array.isArray(v.egress) || !v.egress.every((h) => typeof h === "string")) c.add(`${where}.egress`, "must be a list of host names");
    else {
      egress = [...new Set((v.egress as string[]).map((h) => h.trim().toLowerCase()))];
      for (const h of egress) if (!HOST_RE.test(h)) c.add(`${where}.egress`, `invalid host "${h.slice(0, 80)}" (no IPs, no ports; *.example.com allowed)`);
      if (egress.length > LIMITS.egressHosts) c.add(`${where}.egress`, `at most ${LIMITS.egressHosts} hosts`);
    }
  }

  if (v.storage_gb !== undefined) {
    const n = Number(v.storage_gb);
    if (!Number.isInteger(n) || n < LIMITS.storageGb.min || n > LIMITS.storageGb.max) c.add(`${where}.storage_gb`, `must be a whole number of GB up to ${LIMITS.storageGb.max}`);
    else storageGb = n;
  }
  return { web, egress, storageGb };
}

function parseDocument(source: string): unknown {
  if (Buffer.byteLength(source) > LIMITS.sourceBytes) throw new SpecError([`stack definition is larger than ${LIMITS.sourceBytes / 1024} KB`]);
  try {
    // No custom tags, a hard alias limit (no "billion laughs"), duplicate keys are errors.
    return parseYaml(source, { schema: "core", maxAliasCount: 20, uniqueKeys: true, prettyErrors: false });
  } catch (err) {
    throw new SpecError([`not valid YAML: ${(err as Error).message.split("\n")[0].slice(0, 200)}`]);
  }
}

/** Parses a customer's compose subset (YAML or JSON). Throws SpecError with every problem found. */
export function parseStack(source: string): StackSpec {
  return checkStack(parseDocument(source));
}

/**
 * What is stored for a stack: the parsed document as JSON, and the spec it
 * checks to as stored. castord reads only this JSON and has no YAML parser,
 * so the document it checks and runs is exactly the one checked here.
 */
export function storedDefinition(source: string): { definition: string; spec: StackSpec } {
  const definition = JSON.stringify(parseDocument(source)) ?? "null";
  if (Buffer.byteLength(definition) > LIMITS.sourceBytes) throw new SpecError([`stack definition is larger than ${LIMITS.sourceBytes / 1024} KB`]);
  return { definition, spec: checkStack(JSON.parse(definition)) };
}

/** Validates an already parsed document (also used to re-check stored specs). */
export function checkStack(doc: unknown): StackSpec {
  const c = new Collector();
  if (!isObj(doc)) throw new SpecError(["the stack must be a map with services:"]);
  onlyKeys(c, "stack", doc, ["services", "volumes", "x-castor", "version", "name"], {
    networks: "networks are managed by Castor Cloud",
    secrets: "use environment variables (${secret:name} generates one)",
    configs: "configs are not supported",
  });

  const services: Record<string, ServiceSpec> = {};
  if (!isObj(doc.services) || Object.keys(doc.services).length === 0) c.add("services", "at least one service is needed");
  else {
    const names = Object.keys(doc.services);
    if (names.length > LIMITS.services) c.add("services", `at most ${LIMITS.services} services`);
    for (const name of names) {
      if (!NAME_RE.test(name) || name.endsWith("-") || RESERVED_SERVICE_NAMES.has(name) || RESERVED_SERVICE_PREFIXES.some((p) => name.startsWith(p))) { c.add("services", `invalid service name "${name.slice(0, 40)}"`); continue; }
      const svc = parseService(c, name, doc.services[name]);
      if (svc) services[name] = svc;
    }
  }

  const declared: string[] = [];
  if (doc.volumes !== undefined && doc.volumes !== null) {
    if (!isObj(doc.volumes)) c.add("volumes", "must be a map of volume names");
    else {
      for (const [name, def] of Object.entries(doc.volumes)) {
        if (!NAME_RE.test(name)) { c.add("volumes", `invalid volume name "${name.slice(0, 40)}"`); continue; }
        if (def !== null && !(isObj(def) && Object.keys(def).length === 0)) c.add(`volumes.${name}`, "driver options are not supported");
        declared.push(name);
      }
    }
  }
  if (declared.length > LIMITS.volumes) c.add("volumes", `at most ${LIMITS.volumes} volumes`);

  // Every mount names a declared volume, and each volume belongs to one
  // service: its directory is owned by that service's user.
  const owner = new Map<string, string>();
  for (const [name, svc] of Object.entries(services)) {
    for (const m of svc.volumes) {
      if (!declared.includes(m.volume)) c.add(`services.${name}.volumes`, `volume "${m.volume}" is not declared under volumes:`);
      const prev = owner.get(m.volume);
      if (prev && prev !== name) c.add(`services.${name}.volumes`, `volume "${m.volume}" is already used by ${prev}`);
      owner.set(m.volume, name);
    }
    for (const d of svc.dependsOn) if (!services[d] && !(isObj(doc.services) && d in doc.services)) c.add(`services.${name}.depends_on`, `unknown service "${d}"`);
    if (svc.dependsOn.includes(name)) c.add(`services.${name}.depends_on`, "can't depend on itself");
  }

  const castor = parseCastor(c, doc["x-castor"], Object.keys(services));
  if (declared.length > 0 && castor.storageGb === 0) c.add("x-castor.storage_gb", "volumes need storage: set storage_gb (e.g. 10)");

  if (c.problems.length) throw new SpecError(c.problems);
  return { services, volumes: declared, ...castor };
}

/** Everything a stack reserves (the sum of its containers' limits, plus its storage). */
export function stackResources(spec: StackSpec) {
  const all = Object.values(spec.services);
  const memoryMb = all.reduce((s, x) => s + x.memoryMb, 0);
  const cpus = Math.round(all.reduce((s, x) => s + x.cpus, 0) * 100) / 100;
  return { memoryMb, cpus, storageGb: spec.storageGb };
}

export function specUnits(spec: StackSpec): StackUnits {
  return stackUnits(stackResources(spec));
}

/** Names of the generated secrets a spec refers to. */
export function secretNames(spec: StackSpec): string[] {
  const names = new Set<string>();
  for (const svc of Object.values(spec.services)) {
    for (const v of Object.values(svc.environment)) for (const m of v.matchAll(SECRET_PLACEHOLDER_RE)) names.add(m[1]);
  }
  return [...names].sort();
}

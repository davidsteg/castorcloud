// Customers' own stacks (docs/stacks.md). One idempotent job converges a
// tenant's stacks on its STACKS node to what the database says: deploy new
// and changed ones, stop what the booking no longer covers, remove what the
// owner removed. Each step trusts nothing from the customer: the stored
// definition is parsed and validated again here, images are pulled and
// pinned to a digest, and the compose file is rendered by @pc/stacks.
import { randomBytes } from "node:crypto";
import { prisma, type Node, type Stack, type Tenant } from "@pc/db";
import { audit, openSecret, sealSecret } from "@pc/core";
import { addUnits, fitsUnits, NO_STACK_UNITS, stackStorageMb, type StackUnits } from "@pc/pricing";
import {
  STACK_NAME_RE, controlPlaneRoutes, digestFor, nodeRoute, parseImageRef, parseStack, pinned, renderCompose,
  secretNames, specUnits, stackHost, stackNames, type StackSpec,
} from "@pc/stacks";
import type { Authentik } from "./authentik.ts";
import type { SshNode } from "./node.ts";
import { removeStacksRoute, writeStacksRoute } from "./routes.ts";
import { EGRESS_IMAGE, connectEdge, disconnectEdge, putRoute, removeRoute } from "./stacks-node.ts";
import type { StacksDataset } from "./zfs.ts";

export type StacksPlatform = {
  /** Registrable domain the stacks are served under, e.g. castorapps.ch. Never the portal's domain in production. */
  domain: string;
  scheme: "http" | "https";
  /** Production: the ACME resolver that gets the stacks domain's wildcard certificate. */
  certResolver?: string;
};

/** The STACKS node of a tenant, placed on first use (Nodes.placeStacks). */
export type StacksNodeResolver = (tenantId: string) => Promise<{ row: Node; node: SshNode }>;
type Reporter = (step: string, message: string) => Promise<void>;

/** Biggest image a stack may pull (compressed layers are bigger on disk). */
export const MAX_IMAGE_BYTES = 4 * 1024 ** 3;
/** Container uids above this are outside the userns mapping (65536 ids). */
const MAX_CONTAINER_ID = 65535;

export const definitionKey = (stack: string) => `stack-definition:${stack}`;
export const stackSecretKey = (stack: string, name: string) => `stack:${stack}:${name}`;

type TenantWithStacks = Tenant & { stacks: Stack[]; volumeKey: { wrappedKey: string } | null };

// ------------------------------------------------------------ pure helpers

/** Which stacks the booking covers, in the order they were added. The rest are stopped. */
export function coveredStacks<T extends { id: string; units: StackUnits }>(stacks: T[], booked: StackUnits) {
  let used = NO_STACK_UNITS;
  const covered: T[] = [];
  const over: T[] = [];
  for (const s of stacks) {
    const next = addUnits(used, s.units);
    if (fitsUnits(next, booked)) {
      covered.push(s);
      used = next;
    } else over.push(s);
  }
  return { covered, over };
}

/** The contents of the single file in a tar archive (`docker cp <c>:<file> -`). */
export function untarFile(tar: Buffer): string {
  if (tar.length < 512) throw new Error("empty archive");
  const size = parseInt(tar.subarray(124, 136).toString("ascii").replace(/\0.*$/, "").trim(), 8);
  if (!Number.isFinite(size) || size < 0 || size > 1024 * 1024 || 512 + size > tar.length) throw new Error("unexpected archive");
  return tar.subarray(512, 512 + size).toString("utf8");
}

/** name -> id from /etc/passwd or /etc/group. */
export function parseIds(text: string): Map<string, number> {
  const ids = new Map<string, number>();
  for (const line of text.split("\n")) {
    const f = line.split(":");
    if (f.length >= 3 && /^[0-9]+$/.test(f[2])) ids.set(f[0], Number(f[2]));
  }
  return ids;
}

/**
 * The container uid/gid a service runs as: "1000", "1000:1000", "node",
 * "node:node", or "" (root). Names are looked up in the image's own passwd
 * and group files (passed in when needed).
 */
export function resolveUser(user: string, files?: { passwd: string; group: string }): { uid: number; gid: number } {
  const [u = "", g] = user.split(":");
  const passwd = files ? parseIds(files.passwd) : new Map<string, number>();
  const groups = files ? parseIds(files.group) : new Map<string, number>();
  const uid = u === "" ? 0 : /^[0-9]+$/.test(u) ? Number(u) : passwd.get(u);
  if (uid === undefined) throw new Error(`user "${u}" is not in the image's /etc/passwd; set user: to a number`);
  let gid: number | undefined;
  if (g === undefined) {
    // The user's primary group from passwd (4th field), else the same number.
    const line = files?.passwd.split("\n").find((l) => l.split(":")[2] === String(uid));
    gid = line && /^[0-9]+$/.test(line.split(":")[3] ?? "") ? Number(line.split(":")[3]) : uid;
  } else gid = /^[0-9]+$/.test(g) ? Number(g) : groups.get(g);
  if (gid === undefined) throw new Error(`group "${g}" is not in the image's /etc/group; set user: to numbers`);
  if (uid > MAX_CONTAINER_ID || gid > MAX_CONTAINER_ID) throw new Error(`user ${uid}:${gid} is outside the allowed range (0-${MAX_CONTAINER_ID})`);
  return { uid, gid };
}

const needsNames = (user: string) => user.split(":").some((p) => p !== "" && !/^[0-9]+$/.test(p));

// ------------------------------------------------------------ the manager

export class Stacks {
  private readonly nodeFor: StacksNodeResolver;
  private readonly existingNode: (nodeId: string) => Promise<{ row: Node; node: SshNode }>;
  private readonly ak: Authentik;
  private readonly p: StacksPlatform;

  constructor(
    nodeFor: StacksNodeResolver,
    existingNode: (nodeId: string) => Promise<{ row: Node; node: SshNode }>,
    ak: Authentik,
    p: StacksPlatform,
  ) {
    this.nodeFor = nodeFor;
    this.existingNode = existingNode;
    this.ak = ak;
    this.p = p;
  }

  private load(tenantId: string): Promise<TenantWithStacks> {
    return prisma.tenant.findUniqueOrThrow({
      where: { id: tenantId },
      include: { stacks: { orderBy: { createdAt: "asc" } }, volumeKey: true },
    });
  }

  private origin(slug: string, stack: string) {
    return `${this.p.scheme}://${stackHost(slug, stack, this.p.domain)}`;
  }

  private booked(t: Tenant): StackUnits {
    return { memoryUnits: t.stackMemoryUnits, cpuUnits: t.stackCpuUnits, storageUnits: t.stackStorageUnits };
  }

  private storage(node: SshNode, t: TenantWithStacks): StacksDataset {
    if (!t.volumeKey) throw new Error("tenant has no volume key yet");
    return node.stacksStorage(t.id, t.slug, t.volumeKey.wrappedKey);
  }

  /** Converges every stack of the tenant. Per-stack failures are recorded on the stack; node trouble throws. */
  async apply(tenantId: string, report: Reporter) {
    const tenant = await this.load(tenantId);
    if (tenant.status !== "ACTIVE") {
      await this.stopAll(tenantId);
      return;
    }
    const removing = tenant.stacks.filter((s) => s.status === "REMOVING");
    const wanted = tenant.stacks.filter((s) => s.status !== "REMOVING");
    if (!wanted.length && !tenant.stacksNodeId) {
      for (const s of removing) await prisma.stack.delete({ where: { id: s.id } });
      return;
    }

    const { row, node } = await this.nodeFor(tenantId);
    await report("stacks", `custom stacks on ${row.name}`);
    for (const s of removing) await this.remove(node, tenant, s);

    // Parse first: a stored definition that no longer validates is a failed stack, not a failed job.
    const parsed: { stack: Stack; spec: StackSpec; units: StackUnits; id: string }[] = [];
    for (const s of wanted) {
      try {
        if (!STACK_NAME_RE.test(s.name)) throw new Error("invalid stack name");
        const spec = parseStack(openSecret(tenant.id, definitionKey(s.name), s.definition));
        parsed.push({ stack: s, spec, units: specUnits(spec), id: s.id });
      } catch (err) {
        await this.fail(s, err);
      }
    }
    const { covered, over } = coveredStacks(parsed, this.booked(tenant));
    for (const s of over) await this.stop(node, tenant, s.stack, "not covered by the booked resources");

    const membersPk = await this.ak.ensureGroup(`cust-${tenant.slug}`);
    const defaults = await this.ak.providerDefaults();
    let storage: StacksDataset | null = null;
    if (covered.some((s) => s.spec.storageGb > 0)) {
      storage = this.storage(node, tenant);
      storage.open(stackStorageMb(Math.max(1, tenant.stackStorageUnits)));
      await audit("system", "stacks.unlock", tenant.id, { node: row.name });
    }

    for (const s of covered) {
      try {
        await prisma.stack.update({ where: { id: s.id }, data: { status: "DEPLOYING", error: null } });
        await this.deploy(node, row, tenant, s.stack, s.spec, storage, membersPk, defaults);
      } catch (err) {
        await this.fail(s.stack, err);
        await audit("system", "stack.failed", tenant.id, { stack: s.stack.name, error: (err as Error).message.slice(0, 300) });
      }
    }
    await this.writeControlPlaneRoute(tenant.id);
    await report("stacks", `${covered.length} stack(s) applied`);
  }

  private async fail(s: Stack, err: unknown) {
    const message = (err as Error).message ?? String(err);
    console.error(`[stack ${s.name}] ${message}`);
    await prisma.stack.update({ where: { id: s.id }, data: { status: "FAILED", error: message.slice(0, 2000) } });
  }

  private async secrets(tenantId: string, stack: string, names: string[]) {
    const out: Record<string, string> = {};
    for (const name of names) {
      const key = stackSecretKey(stack, name);
      const row = await prisma.secret.findUnique({ where: { tenantId_key: { tenantId, key } } });
      if (row) out[name] = openSecret(tenantId, key, row.ciphertext);
      else {
        out[name] = randomBytes(24).toString("hex");
        await prisma.secret.create({ data: { tenantId, key, ciphertext: sealSecret(tenantId, key, out[name]) } });
      }
    }
    return out;
  }

  /** Pulls the image on the node and pins it; returns the pinned reference and the image's user. */
  private pull(node: SshNode, image: string) {
    const ref = parseImageRef(image);
    if ("error" in ref) throw new Error(ref.error);
    node.docker(["pull", "--quiet", ref.canonical]);
    const info = JSON.parse(node.docker(["image", "inspect", "--format", "{{json .}}", ref.canonical])) as {
      RepoDigests?: string[];
      Size?: number;
      Config?: { User?: string };
    };
    if ((info.Size ?? 0) > MAX_IMAGE_BYTES) {
      node.docker(["image", "rm", ref.canonical], { allowFail: true });
      throw new Error(`${ref.canonical} is larger than ${MAX_IMAGE_BYTES / 1024 ** 3} GB`);
    }
    const digest = ref.digest ?? digestFor(ref, info.RepoDigests ?? []);
    if (!digest) throw new Error(`no digest for ${ref.canonical}`);
    return { image: pinned(ref, digest), user: info.Config?.User ?? "" };
  }

  /** /etc/passwd and /etc/group of an image, read without running it. */
  private imageIds(node: SshNode, image: string) {
    const name = `pc-s-ids-${randomBytes(4).toString("hex")}`;
    node.docker(["create", "--name", name, "--network", "none", image]);
    try {
      const read = (file: string) => {
        try {
          return untarFile(node.dockerBuffer(["cp", `${name}:${file}`, "-"]));
        } catch {
          return "";
        }
      };
      return { passwd: read("/etc/passwd"), group: read("/etc/group") };
    } finally {
      node.docker(["rm", "-f", name], { allowFail: true });
    }
  }

  private async deploy(
    node: SshNode, row: Node, tenant: TenantWithStacks, stack: Stack, spec: StackSpec, storage: StacksDataset | null,
    membersPk: string, defaults: Awaited<ReturnType<Authentik["providerDefaults"]>>,
  ) {
    const n = stackNames(tenant.slug, stack.name);
    const host = stackHost(tenant.slug, stack.name, this.p.domain);
    if (row.uidBase == null) throw new Error(`${row.name} has no userns uid base`);

    // Images, pinned; data directories owned by the (userns-mapped) container user.
    const images: Record<string, string> = {};
    const owners: Record<string, { uid: number; gid: number }> = {};
    for (const [name, svc] of Object.entries(spec.services)) {
      const pulled = this.pull(node, svc.image);
      images[name] = pulled.image;
      const user = svc.user ?? pulled.user;
      if (!svc.volumes.length) continue;
      const ids = resolveUser(user, needsNames(user) ? this.imageIds(node, pulled.image) : undefined);
      for (const m of svc.volumes) owners[m.volume] = { uid: row.uidBase + ids.uid, gid: row.uidBase + ids.gid };
    }
    for (const v of spec.volumes) owners[v] ??= { uid: row.uidBase, gid: row.uidBase };

    if (spec.storageGb > 0) {
      if (!storage) throw new Error("stacks dataset not open");
      storage.createStack(stack.name, spec.storageGb * 1024, owners);
    }

    await this.ak.ensureProxyApp({
      slug: n.authSlug,
      name: `${stack.name} (${tenant.slug})`,
      externalHost: this.origin(tenant.slug, stack.name),
      allowedGroupPk: membersPk,
      defaults,
    });

    const compose = renderCompose(spec, {
      slug: tenant.slug,
      stack: stack.name,
      origin: this.origin(tenant.slug, stack.name),
      dataDir: storage ? storage.dataDir(stack.name) : "/nonexistent",
      images,
      secrets: await this.secrets(tenant.id, stack.name, secretNames(spec)),
      runtime: row.stackRuntime === "runsc" ? "runsc" : undefined,
      egressImage: EGRESS_IMAGE,
    });
    const file = node.writeConfig(tenant.slug, `stacks/${stack.name}/compose.json`, JSON.stringify(compose, null, 2));
    node.docker(["compose", "-p", n.project, "-f", file, "up", "-d", "--remove-orphans", "--pull", "never", "--wait", "--wait-timeout", "300"]);
    connectEdge(node, n.edgeNetwork);
    putRoute(node, n.project, nodeRoute({ slug: tenant.slug, stack: stack.name, host, port: spec.web.port }));

    await prisma.stack.update({
      where: { id: stack.id },
      data: { status: "RUNNING", error: null, images, deployedAt: new Date() },
    });
    await audit("system", "stack.deployed", tenant.id, { stack: stack.name, images, node: row.name });
  }

  /** Containers stop, data stays. */
  private async stop(node: SshNode, tenant: Tenant, stack: Stack, reason: string) {
    const n = stackNames(tenant.slug, stack.name);
    node.docker(["compose", "-p", n.project, "stop"], { allowFail: true });
    removeRoute(node, n.project);
    await prisma.stack.update({ where: { id: stack.id }, data: { status: "STOPPED", error: reason } });
  }

  /** The owner removed it: containers, networks, route, sign-in, secrets and data go. */
  private async remove(node: SshNode, tenant: TenantWithStacks, stack: Stack) {
    const n = stackNames(tenant.slug, stack.name);
    removeRoute(node, n.project);
    disconnectEdge(node, n.edgeNetwork);
    node.docker(["compose", "-p", n.project, "down", "--volumes", "--remove-orphans"], { allowFail: true });
    if (stack.storageGb > 0 && tenant.volumeKey) this.storage(node, tenant).destroyStack(stack.name);
    node.removeLocalConfig(tenant.slug, `stacks/${stack.name}`);
    await this.ak.deleteProxyApp(n.authSlug);
    await prisma.secret.deleteMany({ where: { tenantId: tenant.id, key: { startsWith: `stack:${stack.name}:` } } });
    await prisma.stack.delete({ where: { id: stack.id } });
    await audit("system", "stack.removed", tenant.id, { stack: stack.name });
  }

  /** The control plane's routes for the tenant's running stacks. */
  private async writeControlPlaneRoute(tenantId: string) {
    const tenant = await prisma.tenant.findUniqueOrThrow({ where: { id: tenantId }, include: { stacks: true, stacksNode: true } });
    const running = tenant.stacks.filter((s) => s.status === "RUNNING");
    if (!running.length || !tenant.stacksNode?.edgeHost) {
      removeStacksRoute(tenant.slug);
      return;
    }
    const { node } = await this.existingNode(tenant.stacksNode.id);
    if (!node.edgeUrl) throw new Error(`${tenant.stacksNode.name} has no edge address`);
    writeStacksRoute(tenant.slug, controlPlaneRoutes({
      slug: tenant.slug,
      stacks: running.map((s) => ({ stack: s.name, host: stackHost(tenant.slug, s.name, this.p.domain) })),
      edgeUrl: node.edgeUrl,
      certResolver: this.p.certResolver,
      stacksDomain: this.p.domain,
    }));
  }

  /** Suspend: every stack stops and the data is locked. */
  async stopAll(tenantId: string) {
    const tenant = await this.load(tenantId);
    removeStacksRoute(tenant.slug);
    if (!tenant.stacksNodeId) return;
    const { node } = await this.existingNode(tenant.stacksNodeId);
    for (const s of tenant.stacks) {
      const n = stackNames(tenant.slug, s.name);
      node.docker(["compose", "-p", n.project, "stop"], { allowFail: true });
      removeRoute(node, n.project);
      if (s.status !== "REMOVING") await prisma.stack.update({ where: { id: s.id }, data: { status: "STOPPED", error: null } });
    }
    if (tenant.volumeKey && this.storage(node, tenant).status().state === "open") {
      this.storage(node, tenant).close();
      await audit("system", "stacks.lock", tenant.id, { reason: "suspend" });
    }
  }

  /** Tenant deleted: everything on the stacks node goes (called before the volume key is deleted). */
  async deleteAll(tenantId: string) {
    const tenant = await this.load(tenantId);
    removeStacksRoute(tenant.slug);
    if (tenant.stacksNodeId) {
      const { node } = await this.existingNode(tenant.stacksNodeId);
      for (const s of tenant.stacks) {
        const n = stackNames(tenant.slug, s.name);
        removeRoute(node, n.project);
        disconnectEdge(node, n.edgeNetwork);
        node.docker(["compose", "-p", n.project, "down", "--volumes", "--remove-orphans"], { allowFail: true });
      }
      if (tenant.volumeKey) this.storage(node, tenant).destroy();
    }
    for (const s of tenant.stacks) await this.ak.deleteProxyApp(stackNames(tenant.slug, s.name).authSlug);
    await prisma.stack.deleteMany({ where: { tenantId } });
  }

  /** Locked after a reboot of the stacks node? (Nodes.watch) */
  async needsUnlock(tenantId: string): Promise<boolean> {
    const tenant = await this.load(tenantId);
    if (!tenant.stacksNodeId || !tenant.volumeKey || !tenant.stacks.some((s) => s.storageGb > 0 && s.status === "RUNNING")) return false;
    const { node } = await this.existingNode(tenant.stacksNodeId);
    return this.storage(node, tenant).status().state === "closed";
  }
}

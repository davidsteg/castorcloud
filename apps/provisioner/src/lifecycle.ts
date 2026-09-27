// Tenant lifecycle. Every operation is idempotent: a retried or repeated job
// converges the stack to what the database says the customer has booked.
import { rmSync } from "node:fs";
import path from "node:path";
import { prisma, type Tenant } from "@pc/db";
import { audit, newWrappedVolumeKey, openSecret, sealSecret } from "@pc/core";
import type { Authentik } from "./authentik.ts";
import type { NodeExec } from "./node.ts";
import {
  ALL_VOLUMES, SERVICES, STACK_SERVICES, ensureSecrets, immichConfig, names, redirectUris,
  tenantEnv, volumesOverride, type Platform, type ServiceKey,
} from "./tenant.ts";
import { TenantVolume } from "./volume.ts";

export type Step = "identity" | "storage" | "services" | "ready";
export type Reporter = (step: Step, message: string) => Promise<void>;

const SHARED_CONTAINERS = ["pc-node-immich-ml", "pc-node-tika", "pc-node-gotenberg"];

export class Lifecycle {
  constructor(
    private readonly node: NodeExec,
    private readonly ak: Authentik,
    private readonly p: Platform,
    private readonly versions: Record<string, string>,
    private readonly versionsFile: string,
  ) {}

  private async load(tenantId: string) {
    return prisma.tenant.findUniqueOrThrow({
      where: { id: tenantId },
      include: { members: { include: { user: true } }, services: true, volumeKey: true },
    });
  }

  private enabled(tenant: Awaited<ReturnType<Lifecycle["load"]>>): ServiceKey[] {
    return STACK_SERVICES.filter((s) => tenant.services.some((i) => i.enabled && i.kind === s.toUpperCase()));
  }

  private compose(tenant: Tenant, args: string[], profiles: ServiceKey[]) {
    const n = names(tenant.slug, this.p);
    const dir = this.node.configDir(tenant.slug);
    return this.node.docker([
      "compose", "-p", n.project,
      "-f", path.join(this.node.paths.templates, "tenant/compose.yml"),
      "-f", `${dir}/compose.volumes.yml`,
      "--env-file", `${dir}/tenant.env`,
      ...profiles.flatMap((s) => ["--profile", s]),
      ...args,
    ]);
  }

  /** Provision, update and resume: make the stack match the booking. */
  async apply(tenantId: string, report: Reporter) {
    let tenant = await this.load(tenantId);
    const n = names(tenant.slug, this.p);
    const enabled = this.enabled(tenant);

    // ------------------------------------------------------------ identity
    await report("identity", "authentik groups and apps");
    const membersPk = await this.ak.ensureGroup(n.membersGroup);
    const ownersPk = await this.ak.ensureGroup(n.ownersGroup);
    for (const m of tenant.members) {
      const userPk = await this.ak.findUserPkByEmail(m.user.email);
      if (!userPk) continue; // signed up via the portal, so normally present
      await this.ak.addUserToGroup(membersPk, userPk);
      if (m.role === "OWNER") await this.ak.addUserToGroup(ownersPk, userPk);
    }

    const secrets = await this.secrets(tenant.id);
    const defaults = await this.ak.providerDefaults();
    for (const s of STACK_SERVICES) {
      if (!enabled.includes(s)) {
        await this.ak.deleteOidcApp(n.appSlug(s));
        continue;
      }
      await this.ak.ensureOidcApp({
        slug: n.appSlug(s),
        name: `${SERVICES[s].label} (${tenant.slug})`,
        clientId: n.appSlug(s),
        clientSecret: secrets[`${s.toUpperCase()}_OIDC_CLIENT_SECRET`],
        redirectUris: redirectUris(tenant.slug, this.p, s),
        launchUrl: `${n.portalOrigin}/${s}`,
        allowedGroupPk: membersPk,
        defaults,
      });
    }

    // ------------------------------------------------------------- storage
    await report("storage", "encrypted volume");
    if (!tenant.volumeKey) {
      await prisma.volumeKey.create({ data: { tenantId: tenant.id, wrappedKey: newWrappedVolumeKey(tenant.id) } });
      await audit("system", "volume.key_created", tenant.id);
      tenant = await this.load(tenantId);
    }
    const volume = new TenantVolume(this.node, tenant.id, tenant.slug, tenant.volumeKey!.wrappedKey);
    volume.ensureOpen(tenant.storageUnits, ALL_VOLUMES.map((v) => `data/${v}`));
    volume.grow(tenant.storageUnits);
    await audit("system", "volume.unlock", tenant.id, { reason: "apply" });

    // ------------------------------------------------------------ services
    await report("services", "starting containers");
    const configDir = this.node.configDir(tenant.slug);
    this.node.writeConfig(tenant.slug, "immich.json", JSON.stringify(immichConfig(tenant.slug, this.p, secrets), null, 2));
    this.node.writeConfig(tenant.slug, "compose.volumes.yml", volumesOverride(`${this.node.mountRoot}/${tenant.slug}`));
    this.node.writeConfig(
      tenant.slug,
      "tenant.env",
      tenantEnv({
        slug: tenant.slug, p: this.p, secrets, versions: this.versions,
        configDir, brandDir: this.node.paths.brand, caDir: this.node.paths.ca,
      }),
    );

    this.node.docker(["compose", "-f", path.join(this.node.paths.templates, "../../infra/node/compose.yml"), "--env-file", this.versionsFile, "up", "-d"]);
    this.compose(tenant, ["up", "-d", "--remove-orphans"], enabled);
    for (const c of SHARED_CONTAINERS) {
      this.node.docker(["network", "connect", `${n.project}-shared`, c], { allowFail: true });
    }
    // Unbooked services: stop and remove their containers; data stays on the volume.
    for (const s of STACK_SERVICES.filter((x) => !enabled.includes(x))) {
      for (const c of SERVICES[s].containers) {
        const ids = this.node.docker(["ps", "-aq", "--filter", `label=com.docker.compose.project=${n.project}`, "--filter", `label=com.docker.compose.service=${c}`]).trim();
        if (ids) this.node.docker(["rm", "-f", ...ids.split(/\s+/)]);
      }
    }

    for (const s of enabled) await this.waitHealthy(n.serviceOrigin(s) + SERVICES[s].healthPath, s);
    if (enabled.includes("photos")) await this.immichServiceAccount(tenant.slug, secrets);

    for (const s of enabled) {
      await prisma.serviceInstance.update({
        where: { tenantId_kind: { tenantId: tenant.id, kind: s.toUpperCase() as never } },
        data: { healthy: true, lastHealthAt: new Date(), version: this.versionOf(s) },
      });
    }

    // --------------------------------------------------------------- ready
    await prisma.tenant.update({ where: { id: tenant.id }, data: { status: "ACTIVE" } });
    await report("ready", "active");
  }

  async suspend(tenantId: string) {
    const tenant = await this.load(tenantId);
    this.compose(tenant, ["stop"], STACK_SERVICES);
    if (tenant.volumeKey) {
      new TenantVolume(this.node, tenant.id, tenant.slug, tenant.volumeKey.wrappedKey).close();
      await audit("system", "volume.lock", tenant.id, { reason: "suspend" });
    }
    await prisma.tenant.update({ where: { id: tenant.id }, data: { status: "SUSPENDED" } });
  }

  /** Irreversible: removes the stack, the volume, and the key (crypto-shredding). */
  async delete(tenantId: string) {
    const tenant = await this.load(tenantId);
    const n = names(tenant.slug, this.p);
    await prisma.tenant.update({ where: { id: tenant.id }, data: { status: "DELETING" } });

    // Detach the node's shared services, or the tenant network can't be removed.
    for (const c of SHARED_CONTAINERS) {
      this.node.docker(["network", "disconnect", "-f", `${n.project}-shared`, c], { allowFail: true });
    }
    this.compose(tenant, ["down", "--volumes", "--remove-orphans"], STACK_SERVICES);
    this.node.docker(["network", "rm", `${n.project}-shared`], { allowFail: true });
    if (tenant.volumeKey) {
      new TenantVolume(this.node, tenant.id, tenant.slug, tenant.volumeKey.wrappedKey).destroy();
      // Without the key, any copy of the volume (and its backups) is unreadable.
      await prisma.volumeKey.delete({ where: { tenantId: tenant.id } });
    }
    for (const s of STACK_SERVICES) await this.ak.deleteOidcApp(n.appSlug(s));
    await this.ak.deleteGroup(n.ownersGroup);
    await this.ak.deleteGroup(n.membersGroup);
    await prisma.secret.deleteMany({ where: { tenantId: tenant.id } });
    rmSync(this.node.configDir(tenant.slug), { recursive: true, force: true });

    await prisma.tenant.update({ where: { id: tenant.id }, data: { status: "DELETED" } });
    await audit("system", "tenant.deleted", tenant.id, { slug: tenant.slug, cryptoShredded: true });
  }

  // ------------------------------------------------------------- helpers

  /** Tenant secrets from the DB (encrypted at rest); missing ones are generated. */
  private async secrets(tenantId: string) {
    const rows = await prisma.secret.findMany({ where: { tenantId } });
    const existing = Object.fromEntries(rows.map((r) => [r.key, openSecret(tenantId, r.key, r.ciphertext)]));
    const { secrets, created } = ensureSecrets(existing);
    for (const key of created) {
      await prisma.secret.create({ data: { tenantId, key, ciphertext: sealSecret(tenantId, key, secrets[key]) } });
    }
    return secrets;
  }

  private versionOf(s: ServiceKey) {
    const key = { photos: "IMMICH_VERSION", docs: "PAPERLESS_VERSION", places: "DAWARICH_VERSION", flights: "AIRTRAIL_VERSION" }[s];
    return this.versions[key];
  }

  private async waitHealthy(url: string, label: string, timeoutS = 900) {
    const deadline = Date.now() + timeoutS * 1000;
    while (Date.now() < deadline) {
      try {
        const res = await fetch(url, { redirect: "manual" });
        if (res.status < 400) return; // 404 = no route yet, 5xx = still booting
      } catch {
        // not reachable yet
      }
      await new Promise((r) => setTimeout(r, 5000));
    }
    throw new Error(`${label} not healthy after ${timeoutS}s (${url})`);
  }

  /** Immich admin used by the dashboard and assistant; customers sign in via SSO. */
  private async immichServiceAccount(slug: string, secrets: Record<string, string>) {
    const origin = names(slug, this.p).serviceOrigin("photos");
    const res = await fetch(`${origin}/api/auth/admin-sign-up`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email: `service@${slug}.${this.p.baseDomain}`, password: secrets.IMMICH_SERVICE_PASSWORD, name: "Service" }),
    });
    // 400 = an admin already exists (re-run)
    if (!res.ok && res.status !== 400) throw new Error(`immich admin sign-up failed: ${res.status}`);
  }
}

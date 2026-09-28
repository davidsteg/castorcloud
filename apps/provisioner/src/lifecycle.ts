// Tenant lifecycle. Every operation is idempotent: a retried or repeated job
// converges the stack to what the database says the customer has booked.
import path from "node:path";
import { prisma, type Tenant } from "@pc/db";
import { audit, newWrappedVolumeKey, openSecret, sealSecret } from "@pc/core";
import { aiBudgetUsd, fitsStorage, storageQuotaMb } from "@pc/pricing";
import {
  assistantFiles, isMemberSecret, memberKeyName, memberToolsTokenName, newAssistantSecret, type AssistantMember,
} from "./assistant.ts";
import { accessCommands, execArgs, strongest } from "./access.ts";
import type { Authentik } from "./authentik.ts";
import type { LlmGateway } from "./llm.ts";
import type { NodeExec } from "./node.ts";
import { removeTenantRoute, writeTenantRoute } from "./routes.ts";
import type { TenantStorage } from "./storage.ts";
import {
  ALL_VOLUMES, ASSISTANT_CONTAINERS, SERVICES, STACK_SERVICES, edgeRoute, ensureSecrets, immichConfig, names, redirectUris,
  tenantEnv, volumesOverride, type Platform, type ServiceKey,
} from "./tenant.ts";

export type Step = "identity" | "storage" | "services" | "ready";
export type Reporter = (step: Step, message: string) => Promise<void>;
/** The node a tenant's stack runs on (placement happens before apply). */
export type NodeResolver = (nodeId: string | null) => Promise<NodeExec>;

const SHARED_CONTAINERS = ["pc-node-immich-ml", "pc-node-tika", "pc-node-gotenberg"];
const LLM_GATEWAY = "pc-node-llm-gateway";
/** Compose profiles: one per stack service, plus the assistant. */
const ALL_PROFILES = [...STACK_SERVICES, "assistant"];

export type AssistantSettings = {
  gateway: LlmGateway;
  /** Conservative CHF -> USD rate for budgets (the gateway meters in USD). */
  usdPerChf: number;
};

export class Lifecycle {
  constructor(
    private readonly nodeFor: NodeResolver,
    private readonly ak: Authentik,
    private readonly p: Platform,
    private readonly versions: Record<string, string>,
    private readonly versionsFile: string,
    private readonly assistant?: AssistantSettings,
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

  private storage(node: NodeExec, tenant: { id: string; slug: string; volumeKey: { wrappedKey: string } | null }) {
    return tenant.volumeKey ? node.storage(tenant.id, tenant.slug, tenant.volumeKey.wrappedKey) : null;
  }

  private assistantBooked(tenant: Awaited<ReturnType<Lifecycle["load"]>>) {
    return tenant.services.some((i) => i.enabled && i.kind === "ASSISTANT");
  }

  private compose(node: NodeExec, tenant: Tenant, args: string[], profiles: string[], input?: string) {
    const n = names(tenant.slug, this.p);
    const dir = node.configDir(tenant.slug);
    return node.docker([
      "compose", "-p", n.project,
      "-f", path.join(node.paths.templates, "tenant/compose.yml"),
      // Relative bind mounts (skin config, db init) resolve on the node.
      "--project-directory", `${node.paths.nodeTemplates}/tenant`,
      "-f", `${dir}/compose.volumes.yml`,
      "--env-file", `${dir}/tenant.env`,
      ...profiles.flatMap((s) => ["--profile", s]),
      ...args,
    ], input === undefined ? {} : { input: Buffer.from(input) });
  }

  /** Provision, update and resume: make the stack match the booking. */
  async apply(tenantId: string, report: Reporter) {
    let tenant = await this.load(tenantId);
    const node = await this.nodeFor(tenant.nodeId);
    const n = names(tenant.slug, this.p);
    const enabled = this.enabled(tenant);
    const assistant = this.assistantBooked(tenant);
    if (assistant && !this.assistant) throw new Error("assistant booked but the LLM gateway is not configured");
    const profiles = assistant ? [...enabled, "assistant"] : enabled;

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
    await this.removeFormerMembers(tenant, membersPk, ownersPk);

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
    await report("storage", `encrypted storage on ${node.name}`);
    if (!tenant.volumeKey) {
      await prisma.volumeKey.create({ data: { tenantId: tenant.id, wrappedKey: newWrappedVolumeKey(tenant.id) } });
      await audit("system", "volume.key_created", tenant.id);
      tenant = await this.load(tenantId);
    }
    const volume = this.storage(node, tenant)!;
    volume.ensureOpen(tenant.storageUnits, ALL_VOLUMES);
    await audit("system", "volume.unlock", tenant.id, { reason: "apply", node: node.name });
    await this.resize(node, tenant, volume, report);

    // ------------------------------------------------------------ services
    await report("services", "starting containers");
    node.writeNodeFile(tenant.slug, "immich.json", JSON.stringify(immichConfig(tenant.slug, this.p, secrets), null, 2));
    node.writeConfig(tenant.slug, "compose.volumes.yml", volumesOverride(volume.dataDir));
    node.writeConfig(
      tenant.slug,
      "tenant.env",
      tenantEnv({
        slug: tenant.slug, p: { ...this.p, authHostIp: node.authHostIp ?? this.p.authHostIp }, secrets, versions: this.versions,
        configDir: node.nodeConfigDir(tenant.slug), brandDir: node.paths.brand, caDir: node.paths.ca,
        assistantServices: assistant ? enabled : [],
      }),
    );
    if (assistant) await this.prepareAssistant(node, tenant, secrets, enabled);
    else if (this.assistant) await this.assistant.gateway.block(secrets.LLM_VIRTUAL_KEY);

    node.docker(
      [
        "compose", "-f", path.join(node.paths.infra, "compose.yml"),
        // The LLM gateway's config is a relative bind mount: resolve it on the node.
        "--project-directory", node.paths.nodeInfra,
        "--env-file", this.versionsFile, "up", "-d",
      ],
      node.gatewayBind ? { env: { LLM_GATEWAY_BIND: node.gatewayBind } } : {},
    );
    this.compose(node, tenant, ["up", "-d", "--remove-orphans"], profiles);
    for (const c of SHARED_CONTAINERS) {
      node.docker(["network", "connect", `${n.project}-shared`, c], { allowFail: true });
    }
    if (assistant) {
      // The gateway is the assistant's only way out of the stack.
      node.docker(["network", "connect", `${n.project}-llm`, LLM_GATEWAY], { allowFail: true });
      // Re-copy the rendered config (members, services) and restart Hermes.
      this.compose(node, tenant, ["up", "-d", "--force-recreate", "assistant-init", "assistant"], profiles);
    }
    // Unbooked services: stop and remove their containers; data stays on the volume.
    const unbooked = [
      ...STACK_SERVICES.filter((x) => !enabled.includes(x)).flatMap((s) => SERVICES[s].containers),
      ...(assistant ? [] : ASSISTANT_CONTAINERS),
    ];
    for (const c of unbooked) {
      const ids = node.docker(["ps", "-aq", "--filter", `label=com.docker.compose.project=${n.project}`, "--filter", `label=com.docker.compose.service=${c}`]).trim();
      if (ids) node.docker(["rm", "-f", ...ids.split(/\s+/)]);
    }

    // Mac nodes: the control plane forwards the tenant's service hosts to the node.
    if (node.edgeUrl) writeTenantRoute(tenant.slug, edgeRoute(tenant.slug, this.p, node.edgeUrl));
    else removeTenantRoute(tenant.slug);

    for (const s of enabled) await this.waitHealthy(n.serviceOrigin(s) + SERVICES[s].healthPath, s);
    if (enabled.includes("photos")) await this.immichServiceAccount(tenant.slug, secrets);
    if (assistant) await this.waitHealthy(`${n.assistantOrigin}/health`, "assistant");

    for (const s of enabled) {
      await prisma.serviceInstance.update({
        where: { tenantId_kind: { tenantId: tenant.id, kind: s.toUpperCase() as never } },
        data: { healthy: true, lastHealthAt: new Date(), version: this.versionOf(s) },
      });
    }
    if (assistant) {
      await prisma.serviceInstance.update({
        where: { tenantId_kind: { tenantId: tenant.id, kind: "ASSISTANT" } },
        data: { healthy: true, lastHealthAt: new Date(), version: this.versions.HERMES_IMAGE?.split(":").pop() },
      });
    }

    // Sign-outs and removals that waited for the apps to run.
    await this.processRevocations(node, tenant, enabled);

    // --------------------------------------------------------------- ready
    await prisma.tenant.update({ where: { id: tenant.id }, data: { status: "ACTIVE" } });
    await this.measure(tenant.id);
    await report("ready", "active");
  }

  /**
   * REVOKE_ACCESS job: ends access in the apps now if they run. Otherwise the
   * requests stay queued and the next apply (resume) carries them out.
   */
  async revokeAccess(tenantId: string, report: Reporter) {
    const tenant = await this.load(tenantId);
    if (tenant.status !== "ACTIVE" || !tenant.nodeId) {
      await report("identity", `apps not running (${tenant.status}); revocations wait for the next start`);
      return;
    }
    await this.processRevocations(await this.nodeFor(tenant.nodeId), tenant, this.enabled(tenant));
    await report("ready", "access revoked");
  }

  async suspend(tenantId: string) {
    const tenant = await this.load(tenantId);
    if (tenant.nodeId) {
      const node = await this.nodeFor(tenant.nodeId);
      this.compose(node, tenant, ["stop"], ALL_PROFILES);
      const volume = this.storage(node, tenant);
      if (volume) {
        volume.close();
        await audit("system", "volume.lock", tenant.id, { reason: "suspend" });
      }
    }
    const llmKey = await this.existingSecret(tenant.id, "LLM_VIRTUAL_KEY");
    if (this.assistant && llmKey) await this.assistant.gateway.block(llmKey);
    await prisma.tenant.update({ where: { id: tenant.id }, data: { status: "SUSPENDED" } });
  }

  /** Irreversible: removes the stack, the volume, and the key (crypto-shredding). */
  async delete(tenantId: string) {
    const tenant = await this.load(tenantId);
    const n = names(tenant.slug, this.p);
    await prisma.tenant.update({ where: { id: tenant.id }, data: { status: "DELETING" } });

    // A tenant that failed before placement has nothing on any node.
    const node = tenant.nodeId ? await this.nodeFor(tenant.nodeId) : null;
    if (node) {
      // Detach the node's shared services, or the tenant network can't be removed.
      for (const c of SHARED_CONTAINERS) {
        node.docker(["network", "disconnect", "-f", `${n.project}-shared`, c], { allowFail: true });
      }
      node.docker(["network", "disconnect", "-f", `${n.project}-llm`, LLM_GATEWAY], { allowFail: true });
      this.compose(node, tenant, ["down", "--volumes", "--remove-orphans"], ALL_PROFILES);
      node.docker(["network", "rm", `${n.project}-shared`], { allowFail: true });
      node.docker(["network", "rm", `${n.project}-llm`], { allowFail: true });
      this.storage(node, tenant)?.destroy();
      node.removeConfig(tenant.slug);
    }
    removeTenantRoute(tenant.slug);
    const llmKey = await this.existingSecret(tenant.id, "LLM_VIRTUAL_KEY");
    if (this.assistant && llmKey) await this.assistant.gateway.delete(llmKey);
    if (tenant.volumeKey) {
      // Without the key, any copy of the volume (and its backups) is unreadable.
      await prisma.volumeKey.delete({ where: { tenantId: tenant.id } });
    }
    for (const s of STACK_SERVICES) await this.ak.deleteOidcApp(n.appSlug(s));
    await this.ak.deleteGroup(n.ownersGroup);
    await this.ak.deleteGroup(n.membersGroup);
    await prisma.secret.deleteMany({ where: { tenantId: tenant.id } });

    await prisma.tenant.update({ where: { id: tenant.id }, data: { status: "DELETED" } });
    await audit("system", "tenant.deleted", tenant.id, { slug: tenant.slug, cryptoShredded: true });
  }

  /**
   * Records the tenant's storage usage (total and per service). Read-only on
   * the node, so it runs alongside lifecycle jobs; a closed volume is skipped.
   */
  async measure(tenantId: string) {
    const tenant = await this.load(tenantId);
    const volume = this.storage(await this.nodeFor(tenant.nodeId), tenant);
    if (!volume) return;
    const status = volume.status();
    if (status.state !== "open") return;
    await prisma.tenant.update({
      where: { id: tenant.id },
      data: {
        storageUsedMb: status.usedMb,
        storageVolumeMb: status.volumeMb,
        storageByService: usageByService(volume.usage()),
        storageMeasuredAt: new Date(),
      },
    });
  }

  // ------------------------------------------------------------- helpers

  /**
   * People in the tenant's Authentik groups who are no longer (owner) members:
   * out of the group first, so they can't sign in to the apps again, then
   * everything they still hold in the apps is revoked (processRevocations).
   */
  private async removeFormerMembers(tenant: Awaited<ReturnType<Lifecycle["load"]>>, membersPk: string, ownersPk: string) {
    const subs = new Set(tenant.members.map((m) => m.user.authentikSub));
    const ownerSubs = new Set(tenant.members.filter((m) => m.role === "OWNER").map((m) => m.user.authentikSub));
    for (const u of await this.ak.groupUsers(ownersPk)) {
      if (!ownerSubs.has(u.uid)) await this.ak.removeUserFromGroup(ownersPk, u.pk);
    }
    for (const u of await this.ak.groupUsers(membersPk)) {
      if (subs.has(u.uid)) continue;
      await prisma.accessRevocation.create({ data: { tenantId: tenant.id, email: u.email, sub: u.uid, scope: "REMOVED" } });
      await this.ak.removeUserFromGroup(membersPk, u.pk);
      await audit("system", "member.removed_from_apps", tenant.id, { sub: u.uid });
    }
  }

  /**
   * Carries out the tenant's pending AccessRevocations in the booked apps
   * (commands in access.ts). "Sign out everywhere" (DEVICES) also ends the
   * person's Authentik sessions; a removed member may still use Authentik for
   * another cloud, and leaving the group already locks them out of this one.
   * Requests are deleted once done.
   */
  private async processRevocations(node: NodeExec, tenant: Tenant, enabled: ServiceKey[]) {
    const pending = await prisma.accessRevocation.findMany({ where: { tenantId: tenant.id }, orderBy: { createdAt: "asc" } });
    const people = new Map<string, typeof pending>();
    for (const r of pending) people.set(r.sub, [...(people.get(r.sub) ?? []), r]);

    for (const [sub, requests] of people) {
      const scope = strongest(requests.map((r) => r.scope));
      const who = { email: requests.at(-1)!.email, sub, before: requests.at(-1)!.createdAt };
      for (const cmd of accessCommands(enabled, who, scope)) {
        this.compose(node, tenant, execArgs(cmd), enabled, cmd.input ?? "");
      }
      if (requests.some((r) => r.scope === "DEVICES")) {
        const userPk = await this.ak.findUserPkByEmail(who.email);
        if (userPk) await this.ak.endUserSessions(userPk);
      }
      await prisma.accessRevocation.deleteMany({ where: { id: { in: requests.map((r) => r.id) } } });
      await audit("system", "access.revoked", tenant.id, { sub, scope, apps: enabled });
    }
  }

  /**
   * Sizes the volume to the booking. Growing is online. Shrinking only happens
   * when the data fits with headroom; otherwise the volume keeps its size until
   * the customer frees up space or books more, and the portal says so. LUKS
   * volumes stop the stack for a moment (ext4 can't shrink while mounted);
   * ZFS quotas shrink online.
   */
  private async resize(node: NodeExec, tenant: Tenant, volume: TenantStorage, report: Reporter) {
    const status = volume.status();
    const targetMb = storageQuotaMb(tenant.storageUnits);
    if (status.state !== "open" || status.volumeMb <= targetMb) {
      volume.grow(tenant.storageUnits);
      return;
    }
    if (!fitsStorage(status.usedMb, tenant.storageUnits)) {
      await report("storage", `downgrade held back: ${status.usedMb} MB used, ${targetMb} MB booked`);
      await audit("system", "volume.shrink_held", tenant.id, { usedMb: status.usedMb, volumeMb: status.volumeMb, targetMb });
      return;
    }
    await report("storage", "shrinking volume");
    if (volume.offlineShrink) this.compose(node, tenant, ["stop"], STACK_SERVICES);
    volume.shrink(tenant.storageUnits);
    await audit("system", "volume.shrunk", tenant.id, { fromMb: status.volumeMb, toMb: targetMb });
  }


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

  private async existingSecret(tenantId: string, key: string) {
    const row = await prisma.secret.findUnique({ where: { tenantId_key: { tenantId, key } } });
    return row ? openSecret(tenantId, key, row.ciphertext) : undefined;
  }

  /**
   * Assistant: the tenant's LLM key with this month's budget, one Hermes
   * profile per member (own API key and tools token), rendered config.
   */
  private async prepareAssistant(
    node: NodeExec,
    tenant: Awaited<ReturnType<Lifecycle["load"]>>,
    secrets: Record<string, string>,
    services: ServiceKey[],
  ) {
    const { gateway, usdPerChf } = this.assistant!;
    await gateway.ensureTenantKey({
      key: secrets.LLM_VIRTUAL_KEY,
      slug: tenant.slug,
      tenantId: tenant.id,
      maxBudgetUsd: aiBudgetUsd(tenant.aiCreditChf, usdPerChf),
    });

    const members: { member: AssistantMember; apiKey: string; toolsToken: string }[] = [];
    for (const m of tenant.members) {
      members.push({
        member: {
          id: m.id,
          email: m.user.email,
          authentikSub: m.user.authentikSub,
          name: m.user.name,
          locale: m.user.locale ?? tenant.locale,
        },
        apiKey: await this.memberSecret(tenant.id, memberKeyName(m.id)),
        toolsToken: await this.memberSecret(tenant.id, memberToolsTokenName(m.id)),
      });
    }
    // Members who left lose their keys; assistant-init removes their memory.
    const current = new Set(tenant.members.flatMap((m) => [memberKeyName(m.id), memberToolsTokenName(m.id)]));
    const stale = await prisma.secret.findMany({ where: { tenantId: tenant.id }, select: { key: true } });
    const gone = stale.map((r) => r.key).filter((k) => isMemberSecret(k) && !current.has(k));
    if (gone.length) await prisma.secret.deleteMany({ where: { tenantId: tenant.id, key: { in: gone } } });

    const files = assistantFiles({
      brandName: this.p.brandName,
      services,
      adminKey: secrets.ASSISTANT_ADMIN_KEY,
      llmKey: secrets.LLM_VIRTUAL_KEY,
      members,
    });
    node.removeNodeFiles(tenant.slug, "assistant/hermes/profiles");
    for (const [file, content] of Object.entries(files)) {
      // The tools container runs unprivileged; members.json holds only token hashes.
      node.writeNodeFile(tenant.slug, `assistant/${file}`, content, file === "members.json" ? 0o644 : 0o600);
    }
  }

  private async memberSecret(tenantId: string, key: string) {
    const existing = await this.existingSecret(tenantId, key);
    if (existing) return existing;
    const value = newAssistantSecret();
    await prisma.secret.create({ data: { tenantId, key, ciphertext: sealSecret(tenantId, key, value) } });
    return value;
  }
}

/** Maps data directories (compose volume names) to the service that owns them. */
export function usageByService(byDir: Record<string, number>) {
  const out: Record<ServiceKey | "system", number> = { photos: 0, docs: 0, places: 0, flights: 0, system: 0 };
  for (const [dir, mb] of Object.entries(byDir)) {
    const owner = STACK_SERVICES.find((s) => SERVICES[s].volumes.includes(dir)) ?? "system";
    out[owner] += mb;
  }
  return out;
}

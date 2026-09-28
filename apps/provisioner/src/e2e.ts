// End-to-end check of the provisioning pipeline, without browser or Stripe:
// creates a test tenant, waits for the worker to provision it, verifies the
// encrypted volume and every service, then deletes it again.
//   npm run e2e -w @pc/provisioner            (keeps the tenant)
//   npm run e2e -w @pc/provisioner -- --delete
import path from "node:path";
import { fileURLToPath } from "node:url";
import { config } from "dotenv";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
config({ path: path.join(root, ".env"), quiet: true });
const { prisma } = await import("@pc/db");
const { enqueueTenantJob } = await import("@pc/core");
const { Nodes } = await import("./nodes.ts");

const base = process.env.BASE_DOMAIN ?? "brand.localhost";
const scheme = process.env.PUBLIC_SCHEME ?? "https";
const slug = `e2e-${Date.now().toString(36)}`;
const services = ["PHOTOS", "DOCS", "PLACES", "FLIGHTS"] as const;

const user = await prisma.user.upsert({
  where: { authentikSub: "e2e-test" },
  update: {},
  create: { authentikSub: "e2e-test", email: "e2e@example.invalid", name: "E2E" },
});
const tenant = await prisma.tenant.create({
  data: {
    slug,
    displayName: "E2E test",
    status: "PROVISIONING",
    members: { create: { userId: user.id, role: "OWNER" } },
    services: { create: services.map((kind) => ({ kind })) },
  },
});
console.log(`tenant ${slug} created, provisioning…`);
await enqueueTenantJob(tenant.id, "PROVISION");

async function waitFor(status: string[], timeoutS = 1800) {
  let lastStep = "";
  const deadline = Date.now() + timeoutS * 1000;
  while (Date.now() < deadline) {
    const t = await prisma.tenant.findUniqueOrThrow({ where: { id: tenant.id } });
    const job = await prisma.job.findFirst({ where: { tenantId: tenant.id }, orderBy: { createdAt: "desc" } });
    if (job?.step && job.step !== lastStep) console.log(`  step: ${(lastStep = job.step)}`);
    if (status.includes(t.status)) return t.status;
    if (job?.status === "FAILED") throw new Error(`job failed: ${job.error}`);
    await new Promise((r) => setTimeout(r, 3000));
  }
  throw new Error("timeout");
}

const t0 = Date.now();
const result = await waitFor(["ACTIVE", "FAILED"]);
console.log(`status ${result} after ${Math.round((Date.now() - t0) / 1000)}s`);

const key = await prisma.volumeKey.findUniqueOrThrow({ where: { tenantId: tenant.id } });
const placed = await prisma.tenant.findUniqueOrThrow({ where: { id: tenant.id }, include: { node: true } });
const volume = new Nodes(root).driver(placed.node!).storage(tenant.id, slug, key.wrappedKey);
console.log(`node: ${placed.node!.name}`);
console.log("volume:", JSON.stringify(volume.status()));
const secrets = await prisma.secret.findMany({ where: { tenantId: tenant.id } });
console.log(`secrets: ${secrets.length} stored, encrypted: ${secrets.every((s) => s.ciphertext.startsWith("v1."))}`);

for (const s of ["photos", "docs", "places", "flights"]) {
  const res = await fetch(`${scheme}://${s}-${slug}.${base}/`, { redirect: "manual" });
  const csp = res.headers.get("content-security-policy");
  console.log(`${s.padEnd(8)} ${res.status}  frame-ancestors ok: ${csp === `frame-ancestors ${scheme}://${slug}.${base}`}`);
}

if (process.argv.includes("--delete")) {
  console.log("deleting…");
  await enqueueTenantJob(tenant.id, "DELETE");
  await waitFor(["DELETED"]);
  const keyLeft = await prisma.volumeKey.findUnique({ where: { tenantId: tenant.id } });
  console.log(`deleted. volume: ${JSON.stringify(volume.status())}, key destroyed: ${!keyLeft}`);
}
await prisma.$disconnect();
process.exit(0);

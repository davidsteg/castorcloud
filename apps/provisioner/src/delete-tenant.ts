// Ops helper: queue deletion of a tenant (irreversible once the job runs:
// stack, encrypted volume and volume key are destroyed).
//   npm run delete-tenant -w @pc/provisioner -- <slug>
import path from "node:path";
import { fileURLToPath } from "node:url";
import { config } from "dotenv";

config({ path: path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../.env"), quiet: true });
const { prisma } = await import("@pc/db");
const { audit, enqueueTenantJob } = await import("@pc/core");

const slug = process.argv[2];
if (!slug) throw new Error("usage: delete-tenant <slug>");
const tenant = await prisma.tenant.findUniqueOrThrow({ where: { slug } });
await audit("ops-cli", "tenant.delete_requested", tenant.id, { slug });
const job = await enqueueTenantJob(tenant.id, "DELETE");
console.log(`queued DELETE for ${slug} (job ${job.id})`);
await prisma.$disconnect();
process.exit(0);

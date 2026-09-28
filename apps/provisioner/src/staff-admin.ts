// Staff accounts for the admin console (app.<domain>/<locale>/admin): beta
// applications and the newsletter. The person must have signed in once.
//   npm run staff -w @pc/provisioner -- list
//   npm run staff -w @pc/provisioner -- add <email>
//   npm run staff -w @pc/provisioner -- remove <email>
import path from "node:path";
import { fileURLToPath } from "node:url";
import { config } from "dotenv";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
config({ path: path.join(root, ".env"), quiet: true });
const { prisma } = await import("@pc/db");
const { audit } = await import("@pc/core");

const [cmd, email] = process.argv.slice(2);

switch (cmd) {
  case "list": {
    const staff = await prisma.user.findMany({ where: { isStaff: true }, orderBy: { email: "asc" } });
    for (const u of staff) console.log(u.email);
    if (!staff.length) console.log("no staff");
    break;
  }
  case "add":
  case "remove": {
    if (!email) throw new Error(`usage: ${cmd} <email>`);
    const user = await prisma.user.findUnique({ where: { email: email.trim().toLowerCase() } })
      ?? await prisma.user.findUnique({ where: { email: email.trim() } });
    if (!user) throw new Error(`no user ${email}: they must sign in to the portal once first`);
    await prisma.user.update({ where: { id: user.id }, data: { isStaff: cmd === "add" } });
    await audit("system", cmd === "add" ? "staff.grant" : "staff.revoke", undefined, { userId: user.id, email: user.email });
    console.log(`${user.email}: ${cmd === "add" ? "now staff" : "no longer staff"}`);
    break;
  }
  default:
    console.log("usage: list | add <email> | remove <email>");
}
await prisma.$disconnect();

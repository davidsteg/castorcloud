// Stores stack definitions saved before castord ran STACKS as the parsed
// document (JSON), which is all castord reads: it never parses the
// customer's YAML. Run once after upgrading:
//   npm run stacks:json -w @pc/portal
// Idempotent. A definition that no longer parses stays as it is and its
// stack fails as before.
import path from "node:path";
import { config } from "dotenv";

config({ path: path.resolve(import.meta.dirname, "../../../.env"), quiet: true });
const { prisma } = await import("@pc/db");
const { openSecret, sealSecret } = await import("@pc/core");
const { storedDefinition } = await import("@pc/stacks");

/** As in src/lib/stacks.ts. */
const definitionKey = (name: string) => `stack-definition:${name}`;

const stacks = await prisma.stack.findMany({ select: { id: true, tenantId: true, name: true, definition: true } });
let converted = 0;
for (const s of stacks) {
  const key = definitionKey(s.name);
  let definition: string;
  try {
    const source = openSecret(s.tenantId, key, s.definition);
    try {
      JSON.parse(source);
      continue;
    } catch {
      // YAML: convert below.
    }
    ({ definition } = storedDefinition(source));
  } catch (err) {
    console.error(`${s.id}: ${(err as Error).message}`);
    continue;
  }
  await prisma.stack.update({ where: { id: s.id }, data: { definition: sealSecret(s.tenantId, key, definition) } });
  converted++;
}
console.log(`stored ${converted} of ${stacks.length} stack definition(s) as JSON`);
await prisma.$disconnect();

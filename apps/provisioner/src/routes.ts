// Per-tenant routes in the control plane Traefik's file-provider directory
// (it watches the directory, so writing a file is enough).
import { renameSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const dir = () =>
  process.env.TRAEFIK_DYNAMIC_DIR ??
  path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../infra/control-plane/traefik/dynamic");

const file = (slug: string) => path.join(dir(), `tenant-${slug}.yml`);

export function writeTenantRoute(slug: string, content: string) {
  // Write then rename, so Traefik never reads a half-written file.
  const tmp = `${file(slug)}.tmp`;
  writeFileSync(tmp, content, { mode: 0o644 });
  renameSync(tmp, file(slug));
}

export function removeTenantRoute(slug: string) {
  rmSync(file(slug), { force: true });
}

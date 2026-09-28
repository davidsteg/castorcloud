// A data-plane node: where tenant stacks run. The LocalNode drives the Docker
// host the provisioner runs on (development, single-server setups). A remote
// node implements the same interface over SSH (docker compose on the node).
import { execFileSync } from "node:child_process";
import { chmodSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";

export interface NodeExec {
  /** Runs docker with args; `input` is passed on stdin (used for keys). */
  docker(args: string[], opts?: { input?: Buffer; allowFail?: boolean }): string;
  /** Writes a tenant config file (owner-only unless `mode` says otherwise); returns its path. */
  writeConfig(slug: string, file: string, content: string, mode?: number): string;
  configDir(slug: string): string;
  /** One-time node preparation (idempotent). */
  prepare(): void;
  readonly paths: { templates: string; brand: string; ca: string };
  /** Where tenant volumes are mounted on the node. */
  readonly mountRoot: string;
}

const posix = (p: string) => p.replace(/\\/g, "/");

export class LocalNode implements NodeExec {
  readonly mountRoot = "/srv/pc/tenants";
  readonly paths: NodeExec["paths"];

  constructor(private readonly root: string) {
    this.paths = {
      templates: path.join(root, "packages/stack-templates"),
      brand: posix(path.join(root, "packages/brand")),
      ca: posix(path.join(root, ".dev-certs")),
    };
  }

  docker(args: string[], opts: { input?: Buffer; allowFail?: boolean } = {}) {
    try {
      return execFileSync("docker", args, {
        input: opts.input,
        stdio: ["pipe", "pipe", "pipe"],
        encoding: "utf8",
        // Git Bash / MSYS must not rewrite /srv/... into Windows paths.
        env: { ...process.env, MSYS_NO_PATHCONV: "1" },
        maxBuffer: 32 * 1024 * 1024,
      });
    } catch (err) {
      if (opts.allowFail) return "";
      const e = err as { stderr?: string; message?: string };
      throw new Error(`docker ${args.slice(0, 4).join(" ")} … failed: ${(e.stderr || e.message || "").trim().slice(-2000)}`);
    }
  }

  configDir(slug: string) {
    return posix(path.join(this.root, ".tenants", slug));
  }

  writeConfig(slug: string, file: string, content: string, mode = 0o600) {
    const target = path.join(this.configDir(slug), file);
    mkdirSync(path.dirname(target), { recursive: true });
    writeFileSync(target, content, { mode });
    chmodSync(target, mode); // writeFileSync keeps the mode of an existing file
    return posix(target);
  }

  prepare() {
    // /srv/pc must be a shared mount so volumes opened by the cryptd helper
    // are visible to tenant containers. On production nodes this is part of
    // the node bootstrap; Docker Desktop's VM loses it on restart.
    this.docker([
      "run", "--rm", "--privileged", "--pid=host", "alpine:3.22", "nsenter", "-t", "1", "-m", "--", "sh", "-c",
      "mkdir -p /srv/pc/volumes /srv/pc/tenants && (findmnt -no PROPAGATION /srv/pc | grep -q shared || (mount --bind /srv/pc /srv/pc && mount --make-rshared /srv/pc))",
    ]);
    this.docker(["build", "-q", "-t", CRYPTD_IMAGE, path.join(this.root, "infra/node/cryptd")]);
    this.docker(["build", "-q", "-t", ASSISTANT_TOOLS_IMAGE, "-f", path.join(this.root, "agent/tools/Dockerfile"), this.root]);
  }
}

export const CRYPTD_IMAGE = "pc-cryptd:1";
/** Same tag as ASSISTANT_TOOLS_IMAGE in packages/stack-templates/versions.env. */
export const ASSISTANT_TOOLS_IMAGE = "pc-assistant-tools:1";

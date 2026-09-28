// Castor's own node images (cryptd, assistant-tools).
//
// Production (CASTOR_IMAGE_TAG set): the images come from GHCR, published by
// the Images workflow. The control plane checks each image's cosign signature
// against CASTOR_COSIGN_PUBKEY (verify-image.ts) and resolves it to a digest; the node then
// pulls exactly that digest and tags it with the local name the stacks use.
// Registry credentials (GHCR_PULL_TOKEN) stay on the control plane: the
// docker CLI hands them to the node's engine for that one pull.
//
// Development (CASTOR_IMAGE_TAG unset): the images are built on the node from
// this checkout, as before. See docs/ci.md.
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import os from "node:os";
import path from "node:path";

export type NodeImageName = "cryptd" | "assistant-tools";

/** What the docker CLI needs to talk to one node. */
export type ImageTarget = {
  docker(args: string[], opts?: { env?: NodeJS.ProcessEnv }): string;
};

export type ImageSource =
  | { kind: "build" }
  | { kind: "registry"; repo: string; tag: string; pubkey: string; user?: string; token?: string };

const TAG = /^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$/;
const REPO = /^[a-z0-9.-]+(?::[0-9]+)?(?:\/[a-z0-9._-]+)+$/;

export function imageSource(env: NodeJS.ProcessEnv = process.env): ImageSource {
  const tag = env.CASTOR_IMAGE_TAG?.trim();
  if (!tag) return { kind: "build" };
  if (!TAG.test(tag)) throw new Error("CASTOR_IMAGE_TAG is not a valid image tag");
  const repo = (env.CASTOR_IMAGE_REPO ?? "ghcr.io/davidsteg/castorcloud").trim();
  if (!REPO.test(repo)) throw new Error("CASTOR_IMAGE_REPO is not a valid repository");
  // Unsigned images are never run: no key, no pull.
  const pubkey = env.CASTOR_COSIGN_PUBKEY?.trim();
  if (!pubkey) throw new Error("CASTOR_IMAGE_TAG is set but CASTOR_COSIGN_PUBKEY is not");
  const token = env.GHCR_PULL_TOKEN?.trim() || undefined;
  return { kind: "registry", repo, tag, pubkey, token, user: token ? (env.GHCR_PULL_USER?.trim() || "castor") : undefined };
}

/** ghcr.io/davidsteg/castorcloud + cryptd -> ghcr.io/davidsteg/castorcloud-cryptd */
export function imageRepo(repo: string, name: NodeImageName) {
  return `${repo}-${name}`;
}

/** docker config with only the GHCR pull credential; removed after use. */
function withRegistryAuth<T>(src: Extract<ImageSource, { kind: "registry" }>, fn: (env: NodeJS.ProcessEnv) => T): T {
  if (!src.token) return fn({});
  const dir = mkdtempSync(path.join(os.tmpdir(), "castor-pull-"));
  try {
    const host = src.repo.split("/")[0];
    const auth = Buffer.from(`${src.user}:${src.token}`).toString("base64");
    writeFileSync(path.join(dir, "config.json"), JSON.stringify({ auths: { [host]: { auth } } }), { mode: 0o600 });
    return fn({ DOCKER_CONFIG: dir });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

const verified = new Map<string, string>();
const verifier = path.join(path.dirname(fileURLToPath(import.meta.url)), "verify-image.ts");

/**
 * Signature check on the control plane; cached per process. Returns repo@digest.
 * Runs verify-image.ts as a child so node preparation stays synchronous; the
 * pull token travels on stdin, never in argv.
 */
function verify(src: Extract<ImageSource, { kind: "registry" }>, name: NodeImageName) {
  const repo = imageRepo(src.repo, name);
  const ref = `${repo}:${src.tag}`;
  const cached = verified.get(ref);
  if (cached) return cached;
  const input = JSON.stringify({ repo, tag: src.tag, pubkey: readFileSync(src.pubkey, "utf8"), user: src.user, token: src.token });
  let digest: string;
  try {
    digest = execFileSync(process.execPath, ["--import", "tsx", verifier], { input, encoding: "utf8", stdio: ["pipe", "pipe", "pipe"] }).trim();
  } catch (err) {
    throw new Error(`${ref}: signature check failed: ${String((err as { stderr?: string }).stderr ?? "").trim()}`);
  }
  if (!/^sha256:[a-f0-9]{64}$/.test(digest)) throw new Error(`${ref}: signature check gave no digest`);
  const pinned = `${repo}@${digest}`;
  verified.set(ref, pinned);
  return pinned;
}

/**
 * Puts image `name` on the node under `local`: pulled by verified digest in
 * production, built from `build` (docker build args) in development.
 */
export function installImage(node: ImageTarget, name: NodeImageName, local: string, build: string[], src = imageSource()) {
  if (src.kind === "build") {
    node.docker(["build", "-q", "-t", local, ...build]);
    return;
  }
  const pinned = verify(src, name);
  withRegistryAuth(src, (env) => node.docker(["pull", "-q", pinned], { env }));
  node.docker(["tag", pinned, local]);
}

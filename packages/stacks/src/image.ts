// Image references a custom stack may use. Only public images from a short
// list of registries: no local or LAN registries (a stack must not make the
// node pull from inside the network), no credentials. At deploy the
// provisioner pulls the reference on the stack node and pins it to the digest
// it got (stacks.ts), so a moved tag never changes a running stack silently.

/** Registries images may come from. Docker Hub is the default (no registry in the reference). */
export const ALLOWED_REGISTRIES = ["docker.io", "ghcr.io", "quay.io", "lscr.io", "registry.gitlab.com", "codeberg.org", "gcr.io", "mcr.microsoft.com"];

export type ImageRef = {
  registry: string;
  /** e.g. "library/nginx", "immich-app/immich-server" */
  repository: string;
  tag?: string;
  digest?: string;
  /** registry/repository[:tag][@digest], the form we store and render */
  canonical: string;
};

const COMPONENT = "[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*";
const REPO_RE = new RegExp(`^${COMPONENT}(?:/${COMPONENT}){0,4}$`);
const TAG_RE = /^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$/;
const DIGEST_RE = /^sha256:[a-f0-9]{64}$/;

export function parseImageRef(input: string): ImageRef | { error: string } {
  const ref = input.trim();
  if (!ref || ref.length > 255) return { error: "invalid image reference" };

  let rest = ref;
  let digest: string | undefined;
  const at = rest.indexOf("@");
  if (at >= 0) {
    digest = rest.slice(at + 1);
    rest = rest.slice(0, at);
    if (!DIGEST_RE.test(digest)) return { error: "invalid digest (expected sha256:<64 hex>)" };
  }

  // The first component is a registry when it has a dot or a colon (or is localhost).
  let registry = "docker.io";
  const slash = rest.indexOf("/");
  if (slash > 0) {
    const first = rest.slice(0, slash);
    if (first.includes(".") || first.includes(":") || first === "localhost") {
      registry = first;
      rest = rest.slice(slash + 1);
    }
  }
  if (registry === "index.docker.io" || registry === "registry-1.docker.io") registry = "docker.io";
  // Exact match against the list (written out, so it never reads as a substring check).
  if (!ALLOWED_REGISTRIES.some((allowed) => allowed === registry)) {
    return { error: `registry ${registry} is not allowed (allowed: ${ALLOWED_REGISTRIES.join(", ")})` };
  }

  let tag: string | undefined;
  const colon = rest.lastIndexOf(":");
  if (colon >= 0) {
    tag = rest.slice(colon + 1);
    rest = rest.slice(0, colon);
    if (!TAG_RE.test(tag)) return { error: "invalid tag" };
  }
  if (!REPO_RE.test(rest)) return { error: "invalid repository name" };
  const repository = registry === "docker.io" && !rest.includes("/") ? `library/${rest}` : rest;
  if (!tag && !digest) tag = "latest";

  const canonical = `${registry}/${repository}${tag ? `:${tag}` : ""}${digest ? `@${digest}` : ""}`;
  return { registry, repository, tag, digest, canonical };
}

/** The reference pinned to `digest` (keeps the tag for readability; Docker uses the digest). */
export function pinned(ref: ImageRef, digest: string): string {
  if (!DIGEST_RE.test(digest)) throw new Error(`invalid digest ${digest}`);
  return `${ref.registry}/${ref.repository}${ref.tag ? `:${ref.tag}` : ""}@${digest}`;
}

/** "docker.io/library/nginx@sha256:..." from `docker image inspect`'s RepoDigests, for this repository. */
export function digestFor(ref: ImageRef, repoDigests: string[]): string | null {
  const names = new Set([`${ref.registry}/${ref.repository}`]);
  if (ref.registry === "docker.io") {
    names.add(ref.repository);
    names.add(ref.repository.replace(/^library\//, ""));
  }
  for (const d of repoDigests) {
    const at = d.lastIndexOf("@");
    if (at > 0 && names.has(d.slice(0, at)) && DIGEST_RE.test(d.slice(at + 1))) return d.slice(at + 1);
  }
  return null;
}

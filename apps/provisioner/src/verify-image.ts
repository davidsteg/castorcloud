// Checks a cosign signature on a registry image without the cosign binary
// (which would add a large Go dependency tree to the castor image).
//
// Scope: key-based cosign signatures in the classic layout the Images
// workflow writes (cosign sign --key, stored at <repo>:sha256-<hex>.sig).
// The tag is resolved to a digest; the digest counts as verified only when a
// signature layer's payload (hash-checked) carries a valid ECDSA signature
// from the pinned public key and names exactly this repository and digest.
//
// As a CLI (images.ts runs it synchronously): reads {repo, tag, pubkey,
// user?, token?} as JSON on stdin, prints the verified digest.
import { createHash, createPublicKey, verify as verifySig } from "node:crypto";

export type Fetch = (url: string, init: { headers: Record<string, string> }) => Promise<Response>;

export type VerifyInput = {
  repo: string;
  tag: string;
  /** PEM of the cosign public key (cosign.pub). */
  pubkey: string;
  user?: string;
  token?: string;
};

const MAX_BYTES = 4 * 1024 * 1024;
const DIGEST = /^sha256:[a-f0-9]{64}$/;
const SIG_ANNOTATION = "dev.cosignproject.cosign/signature";
const MANIFESTS = [
  "application/vnd.oci.image.index.v1+json",
  "application/vnd.oci.image.manifest.v1+json",
  "application/vnd.docker.distribution.manifest.list.v2+json",
  "application/vnd.docker.distribution.manifest.v2+json",
].join(", ");

const sha256 = (b: Uint8Array) => `sha256:${createHash("sha256").update(b).digest("hex")}`;

/** Plain HTTP only for a registry on this machine (tests); everything else is HTTPS. */
function base(host: string) {
  return /^(localhost|127\.0\.0\.1)(:\d+)?$/.test(host) ? `http://${host}` : `https://${host}`;
}

class Registry {
  private bearer?: string;
  private readonly host: string;
  private readonly name: string;
  private readonly creds: { user?: string; token?: string };
  private readonly fetchFn: Fetch;

  constructor(repo: string, creds: { user?: string; token?: string }, fetchFn: Fetch) {
    this.creds = creds;
    this.fetchFn = fetchFn;
    const slash = repo.indexOf("/");
    this.host = repo.slice(0, slash);
    this.name = repo.slice(slash + 1);
  }

  /** GET /v2/<name>/<path>, answering one Bearer token challenge. */
  async get(path: string, accept: string): Promise<{ body: Uint8Array; headers: Headers }> {
    const url = `${base(this.host)}/v2/${this.name}/${path}`;
    const headers = (): Record<string, string> => ({ Accept: accept, ...(this.bearer ? { Authorization: `Bearer ${this.bearer}` } : {}) });
    let res = await this.fetchFn(url, { headers: headers() });
    if (res.status === 401 && !this.bearer) {
      await this.login(res.headers.get("www-authenticate") ?? "");
      res = await this.fetchFn(url, { headers: headers() });
    }
    if (!res.ok) throw new Error(`registry: ${path} answered ${res.status}`);
    const body = new Uint8Array(await res.arrayBuffer());
    if (body.byteLength > MAX_BYTES) throw new Error(`registry: ${path} is too large`);
    return { body, headers: res.headers };
  }

  private async login(challenge: string) {
    const params = Object.fromEntries([...challenge.matchAll(/(\w+)="([^"]*)"/g)].map((m) => [m[1], m[2]]));
    if (!/^Bearer /i.test(challenge) || !params.realm) throw new Error("registry: unsupported auth challenge");
    const realm = new URL(params.realm);
    // Credentials go only to the registry's own host, over HTTPS (plain HTTP
    // only for a registry on this machine).
    if (realm.hostname !== this.host.split(":")[0]) throw new Error("registry: token realm on another host");
    if (realm.protocol !== "https:" && realm.origin !== new URL(base(this.host)).origin) throw new Error("registry: insecure token realm");
    if (params.service) realm.searchParams.set("service", params.service);
    realm.searchParams.set("scope", params.scope ?? `repository:${this.name}:pull`);
    const auth: Record<string, string> = this.creds.token ? { Authorization: `Basic ${Buffer.from(`${this.creds.user ?? "castor"}:${this.creds.token}`).toString("base64")}` } : {};
    const res = await this.fetchFn(realm.toString(), { headers: auth });
    if (!res.ok) throw new Error(`registry: token request answered ${res.status}`);
    const json = (await res.json()) as { token?: string; access_token?: string };
    this.bearer = json.token ?? json.access_token;
    if (!this.bearer) throw new Error("registry: no token");
  }
}

/** Resolves repo:tag to a digest and returns it only if a valid signature covers it. */
export async function verifyImage(input: VerifyInput, fetchFn: Fetch = fetch): Promise<string> {
  const key = createPublicKey(input.pubkey);
  const reg = new Registry(input.repo, input, fetchFn);

  const manifest = await reg.get(`manifests/${encodeURIComponent(input.tag)}`, MANIFESTS);
  const digest = sha256(manifest.body);
  const claimed = manifest.headers.get("docker-content-digest");
  if (claimed && claimed !== digest) throw new Error("registry: manifest digest mismatch");

  const sigManifest = await reg.get(`manifests/${digest.replace(":", "-")}.sig`, MANIFESTS);
  const layers = (JSON.parse(Buffer.from(sigManifest.body).toString("utf8")) as {
    layers?: { digest?: string; annotations?: Record<string, string> }[];
  }).layers ?? [];

  for (const layer of layers) {
    const sig = layer.annotations?.[SIG_ANNOTATION];
    if (!sig || typeof layer.digest !== "string" || !DIGEST.test(layer.digest)) continue;
    const { body: payload } = await reg.get(`blobs/${layer.digest}`, "*/*");
    if (sha256(payload) !== layer.digest) throw new Error("registry: signature payload digest mismatch");
    if (!verifySig("sha256", payload, key, Buffer.from(sig, "base64"))) continue;
    const claim = JSON.parse(Buffer.from(payload).toString("utf8")) as {
      critical?: { type?: string; identity?: { "docker-reference"?: string }; image?: { "docker-manifest-digest"?: string } };
    };
    if (
      claim.critical?.type === "cosign container image signature" &&
      claim.critical.identity?.["docker-reference"] === input.repo &&
      claim.critical.image?.["docker-manifest-digest"] === digest
    ) {
      return digest;
    }
  }
  throw new Error(`no valid signature for ${input.repo}:${input.tag}`);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const chunks: Buffer[] = [];
  for await (const c of process.stdin) chunks.push(c as Buffer);
  try {
    process.stdout.write(await verifyImage(JSON.parse(Buffer.concat(chunks).toString("utf8")) as VerifyInput));
  } catch (err) {
    process.stderr.write(`${(err as Error).message}\n`);
    process.exit(1);
  }
}

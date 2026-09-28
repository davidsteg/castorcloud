import assert from "node:assert/strict";
import { createHash, generateKeyPairSync, sign } from "node:crypto";
import { test } from "node:test";
import { verifyImage, type Fetch } from "./verify-image.ts";

const REPO = "ghcr.io/davidsteg/castorcloud-cryptd";
const { privateKey, publicKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
const other = generateKeyPairSync("ec", { namedCurve: "P-256" });
const PUB = publicKey.export({ type: "spki", format: "pem" }).toString();
const sha = (b: Buffer) => `sha256:${createHash("sha256").update(b).digest("hex")}`;

/** A fake GHCR: one tag, its signature manifest and payload, behind a token challenge. */
function registry(opts: { repo?: string; key?: typeof privateKey; tamper?: boolean; realm?: string; badHeader?: boolean } = {}) {
  const manifest = Buffer.from(JSON.stringify({ schemaVersion: 2, manifests: [] }));
  const digest = sha(manifest);
  const payload = Buffer.from(JSON.stringify({
    critical: { type: "cosign container image signature", identity: { "docker-reference": opts.repo ?? REPO }, image: { "docker-manifest-digest": digest } },
    optional: null,
  }));
  const signature = sign("sha256", payload, opts.key ?? privateKey).toString("base64");
  const served = opts.tamper ? Buffer.concat([payload, Buffer.from(" ")]) : payload;
  const sigManifest = Buffer.from(JSON.stringify({ layers: [{ digest: sha(payload), annotations: { "dev.cosignproject.cosign/signature": signature } }] }));
  const seen: { url: string; headers: Record<string, string> }[] = [];
  const f: Fetch = async (url, init) => {
    seen.push({ url, headers: init.headers });
    if (url.startsWith("https://ghcr.io/token")) return Response.json({ token: "T" });
    if (init.headers.Authorization !== "Bearer T") {
      return new Response("", { status: 401, headers: { "www-authenticate": `Bearer realm="${opts.realm ?? "https://ghcr.io/token"}",service="ghcr.io",scope="repository:davidsteg/castorcloud-cryptd:pull"` } });
    }
    const path = url.replace("https://ghcr.io/v2/davidsteg/castorcloud-cryptd/", "");
    if (path === "manifests/v1") return new Response(manifest, { headers: { "docker-content-digest": opts.badHeader ? sha(Buffer.from("x")) : digest } });
    if (path === `manifests/${digest.replace(":", "-")}.sig`) return new Response(sigManifest);
    if (path === `blobs/${sha(payload)}`) return new Response(served);
    return new Response("", { status: 404 });
  };
  return { f, digest, seen };
}

const input = { repo: REPO, tag: "v1", pubkey: PUB, user: "castor", token: "read-only" };

test("a signed tag resolves to its digest; credentials go only to the token endpoint", async () => {
  const r = registry();
  assert.equal(await verifyImage(input, r.f), r.digest);
  const basic = r.seen.filter((s) => s.headers.Authorization?.startsWith("Basic "));
  assert.deepEqual(basic.map((s) => new URL(s.url).origin), ["https://ghcr.io"]);
  assert.ok(basic[0].url.startsWith("https://ghcr.io/token"));
});

test("wrong key, other repository or altered payload is refused", async () => {
  await assert.rejects(verifyImage(input, registry({ key: other.privateKey }).f), /no valid signature/);
  await assert.rejects(verifyImage(input, registry({ repo: "ghcr.io/evil/castorcloud-cryptd" }).f), /no valid signature/);
  await assert.rejects(verifyImage(input, registry({ tamper: true }).f), /payload digest mismatch/);
  await assert.rejects(verifyImage(input, registry({ badHeader: true }).f), /manifest digest mismatch/);
});

test("credentials never go to another host or over plain HTTP", async () => {
  await assert.rejects(verifyImage(input, registry({ realm: "https://evil.example/token" }).f), /another host/);
  await assert.rejects(verifyImage(input, registry({ realm: "http://ghcr.io/token" }).f), /insecure/);
});

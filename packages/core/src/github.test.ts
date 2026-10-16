import assert from "node:assert/strict";
import { createHmac, createVerify, generateKeyPairSync } from "node:crypto";
import { test } from "node:test";
import { appJwt, githubAppConfig, installationToken, newAuthorization, verifiedOwnInstallations, verifyWebhookSignature } from "./github.ts";

const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const pem = privateKey.export({ type: "pkcs1", format: "pem" }).toString();

Object.assign(process.env, {
  GITHUB_APP_CLIENT_ID: "Iv23test",
  GITHUB_APP_CLIENT_SECRET: "secret",
  GITHUB_APP_SLUG: "castor-cloud",
  GITHUB_APP_PRIVATE_KEY: pem.replace(/\n/g, "\\n"),
  GITHUB_APP_WEBHOOK_SECRET: "hook",
});

type Call = { method: string; url: string; auth: string | null; body: unknown };

/** Fake GitHub: answers by "METHOD path" and records every request. */
function fakeGitHub(routes: Record<string, (body: unknown) => [number, unknown]>) {
  const calls: Call[] = [];
  const f = (async (input: string | URL, init?: RequestInit) => {
    const url = new URL(String(input));
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    const headers = new Headers(init?.headers);
    calls.push({ method: init?.method ?? "GET", url: url.toString(), auth: headers.get("authorization"), body });
    const route = routes[`${init?.method ?? "GET"} ${url.pathname}`];
    const [status, json] = route ? route(body) : [404, { message: "Not Found" }];
    return new Response(status === 204 ? null : JSON.stringify(json), { status });
  }) as typeof fetch;
  return { f, calls };
}

test("config needs every setting and unescapes the key", () => {
  const cfg = githubAppConfig();
  assert.ok(cfg);
  assert.equal(cfg.privateKey, pem);
  assert.equal(githubAppConfig({ ...process.env, GITHUB_APP_WEBHOOK_SECRET: "" }), null);
});

test("app JWT is RS256, short-lived and signed by the app key", () => {
  const now = Date.UTC(2026, 9, 16, 12);
  const jwt = appJwt(githubAppConfig()!, now);
  const [h, p, s] = jwt.split(".");
  assert.deepEqual(JSON.parse(Buffer.from(h, "base64url").toString()), { alg: "RS256", typ: "JWT" });
  const claims = JSON.parse(Buffer.from(p, "base64url").toString());
  assert.equal(claims.iss, "Iv23test");
  assert.ok(claims.exp - claims.iat <= 600);
  assert.ok(createVerify("RSA-SHA256").update(`${h}.${p}`).verify(publicKey, Buffer.from(s, "base64url")));
});

test("PKCE challenge is the S256 of the verifier, state is random", () => {
  const a = newAuthorization();
  const b = newAuthorization();
  assert.notEqual(a.state, b.state);
  assert.notEqual(a.verifier, a.challenge);
  assert.ok(a.verifier.length >= 43);
});

test("webhook signature: valid, wrong secret, tampered body, missing", () => {
  const body = '{"action":"deleted"}';
  const sig = `sha256=${createHmac("sha256", "hook").update(body).digest("hex")}`;
  assert.equal(verifyWebhookSignature(body, sig, "hook"), true);
  assert.equal(verifyWebhookSignature(body, sig, "other"), false);
  assert.equal(verifyWebhookSignature(`${body} `, sig, "hook"), false);
  assert.equal(verifyWebhookSignature(body, null, "hook"), false);
  assert.equal(verifyWebhookSignature(body, "sha256=abc", "hook"), false);
  assert.equal(verifyWebhookSignature(body, sig, ""), false);
});

const installation = (id: number, accountId: number, extra: object = {}) => ({
  id,
  app_slug: "castor-cloud",
  target_type: "User",
  account: { id: accountId, login: `user${accountId}` },
  repository_selection: "selected",
  suspended_at: null,
  ...extra,
});

test("linking keeps only installations on the user's own account and revokes the user authorization", async () => {
  const { f, calls } = fakeGitHub({
    "POST /login/oauth/access_token": () => [200, { access_token: "ghu_user" }],
    "GET /user": () => [200, { id: 7, login: "user7" }],
    "GET /user/installations": () => [200, {
      installations: [
        installation(1, 7),
        installation(2, 99), // someone else's account the user can see
        installation(3, 7, { target_type: "Organization" }),
        installation(4, 7, { app_slug: "other-app" }),
        installation(5, 7, { repository_selection: "all", suspended_at: "2026-10-01T00:00:00Z" }),
      ],
    }],
    "DELETE /applications/Iv23test/grant": () => [204, null],
  });
  const r = await verifiedOwnInstallations("code", "verifier", "https://app.example/api/github/callback", f);
  assert.equal(r.githubUser.id, 7n);
  assert.deepEqual(r.installations.map((i) => i.installationId), [1n, 5n]);
  assert.equal(r.installations[1].repositorySelection, "all");
  assert.equal(r.installations[1].suspended, true);

  const exchange = calls[0];
  assert.deepEqual(exchange.body, {
    client_id: "Iv23test", client_secret: "secret", code: "code",
    redirect_uri: "https://app.example/api/github/callback", code_verifier: "verifier",
  });
  const revoke = calls.at(-1)!;
  assert.equal(revoke.method, "DELETE");
  assert.deepEqual(revoke.body, { access_token: "ghu_user" });
  assert.equal(revoke.auth, `Basic ${Buffer.from("Iv23test:secret").toString("base64")}`);
});

test("the user authorization is revoked even when GitHub fails mid-way", async () => {
  const { f, calls } = fakeGitHub({
    "POST /login/oauth/access_token": () => [200, { access_token: "ghu_user" }],
    "GET /user": () => [500, {}],
    "DELETE /applications/Iv23test/grant": () => [204, null],
  });
  await assert.rejects(verifiedOwnInstallations("code", "v", "https://x/cb", f));
  assert.equal(calls.at(-1)!.url, "https://api.github.com/applications/Iv23test/grant");
});

test("a refused code exchange fails without calling the API", async () => {
  const { f, calls } = fakeGitHub({
    "POST /login/oauth/access_token": () => [200, { error: "bad_verification_code" }],
  });
  await assert.rejects(verifiedOwnInstallations("code", "v", "https://x/cb", f), /bad_verification_code/);
  assert.equal(calls.length, 1);
});

test("installation tokens are narrowed to the repository and read-only contents", async () => {
  const { f, calls } = fakeGitHub({
    "POST /app/installations/42/access_tokens": () => [201, { token: "ghs_x", expires_at: "2026-10-16T13:00:00Z" }],
  });
  const t = await installationToken(42n, { repositories: ["site"], permissions: { contents: "read" } }, f);
  assert.equal(t.token, "ghs_x");
  assert.deepEqual(calls[0].body, { repositories: ["site"], permissions: { contents: "read" } });
  assert.match(calls[0].auth ?? "", /^Bearer [\w-]+\.[\w-]+\.[\w-]+$/);
});

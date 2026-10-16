// Read access to a customer's GitHub repositories, through the Castor GitHub
// App (docs/github.md). Separate from "Continue with GitHub" (an Authentik
// login that can only read the profile and e-mail).
//
//   - The customer installs the app on their own GitHub account and picks the
//     repositories. The app only asks for "Contents: read" (+ metadata).
//   - We store which installation belongs to which user, never a GitHub token.
//     Each use mints an installation token (valid 1 hour), narrowed to the one
//     repository and to read-only contents.
//   - Linking proves ownership: the user signs in to GitHub (OAuth + PKCE),
//     and only installations on that GitHub account are linked. An
//     installation id from a URL is never trusted.
import { createHash, createHmac, createSign, randomBytes, timingSafeEqual } from "node:crypto";
import { prisma } from "@pc/db";

const API = "https://api.github.com";
const WEB = "https://github.com";
const API_VERSION = "2022-11-28";

export type GitHubAppConfig = {
  clientId: string;
  clientSecret: string;
  /** github.com/apps/<slug> */
  slug: string;
  /** PEM (PKCS#1 or PKCS#8) as downloaded from GitHub; "\n" escapes allowed */
  privateKey: string;
  webhookSecret: string;
};

/** The app's settings from the environment, or null while it isn't set up. */
export function githubAppConfig(env: NodeJS.ProcessEnv = process.env): GitHubAppConfig | null {
  const clientId = env.GITHUB_APP_CLIENT_ID;
  const clientSecret = env.GITHUB_APP_CLIENT_SECRET;
  const slug = env.GITHUB_APP_SLUG;
  const privateKey = env.GITHUB_APP_PRIVATE_KEY?.replace(/\\n/g, "\n");
  const webhookSecret = env.GITHUB_APP_WEBHOOK_SECRET;
  if (!clientId || !clientSecret || !slug || !privateKey || !webhookSecret) return null;
  return { clientId, clientSecret, slug, privateKey, webhookSecret };
}

function requireConfig(): GitHubAppConfig {
  const cfg = githubAppConfig();
  if (!cfg) throw new Error("GitHub App not configured (GITHUB_APP_* in .env)");
  return cfg;
}

export class GitHubError extends Error {
  readonly status: number;
  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

type Fetch = typeof fetch;

async function call<T>(f: Fetch, method: string, url: string, auth: string, body?: unknown): Promise<T> {
  const res = await f(url, {
    method,
    headers: {
      Accept: "application/vnd.github+json",
      Authorization: auth,
      "X-GitHub-Api-Version": API_VERSION,
      "User-Agent": "castor-cloud",
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    redirect: "error",
  });
  if (!res.ok) throw new GitHubError(`GitHub ${method} ${new URL(url).pathname}: ${res.status}`, res.status);
  return (res.status === 204 ? undefined : await res.json()) as T;
}

/** App JWT (RS256, 9 minutes; issued 60 s in the past against clock drift). */
export function appJwt(cfg: Pick<GitHubAppConfig, "clientId" | "privateKey">, nowMs = Date.now()): string {
  const now = Math.floor(nowMs / 1000);
  const part = (o: object) => Buffer.from(JSON.stringify(o)).toString("base64url");
  const unsigned = `${part({ alg: "RS256", typ: "JWT" })}.${part({ iat: now - 60, exp: now + 540, iss: cfg.clientId })}`;
  const signature = createSign("RSA-SHA256").update(unsigned).sign(cfg.privateKey).toString("base64url");
  return `${unsigned}.${signature}`;
}

// ---- Linking (portal: /api/github/connect -> GitHub -> /api/github/callback) ----

/** PKCE pair and state for one connect attempt (kept in a __Host- cookie). */
export function newAuthorization() {
  const verifier = randomBytes(32).toString("base64url");
  return {
    state: randomBytes(32).toString("base64url"),
    verifier,
    challenge: createHash("sha256").update(verifier).digest("base64url"),
  };
}

export function authorizeUrl(cfg: GitHubAppConfig, redirectUri: string, state: string, challenge: string): string {
  const q = new URLSearchParams({
    client_id: cfg.clientId,
    redirect_uri: redirectUri,
    state,
    code_challenge: challenge,
    code_challenge_method: "S256",
    allow_signup: "false",
  });
  return `${WEB}/login/oauth/authorize?${q}`;
}

/** Where the customer installs the app and picks repositories. */
export function installUrl(cfg: GitHubAppConfig): string {
  return `${WEB}/apps/${encodeURIComponent(cfg.slug)}/installations/new`;
}

export type OwnInstallation = {
  installationId: bigint;
  accountId: bigint;
  accountLogin: string;
  repositorySelection: "all" | "selected";
  suspended: boolean;
};

/**
 * Exchanges the OAuth code, then returns the GitHub user and the app's
 * installations on that user's own account. Installations on organisations
 * (or other people's accounts the user can see) are left out: an org member
 * could otherwise hand us read access to repositories they can't read. The
 * user's authorization (access and refresh token) is revoked before
 * returning; neither is ever stored.
 */
export async function verifiedOwnInstallations(code: string, verifier: string, redirectUri: string, f: Fetch = fetch) {
  const cfg = requireConfig();
  const res = await f(`${WEB}/login/oauth/access_token`, {
    method: "POST",
    headers: { Accept: "application/json", "Content-Type": "application/json", "User-Agent": "castor-cloud" },
    body: JSON.stringify({ client_id: cfg.clientId, client_secret: cfg.clientSecret, code, redirect_uri: redirectUri, code_verifier: verifier }),
    redirect: "error",
  });
  const tokenResponse = (await res.json().catch(() => ({}))) as { access_token?: string; error?: string };
  if (!res.ok || !tokenResponse.access_token) throw new GitHubError(`GitHub code exchange: ${tokenResponse.error ?? res.status}`, res.status);
  const userToken = tokenResponse.access_token;
  try {
    const auth = `Bearer ${userToken}`;
    const user = await call<{ id: number; login: string }>(f, "GET", `${API}/user`, auth);
    const installations: OwnInstallation[] = [];
    for (let page = 1; page <= 10; page++) {
      const r = await call<{ installations: RawInstallation[] }>(f, "GET", `${API}/user/installations?per_page=100&page=${page}`, auth);
      for (const i of r.installations) {
        if (i.app_slug !== cfg.slug || i.account?.id !== user.id || i.target_type !== "User") continue;
        installations.push({
          installationId: BigInt(i.id),
          accountId: BigInt(i.account.id),
          accountLogin: i.account.login,
          repositorySelection: i.repository_selection === "all" ? "all" : "selected",
          suspended: Boolean(i.suspended_at),
        });
      }
      if (r.installations.length < 100) break;
    }
    return { githubUser: { id: BigInt(user.id), login: user.login }, installations };
  } finally {
    await revokeUserGrant(cfg, userToken, f).catch(() => {});
  }
}

type RawInstallation = {
  id: number;
  app_slug: string;
  target_type: string;
  account: { id: number; login: string } | null;
  repository_selection: string;
  suspended_at: string | null;
};

/** Deletes the user's authorization of the app, with every token it issued. Installations stay. */
async function revokeUserGrant(cfg: GitHubAppConfig, token: string, f: Fetch) {
  const basic = Buffer.from(`${cfg.clientId}:${cfg.clientSecret}`).toString("base64");
  await call(f, "DELETE", `${API}/applications/${encodeURIComponent(cfg.clientId)}/grant`, `Basic ${basic}`, { access_token: token });
}

// ---- Using an installation ----

export type RepoToken = { token: string; expiresAt: Date };

/**
 * Installation token narrowed to the given repositories (names without the
 * owner) and permissions. Valid 1 hour; never store it.
 */
export async function installationToken(
  installationId: bigint,
  scope: { repositories?: string[]; permissions: Record<string, "read"> },
  f: Fetch = fetch,
): Promise<RepoToken> {
  const cfg = requireConfig();
  const r = await call<{ token: string; expires_at: string }>(
    f, "POST", `${API}/app/installations/${installationId}/access_tokens`, `Bearer ${appJwt(cfg)}`,
    { ...(scope.repositories ? { repositories: scope.repositories } : {}), permissions: scope.permissions },
  );
  return { token: r.token, expiresAt: new Date(r.expires_at) };
}

export type Repo = { id: number; fullName: string; private: boolean; defaultBranch: string };

/** Repositories the customer gave the installation (metadata only). */
export async function installationRepositories(installationId: bigint, f: Fetch = fetch): Promise<Repo[]> {
  const { token } = await installationToken(installationId, { permissions: { metadata: "read" } }, f);
  const repos: Repo[] = [];
  for (let page = 1; page <= 10; page++) {
    const r = await call<{ repositories: { id: number; full_name: string; private: boolean; default_branch: string }[] }>(
      f, "GET", `${API}/installation/repositories?per_page=100&page=${page}`, `Bearer ${token}`,
    );
    repos.push(...r.repositories.map((x) => ({ id: x.id, fullName: x.full_name, private: x.private, defaultBranch: x.default_branch })));
    if (r.repositories.length < 100) break;
  }
  return repos;
}

const REPO_NAME_RE = /^[A-Za-z0-9-]{1,39}\/[A-Za-z0-9._-]{1,100}$/;

/**
 * Read-only token for one of the user's own repositories ("owner/name"), e.g.
 * to clone it for a custom stack. Null when the user hasn't connected that
 * GitHub account, or the installation is suspended. GitHub refuses (404/422)
 * when the repository isn't part of the installation.
 */
export async function repoReadToken(userId: string, fullName: string, f: Fetch = fetch): Promise<RepoToken | null> {
  if (!REPO_NAME_RE.test(fullName)) return null;
  const [owner, name] = fullName.split("/");
  const link = await prisma.gitHubInstallation.findFirst({
    where: { userId, accountLogin: { equals: owner, mode: "insensitive" }, suspendedAt: null },
  });
  if (!link) return null;
  return installationToken(link.installationId, { repositories: [name], permissions: { contents: "read" } }, f);
}

/** Uninstalls the app from the customer's account (disconnect in the portal). */
export async function deleteInstallation(installationId: bigint, f: Fetch = fetch): Promise<void> {
  const cfg = requireConfig();
  try {
    await call(f, "DELETE", `${API}/app/installations/${installationId}`, `Bearer ${appJwt(cfg)}`);
  } catch (err) {
    // Already uninstalled on GitHub.
    if (!(err instanceof GitHubError && err.status === 404)) throw err;
  }
}

/** Checks X-Hub-Signature-256 (HMAC-SHA256 of the raw body). */
export function verifyWebhookSignature(body: string, header: string | null, secret: string): boolean {
  if (!header?.startsWith("sha256=") || !secret) return false;
  const expected = Buffer.from(createHmac("sha256", secret).update(body).digest("hex"));
  const given = Buffer.from(header.slice("sha256=".length));
  return given.length === expected.length && timingSafeEqual(given, expected);
}

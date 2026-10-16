import "server-only";
import { timingSafeEqual } from "node:crypto";
import { cookies } from "next/headers";
import { authorizeUrl, githubAppConfig, newAuthorization } from "@pc/core";
import { isLocale, DEFAULT_LOCALE, type Locale } from "@/i18n";
import { PUBLIC_SCHEME, appUrl } from "@/lib/config";

// One connect attempt (docs/github.md): state and PKCE verifier in a
// short-lived cookie, bound to the signed-in user. "__Host-" keeps tenant and
// service hosts (*.<domain>) from planting their own.
const COOKIE = PUBLIC_SCHEME === "https" ? "__Host-pc-github" : "pc-github";
const MAX_AGE_S = 10 * 60;

export const GITHUB_CALLBACK_URL = appUrl("/api/github/callback");

type Pending = { state: string; verifier: string; userId: string; locale: Locale; expires: number };

/** Starts linking: remembers the attempt and returns GitHub's sign-in URL. */
export async function startGitHubConnect(userId: string, locale: Locale): Promise<string | null> {
  const cfg = githubAppConfig();
  if (!cfg) return null;
  const { state, verifier, challenge } = newAuthorization();
  const pending: Pending = { state, verifier, userId, locale, expires: Date.now() + MAX_AGE_S * 1000 };
  (await cookies()).set(COOKIE, Buffer.from(JSON.stringify(pending)).toString("base64url"), {
    httpOnly: true,
    secure: PUBLIC_SCHEME === "https",
    sameSite: "lax",
    path: "/",
    maxAge: MAX_AGE_S,
  });
  return authorizeUrl(cfg, GITHUB_CALLBACK_URL, state, challenge);
}

/** Reads and clears the pending attempt; null unless it matches state and user. */
export async function takeGitHubConnect(state: string | null, userId: string | undefined): Promise<Pending | null> {
  const jar = await cookies();
  const raw = jar.get(COOKIE)?.value;
  jar.delete({ name: COOKIE, path: "/", secure: PUBLIC_SCHEME === "https", httpOnly: true, sameSite: "lax" });
  if (!raw || !state || !userId) return null;
  let pending: Pending;
  try {
    pending = JSON.parse(Buffer.from(raw, "base64url").toString("utf8"));
  } catch {
    return null;
  }
  if (typeof pending.state !== "string" || typeof pending.verifier !== "string" || pending.expires < Date.now()) return null;
  const a = Buffer.from(pending.state);
  const b = Buffer.from(state);
  if (a.length !== b.length || !timingSafeEqual(a, b)) return null;
  if (pending.userId !== userId) return null;
  return { ...pending, locale: isLocale(pending.locale) ? pending.locale : DEFAULT_LOCALE };
}

export function accountUrl(locale: Locale, result: "connected" | "error" | "disconnected") {
  return appUrl(`/${locale}/account?github=${result}#github`);
}

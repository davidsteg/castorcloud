// Direct database access, used only to look up a member's account in a
// service and to issue that member a read-only API key. The services have no
// admin API for either: accounts appear on first SSO login, and API keys can
// only be created by the user themselves.
import { createHash, randomBytes } from "node:crypto";
import pg from "pg";

const pools = new Map<string, pg.Pool>();

export function pool(url: string) {
  let p = pools.get(url);
  if (!p) {
    p = new pg.Pool({ connectionString: url, max: 2, idleTimeoutMillis: 30_000 });
    pools.set(url, p);
  }
  return p;
}

/** Name of every key we issue, so re-issuing replaces the previous one. */
export const KEY_NAME = "Castor assistant (read-only)";

export const newApiKey = () => randomBytes(30).toString("base64url");
export const sha256 = (value: string) => createHash("sha256").update(value).digest();

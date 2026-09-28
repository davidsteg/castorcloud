// Newsletter double opt-in (docs/beta.md): address checks, confirmation
// tokens (only their hash is stored) and unsubscribe tokens (an HMAC of the
// subscriber id, so every mail can carry a one-click link without a stored
// token). Plus the small rate limiter the public subscribe form uses.
import { createHash, createHmac, randomBytes, timingSafeEqual } from "node:crypto";

const EMAIL_MAX = 254;
// Deliberately strict: one @, no spaces, quotes or brackets, a dotted domain
// of letters, digits and hyphens (IDN domains in their xn-- form).
const EMAIL_RE = /^[a-z0-9.!#$%&'*+/=?^_`{|}~-]{1,64}@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z0-9-]{2,63}$/;

/** Trimmed, lower-cased address, or null if it isn't one we accept. */
export function normalizeEmail(input: unknown): string | null {
  if (typeof input !== "string") return null;
  const email = input.trim().toLowerCase();
  if (email.length > EMAIL_MAX || !EMAIL_RE.test(email)) return null;
  if (email.startsWith(".") || email.includes("..") || email.split("@")[0].endsWith(".")) return null;
  return email;
}

export function hashToken(token: string): string {
  return createHash("sha256").update(token).digest("hex");
}

/** A fresh confirmation token for the link, and the hash to store. */
export function newConfirmToken(): { token: string; hash: string } {
  const token = randomBytes(32).toString("base64url");
  return { token, hash: hashToken(token) };
}

export function unsubscribeToken(secret: string, subscriberId: string): string {
  if (!secret) throw new Error("unsubscribe tokens need a secret");
  return createHmac("sha256", secret).update(`newsletter-unsubscribe\0${subscriberId}`).digest("base64url");
}

export function verifyUnsubscribeToken(secret: string, subscriberId: string, token: unknown): boolean {
  if (typeof token !== "string" || !subscriberId) return false;
  const expected = Buffer.from(unsubscribeToken(secret, subscriberId));
  const given = Buffer.from(token);
  return given.length === expected.length && timingSafeEqual(given, expected);
}

/**
 * Fixed-window counter per key, in memory (one portal process). Returns
 * whether another hit is allowed and records it if so.
 */
export class RateLimiter {
  private hits = new Map<string, { count: number; resetAt: number }>();
  private readonly limit: number;
  private readonly windowMs: number;

  constructor(limit: number, windowMs: number) {
    this.limit = limit;
    this.windowMs = windowMs;
  }

  take(key: string, now = Date.now()): boolean {
    const entry = this.hits.get(key);
    if (!entry || entry.resetAt <= now) {
      if (this.hits.size > 10_000) this.prune(now);
      this.hits.set(key, { count: 1, resetAt: now + this.windowMs });
      return true;
    }
    if (entry.count >= this.limit) return false;
    entry.count++;
    return true;
  }

  private prune(now: number) {
    for (const [key, entry] of this.hits) if (entry.resetAt <= now) this.hits.delete(key);
  }
}

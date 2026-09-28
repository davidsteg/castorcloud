import "server-only";
import { headers } from "next/headers";
import { assistantApi, assistantKeyName, openSecret } from "@pc/core";
import { prisma } from "@pc/db";
import { auth } from "@/auth";
import { AI_BUDGET_WARN_AT } from "@pc/pricing";
import { BASE_DOMAIN, PUBLIC_SCHEME, tenantSlugFromHost } from "@/lib/config";

// The portal talks to a member's assistant (Hermes, in the tenant stack) on the
// member's behalf. The per-member API key never reaches the browser: the
// browser calls /api/assistant/*, these helpers add the key server-side.

/** Hermes session ids, e.g. api_1790580220_72a70e02. */
export const SESSION_ID_RE = /^[A-Za-z0-9_-]{1,100}$/;

const LLM_GATEWAY_URL = process.env.LLM_GATEWAY_URL ?? "http://localhost:4000";

async function secret(tenantId: string, key: string) {
  const row = await prisma.secret.findUnique({ where: { tenantId_key: { tenantId, key } } });
  return row ? openSecret(tenantId, key, row.ciphertext) : undefined;
}

/**
 * Rejects cross-origin writes. Every tenant and service host shares the
 * registrable domain, so SameSite cookies alone don't stop another host from
 * posting here with the member's session.
 */
export function sameOrigin(req: Request) {
  try {
    return new URL(req.headers.get("origin") ?? "").host === req.headers.get("host");
  } catch {
    return false; // no or opaque ("null") origin
  }
}

export type Assistant = NonNullable<Awaited<ReturnType<typeof memberAssistant>>>;

/**
 * The signed-in member's assistant on this tenant host, or null when there is
 * none (not a member, assistant not booked, tenant not active).
 */
export async function memberAssistant(slug?: string) {
  slug ??= tenantSlugFromHost((await headers()).get("host")) ?? undefined;
  const sub = (await auth())?.user?.id;
  if (!slug || !sub) return null;

  const tenant = await prisma.tenant.findUnique({
    where: { slug },
    include: { members: { include: { user: true } }, services: true },
  });
  if (!tenant || tenant.status !== "ACTIVE") return null;
  if (!tenant.services.some((s) => s.kind === "ASSISTANT" && s.enabled)) return null;
  const member = tenant.members.find((m) => m.user.authentikSub === sub);
  if (!member) return null;
  const apiKey = await secret(tenant.id, assistantKeyName(member.id));
  if (!apiKey) return null;

  const base = assistantApi({ scheme: PUBLIC_SCHEME, baseDomain: BASE_DOMAIN, slug, memberId: member.id });
  return {
    tenant,
    member,
    /** Calls the member's Hermes profile. */
    hermes(path: string, init: RequestInit = {}) {
      return fetch(`${base}${path}`, {
        ...init,
        headers: { Authorization: `Bearer ${apiKey}`, "Content-Type": "application/json", ...init.headers },
        cache: "no-store",
      });
    },
  };
}

export type Credit = {
  /** Share of this month's credit used, 0..1. */
  used: number;
  state: "ok" | "low" | "exhausted";
  /** When the credit resets (ISO), if the gateway knows. */
  resetsAt: string | null;
};

/**
 * This month's AI credit, read from the tenant's own key on the LLM gateway
 * (a key may read its own info). Null when the gateway can't be reached.
 */
export async function tenantCredit(tenantId: string): Promise<Credit | null> {
  const key = await secret(tenantId, "LLM_VIRTUAL_KEY");
  if (!key) return null;
  try {
    const res = await fetch(`${LLM_GATEWAY_URL}/key/info`, {
      headers: { Authorization: `Bearer ${key}` },
      cache: "no-store",
      signal: AbortSignal.timeout(5000),
    });
    if (!res.ok) return null;
    const { info } = (await res.json()) as { info?: { spend?: number; max_budget?: number | null; budget_reset_at?: string | null } };
    if (!info?.max_budget) return null;
    const used = Math.min(1, (info.spend ?? 0) / info.max_budget);
    return {
      used,
      state: used >= 1 ? "exhausted" : used >= AI_BUDGET_WARN_AT ? "low" : "ok",
      resetsAt: info.budget_reset_at ?? null,
    };
  } catch {
    return null;
  }
}

/** First day of next month: LiteLLM resets "1mo" budgets on the 1st. */
export function nextReset(credit: Credit | null) {
  if (credit?.resetsAt) return new Date(credit.resetsAt);
  const now = new Date();
  return new Date(now.getFullYear(), now.getMonth() + 1, 1);
}

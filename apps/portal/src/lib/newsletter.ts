import "server-only";
import { prisma, type NewsletterSubscriber } from "@pc/db";
import { hashToken, newConfirmToken, unsubscribeToken, verifyUnsubscribeToken } from "@pc/core";
import { fill, getDictionary, isLocale, type Locale } from "@/i18n";
import { appUrl } from "@/lib/config";
import { trySend } from "@/lib/mail";

// Newsletter with double opt-in (docs/beta.md): an address is only mailed
// once its owner clicked the confirmation link; every mail carries a
// one-click unsubscribe link (RFC 8058).

const CONFIRM_TTL_MS = 7 * 24 * 3600_000;
/** At most one confirmation mail per address in this time. */
const RESEND_AFTER_MS = 15 * 60_000;

function secret(): string {
  const s = process.env.AUTH_SECRET;
  if (!s) throw new Error("AUTH_SECRET is not set");
  return s;
}

/**
 * Starts (or restarts) a subscription and sends the confirmation mail. The
 * caller shows the same answer whatever happens here, so the form doesn't
 * reveal which addresses are subscribed.
 */
export async function subscribe(email: string, locale: Locale, source: "form" | "signup" | "account") {
  const existing = await prisma.newsletterSubscriber.findUnique({ where: { email } });
  if (existing?.status === "CONFIRMED") return;
  if (existing?.confirmSentAt && Date.now() - existing.confirmSentAt.getTime() < RESEND_AFTER_MS) return;

  const { token, hash } = newConfirmToken();
  const data = { locale, status: "PENDING" as const, confirmTokenHash: hash, confirmSentAt: new Date(), source };
  await prisma.newsletterSubscriber.upsert({ where: { email }, update: data, create: { email, ...data } });

  const t = getDictionary(locale).mail;
  await trySend({
    to: email,
    subject: t.newsletterSubject,
    text: `${t.greeting}\n\n${fill(t.newsletterBody, { link: appUrl(`/${locale}/newsletter/confirm?t=${token}`) })}\n\n${t.signature}\n`,
  });
}

/** Confirms the subscription the token belongs to. */
export async function confirm(token: unknown): Promise<boolean> {
  if (typeof token !== "string" || token.length > 100) return false;
  const sub = await prisma.newsletterSubscriber.findUnique({ where: { confirmTokenHash: hashToken(token) } });
  if (!sub || sub.status !== "PENDING" || !sub.confirmSentAt) return false;
  if (Date.now() - sub.confirmSentAt.getTime() > CONFIRM_TTL_MS) return false;
  // Conditional on the token still being there, so it works exactly once.
  const { count } = await prisma.newsletterSubscriber.updateMany({
    where: { id: sub.id, confirmTokenHash: sub.confirmTokenHash },
    data: { status: "CONFIRMED", confirmedAt: new Date(), confirmTokenHash: null, unsubscribedAt: null },
  });
  return count === 1;
}

/** Unsubscribes with the id and token from an unsubscribe link. */
export async function unsubscribeWithToken(id: unknown, token: unknown): Promise<boolean> {
  if (typeof id !== "string" || !verifyUnsubscribeToken(secret(), id, token)) return false;
  await unsubscribe(id);
  return true;
}

export async function unsubscribe(id: string) {
  await prisma.newsletterSubscriber.updateMany({
    where: { id, status: { not: "UNSUBSCRIBED" } },
    data: { status: "UNSUBSCRIBED", unsubscribedAt: new Date(), confirmTokenHash: null },
  });
}

/** The page behind the unsubscribe link in every newsletter. */
export function unsubscribeUrl(sub: Pick<NewsletterSubscriber, "id" | "locale">): string {
  const locale = isLocale(sub.locale) ? sub.locale : "de";
  return appUrl(`/${locale}/newsletter/unsubscribe?id=${sub.id}&t=${unsubscribeToken(secret(), sub.id)}`);
}

/** Headers for one-click unsubscribe in mail clients (RFC 2369, RFC 8058). */
export function listUnsubscribeHeaders(sub: Pick<NewsletterSubscriber, "id">): Record<string, string> {
  const url = appUrl(`/api/newsletter/unsubscribe?id=${sub.id}&t=${unsubscribeToken(secret(), sub.id)}`);
  return { "List-Unsubscribe": `<${url}>`, "List-Unsubscribe-Post": "List-Unsubscribe=One-Click" };
}

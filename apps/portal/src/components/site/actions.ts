"use server";

import { headers } from "next/headers";
import { RateLimiter, normalizeEmail } from "@pc/core";
import { isLocale } from "@/i18n";
import { subscribe } from "@/lib/newsletter";

export type NewsletterState = { result: "sent" | "invalid" | "busy" } | undefined;

// The form is public: limit confirmation mails per client and overall, so
// it can't be used to flood inboxes or burn the sender's reputation.
const perClient = new RateLimiter(5, 3600_000);
const overall = new RateLimiter(300, 3600_000);

export async function subscribeNewsletter(_prev: NewsletterState, form: FormData): Promise<NewsletterState> {
  // Honeypot: a field people don't see; bots fill it in. Pretend it worked.
  if (form.get("website")) return { result: "sent" };
  const email = normalizeEmail(form.get("email"));
  if (!email) return { result: "invalid" };
  const locale = String(form.get("locale"));

  // Traefik sets X-Real-IP to the client address.
  const client = (await headers()).get("x-real-ip") ?? "unknown";
  if (!perClient.take(client) || !overall.take("all")) return { result: "busy" };

  await subscribe(email, isLocale(locale) ? locale : "de", "form");
  return { result: "sent" };
}

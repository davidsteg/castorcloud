"use client";

import { useActionState } from "react";
import type { Dictionary, Locale } from "@/i18n";
import { subscribeNewsletter, type NewsletterState } from "./actions";

export function NewsletterForm({ locale, t }: { locale: Locale; t: Dictionary["newsletter"] }) {
  const [state, action, pending] = useActionState<NewsletterState, FormData>(subscribeNewsletter, undefined);

  if (state?.result === "sent") return <p className="notice" role="status">{t.sent}</p>;

  return (
    <form action={action} className="newsletter-form">
      <input type="hidden" name="locale" value={locale} />
      <div className="hp" aria-hidden>
        <label htmlFor="nl-website">Website</label>
        <input id="nl-website" name="website" tabIndex={-1} autoComplete="off" />
      </div>
      <label htmlFor="nl-email" className="visually-hidden">{t.email}</label>
      <div className="newsletter-row">
        <input id="nl-email" name="email" type="email" className="input" required maxLength={254}
          autoComplete="email" placeholder={t.email} />
        <button className="button" type="submit" disabled={pending}>{t.subscribe}</button>
      </div>
      {state?.result === "invalid" && <span className="error">{t.invalid}</span>}
      {state?.result === "busy" && <span className="error">{t.busy}</span>}
      <span className="small-print">{t.consent}</span>
    </form>
  );
}

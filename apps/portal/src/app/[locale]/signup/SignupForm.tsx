"use client";

import { useActionState, useEffect, useState } from "react";
import type { Dictionary, Locale } from "@/i18n";
import { BASE_DOMAIN_PUBLIC } from "@/lib/public-config";
import { SLUG_RE, suggestSlug } from "@/lib/slug";
import { createCloud, isSlugAvailable, type SignupState } from "./actions";

export function SignupForm({ locale, t, config, devMode, beta }: { locale: Locale; t: Dictionary; config: string; devMode: boolean; beta: boolean }) {
  const [state, action, pending] = useActionState<SignupState, FormData>(createCloud, undefined);
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [slugTouched, setSlugTouched] = useState(false);
  const [available, setAvailable] = useState<boolean | null>(null);
  const s = t.signup;

  useEffect(() => {
    if (!slugTouched) setSlug(suggestSlug(name));
  }, [name, slugTouched]);

  useEffect(() => {
    setAvailable(null);
    if (!SLUG_RE.test(slug)) return;
    const timer = setTimeout(() => isSlugAvailable(slug).then(setAvailable), 300);
    return () => clearTimeout(timer);
  }, [slug]);

  const slugError = slug && !SLUG_RE.test(slug) ? s.invalid : available === false || state?.error === "taken" ? s.taken : null;

  return (
    <form action={action} className="stack">
      <input type="hidden" name="locale" value={locale} />
      <input type="hidden" name="config" value={config} />
      <div className="field">
        <label htmlFor="name">{s.household}</label>
        <input id="name" name="name" className="input" required maxLength={60}
          placeholder={s.householdPlaceholder} value={name} onChange={(e) => setName(e.target.value)} />
      </div>
      <div className="field">
        <label htmlFor="slug">{s.address}</label>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <input id="slug" name="slug" className="input" required value={slug}
            onChange={(e) => { setSlugTouched(true); setSlug(e.target.value.toLowerCase()); }} />
          <span className="muted" style={{ whiteSpace: "nowrap" }}>.{BASE_DOMAIN_PUBLIC}</span>
        </div>
        {slugError ? <span className="error">{slugError}</span> : <span className="hint">{s.addressHint}</span>}
      </div>
      {beta && (
        <div className="field">
          <label htmlFor="note">{t.apply.note}</label>
          <textarea id="note" name="note" className="input" rows={3} maxLength={500} placeholder={t.apply.notePlaceholder} />
        </div>
      )}
      <label className="check">
        <input type="checkbox" name="accept" required />
        <span>{s.accept}</span>
      </label>
      {beta && (
        <label className="check">
          <input type="checkbox" name="beta" required />
          <span>{t.beta.accept}</span>
        </label>
      )}
      <label className="check">
        <input type="checkbox" name="newsletter" />
        <span>{t.newsletter.signupOptIn}</span>
      </label>
      {state?.error === "payment" && <p className="notice">Stripe is not configured (STRIPE_SECRET_KEY).</p>}
      {state?.error === "tooMany" && <p className="notice">{t.apply.tooMany}</p>}
      <button className="button block" type="submit" disabled={pending || !!slugError || available === false}>
        {beta ? t.apply.submit : devMode ? s.payDev : s.pay}
      </button>
    </form>
  );
}

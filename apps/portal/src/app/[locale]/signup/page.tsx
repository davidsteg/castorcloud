import Link from "next/link";
import { STORAGE_UNIT_GB, formatChf, quote } from "@pc/pricing";
import { auth, signIn } from "@/auth";
import { fill, getDictionary, type Locale } from "@/i18n";
import { configFromQuery, configToQuery } from "@/lib/plan";
import { stripe } from "@/lib/stripe";
import { SiteHeader } from "@/components/site/SiteHeader";
import { SignupForm } from "./SignupForm";

export default async function SignupPage({
  params,
  searchParams,
}: {
  params: Promise<{ locale: Locale }>;
  searchParams: Promise<Record<string, string | string[] | undefined>>;
}) {
  const { locale } = await params;
  const t = getDictionary(locale);
  const config = configFromQuery(await searchParams);
  const query = configToQuery(config);
  const q = quote(config);
  const session = await auth();
  const p = t.pricing;

  async function login() {
    "use server";
    await signIn("authentik", { redirectTo: `/${locale}/signup?${query}` });
  }

  return (
    <>
      <SiteHeader locale={locale} t={t} path={`/signup?${query}`} />
      <main className="flow">
        <h1>{t.signup.title}</h1>
        <p className="lead">{session ? t.signup.lead : t.signup.signInFirst}</p>

        <div className="panel" style={{ marginBottom: 24 }}>
          <h3 style={{ display: "flex", justifyContent: "space-between" }}>
            {t.signup.summary}
            <Link className="text-link" href={`/${locale}/pricing?${query}`}>{t.signup.change}</Link>
          </h3>
          <ul className="summary-list">
            {config.services.map((s) => <li key={s}><span>{t.services.items[s].name}</span></li>)}
            {config.aiCreditChf > 0 && <li><span>{p.aiCredit}</span><span>CHF {config.aiCreditChf} {p.perMonth}</span></li>}
            <li><span>{p.storage}</span><span>{config.storageUnits * STORAGE_UNIT_GB} GB</span></li>
            <li><span>{p.members}</span><span>{fill(p.membersHint, { n: config.extraMembers })}</span></li>
            <li className="sum">
              <span>{config.interval === "year" ? p.yearly : p.monthly}</span>
              <span>{formatChf(q.chargeRp)} {config.interval === "year" ? p.perYear : p.perMonth}</span>
            </li>
          </ul>
          <p className="small-print" style={{ marginTop: 8 }}>{p.vatIncluded}</p>
        </div>

        {session ? (
          <SignupForm locale={locale} t={t} config={query} devMode={!stripe()} />
        ) : (
          <form action={login}>
            <button className="button block" type="submit">{t.signup.signIn}</button>
          </form>
        )}
      </main>
    </>
  );
}

import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getDictionary, type Locale } from "@/i18n";
import { unsubscribeWithToken } from "@/lib/newsletter";
import { SiteHeader } from "@/components/site/SiteHeader";

// The link in every newsletter. Like confirming, unsubscribing takes a click
// here; mail clients use the one-click POST in /api/newsletter/unsubscribe.
export const metadata: Metadata = { referrer: "no-referrer", robots: { index: false } };

export default async function UnsubscribePage({
  params,
  searchParams,
}: {
  params: Promise<{ locale: Locale }>;
  searchParams: Promise<{ id?: string; t?: string; done?: string }>;
}) {
  const { locale } = await params;
  const { id, t: token, done } = await searchParams;
  const t = getDictionary(locale).newsletter;

  async function unsubscribeAction(form: FormData) {
    "use server";
    const ok = await unsubscribeWithToken(form.get("id"), form.get("token"));
    redirect(`/${locale}/newsletter/unsubscribe?done=${ok ? "1" : "0"}`);
  }

  return (
    <>
      <SiteHeader locale={locale} t={getDictionary(locale)} />
      <main className="flow">
        <h1>{t.unsubscribeTitle}</h1>
        {done === "1" ? (
          <p className="notice" role="status">{t.unsubscribed}</p>
        ) : done === "0" || !id || !token ? (
          <p className="notice" role="status">{t.unsubscribeInvalid}</p>
        ) : (
          <form action={unsubscribeAction}>
            <input type="hidden" name="id" value={id} />
            <input type="hidden" name="token" value={token} />
            <button className="button block" type="submit">{t.unsubscribeButton}</button>
          </form>
        )}
      </main>
    </>
  );
}

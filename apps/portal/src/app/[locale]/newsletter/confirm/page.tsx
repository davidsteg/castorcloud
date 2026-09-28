import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getDictionary, type Locale } from "@/i18n";
import { confirm } from "@/lib/newsletter";
import { SiteHeader } from "@/components/site/SiteHeader";

// The link only opens this page; confirming takes a click, so mail scanners
// that fetch links don't subscribe anyone.
export const metadata: Metadata = { referrer: "no-referrer", robots: { index: false } };

export default async function ConfirmPage({
  params,
  searchParams,
}: {
  params: Promise<{ locale: Locale }>;
  searchParams: Promise<{ t?: string; done?: string }>;
}) {
  const { locale } = await params;
  const { t: token, done } = await searchParams;
  const t = getDictionary(locale).newsletter;

  async function confirmAction(form: FormData) {
    "use server";
    redirect(`/${locale}/newsletter/confirm?done=${(await confirm(form.get("token"))) ? "1" : "0"}`);
  }

  return (
    <>
      <SiteHeader locale={locale} t={getDictionary(locale)} />
      <main className="flow">
        <h1>{t.confirmTitle}</h1>
        {done === "1" ? (
          <p className="notice" role="status">{t.confirmed}</p>
        ) : done === "0" || !token ? (
          <p className="notice" role="status">{t.confirmInvalid}</p>
        ) : (
          <form action={confirmAction}>
            <input type="hidden" name="token" value={token} />
            <button className="button block" type="submit">{t.confirmButton}</button>
          </form>
        )}
      </main>
    </>
  );
}

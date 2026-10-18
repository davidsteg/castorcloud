import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { getDictionary, type Locale } from "@/i18n";
import { SiteHeader } from "@/components/site/SiteHeader";
import { SiteFooter } from "@/components/site/SiteFooter";
import { pageMetadata } from "@/lib/seo";

// Legal documents. The texts must come from legal counsel before launch
// (Impressum per UWG, privacy policy per revDSG, terms); this page only
// reserves the routes linked from the footer.
const DOCS = ["legal", "privacy", "terms"] as const;

function docTitle(t: ReturnType<typeof getDictionary>, doc: (typeof DOCS)[number]) {
  return { legal: t.footer.legal, privacy: t.footer.privacy, terms: t.footer.terms }[doc];
}

// Kept out of search results while the texts are drafts.
export async function generateMetadata({ params }: { params: Promise<{ locale: Locale; doc: string }> }): Promise<Metadata> {
  const { locale, doc } = await params;
  if (!(DOCS as readonly string[]).includes(doc)) return {};
  const t = getDictionary(locale);
  return pageMetadata(locale, `/${doc}`, { title: docTitle(t, doc as (typeof DOCS)[number]), description: t.meta.description, index: false });
}

export default async function LegalDoc({ params }: { params: Promise<{ locale: Locale; doc: string }> }) {
  const { locale, doc } = await params;
  if (!(DOCS as readonly string[]).includes(doc)) notFound();
  const t = getDictionary(locale);
  const title = docTitle(t, doc as (typeof DOCS)[number]);

  return (
    <>
      <SiteHeader locale={locale} t={t} path={`/${doc}`} />
      <main className="flow">
        <h1>{title}</h1>
        <p className="notice">Draft – text to be provided by legal counsel before launch.</p>
      </main>
      <SiteFooter locale={locale} t={t} />
    </>
  );
}

import type { Metadata } from "next";
import { getDictionary, type Locale } from "@/i18n";
import { SubPage } from "@/components/site/SubPage";
import { OssGallery } from "@/components/site/OssGallery";
import { pageMetadata } from "@/lib/seo";

export async function generateMetadata({ params }: { params: Promise<{ locale: Locale }> }): Promise<Metadata> {
  const { locale } = await params;
  const t = getDictionary(locale);
  return pageMetadata(locale, "/open-source", { title: t.openSource.title, description: t.openSource.lead });
}

export default async function OpenSourcePage({ params }: { params: Promise<{ locale: Locale }> }) {
  const { locale } = await params;
  const t = getDictionary(locale);
  const o = t.openSourcePage;

  return (
    <SubPage locale={locale} t={t} path="/open-source" eyebrow="Open Source" title={t.openSource.title} lead={t.openSource.lead}>
      <h2 className="sub-heading">{o.rolesTitle}</h2>
      <OssGallery t={t} />
      <div className="panel" style={{ marginTop: 24 }}>
        <h3 style={{ fontSize: 17, marginBottom: 8 }}>{o.donationTitle}</h3>
        <p className="muted" style={{ lineHeight: 1.6 }}>{o.donationText}</p>
      </div>
    </SubPage>
  );
}

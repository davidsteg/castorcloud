import { getDictionary, type Locale } from "@/i18n";
import { SubPage } from "@/components/site/SubPage";
import { OssGallery } from "@/components/site/OssGallery";

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

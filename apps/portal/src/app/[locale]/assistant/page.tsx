import type { Metadata } from "next";
import { getDictionary, type Locale } from "@/i18n";
import { SubPage } from "@/components/site/SubPage";
import { AssistantInfo } from "@/components/site/AssistantInfo";
import { pageMetadata } from "@/lib/seo";

export async function generateMetadata({ params }: { params: Promise<{ locale: Locale }> }): Promise<Metadata> {
  const { locale } = await params;
  const t = getDictionary(locale);
  return pageMetadata(locale, "/assistant", { title: t.assistantInfo.title, description: t.assistantInfo.lead });
}

export default async function AssistantPage({ params }: { params: Promise<{ locale: Locale }> }) {
  const { locale } = await params;
  const t = getDictionary(locale);
  return (
    <SubPage
      locale={locale} t={t} path="/assistant"
      eyebrow={t.services.items.assistant.name} title={t.assistantInfo.title} lead={t.assistantInfo.lead}
    >
      <AssistantInfo t={t} />
      <div className="panel" style={{ marginTop: 16 }}>
        <h3 style={{ fontSize: 17, marginBottom: 8 }}>{t.pricing.aiCredit}</h3>
        <p className="muted" style={{ lineHeight: 1.55 }}>{t.pricing.aiCreditHint}</p>
      </div>
    </SubPage>
  );
}

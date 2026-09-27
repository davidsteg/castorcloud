import { getDictionary, type Locale } from "@/i18n";
import { SubPage } from "@/components/site/SubPage";
import { FaqList } from "@/components/site/FaqList";

export default async function FaqPage({ params }: { params: Promise<{ locale: Locale }> }) {
  const { locale } = await params;
  const t = getDictionary(locale);
  return (
    <SubPage locale={locale} t={t} path="/faq" eyebrow={t.nav.faq} title={t.faq.title}>
      <FaqList t={t} />
    </SubPage>
  );
}

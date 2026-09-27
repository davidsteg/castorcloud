import { getDictionary, type Locale } from "@/i18n";
import { SubPage } from "@/components/site/SubPage";

export default async function SecurityPage({ params }: { params: Promise<{ locale: Locale }> }) {
  const { locale } = await params;
  const t = getDictionary(locale);
  return (
    <SubPage locale={locale} t={t} path="/security" eyebrow={t.hero.badge} title={t.security.title} lead={t.security.lead}>
      <div className="points">
        {t.security.sections.map((s) => (
          <div className="point" key={s.title}>
            <h3>{s.title}</h3>
            <p>{s.text}</p>
          </div>
        ))}
      </div>
    </SubPage>
  );
}

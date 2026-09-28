import { getDictionary, type Locale } from "@/i18n";
import { SubPage } from "@/components/site/SubPage";

export default async function BetaPage({ params }: { params: Promise<{ locale: Locale }> }) {
  const { locale } = await params;
  const t = getDictionary(locale);
  return (
    <SubPage locale={locale} t={t} path="/beta" eyebrow={t.beta.tag} title={t.beta.title} lead={t.beta.lead}>
      <div className="points">
        {t.beta.points.map((p) => (
          <div className="point" key={p.title}>
            <h3>{p.title}</h3>
            <p>{p.text}</p>
          </div>
        ))}
      </div>
    </SubPage>
  );
}

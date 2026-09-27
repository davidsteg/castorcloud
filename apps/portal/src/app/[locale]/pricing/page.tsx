import { getDictionary, type Locale } from "@/i18n";
import { configFromQuery } from "@/lib/plan";
import { SubPage } from "@/components/site/SubPage";
import { PricingCalculator } from "@/components/site/PricingCalculator";

export default async function PricingPage({
  params,
  searchParams,
}: {
  params: Promise<{ locale: Locale }>;
  searchParams: Promise<Record<string, string | string[] | undefined>>;
}) {
  const { locale } = await params;
  const t = getDictionary(locale);
  // "Change" on the signup page comes back with the chosen configuration.
  const query = await searchParams;
  const initial = query.s ? configFromQuery(query) : undefined;

  return (
    <SubPage locale={locale} t={t} path="/pricing" eyebrow={t.nav.pricing} title={t.pricing.title} lead={t.pricing.lead} hideStart>
      <PricingCalculator locale={locale} t={t} initial={initial} />
    </SubPage>
  );
}

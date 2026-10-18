import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { BRAND_NAME } from "@/lib/config";
import { LOCALES, getDictionary, isLocale } from "@/i18n";

export function generateStaticParams() {
  return LOCALES.map((locale) => ({ locale }));
}

export async function generateMetadata({ params }: { params: Promise<{ locale: string }> }): Promise<Metadata> {
  const { locale } = await params;
  if (!isLocale(locale)) return {};
  const t = getDictionary(locale);
  // Pages set their own title, canonical URL and hreflang (lib/seo.ts).
  return {
    title: { default: `${BRAND_NAME} – ${t.meta.title}`, template: `%s | ${BRAND_NAME}` },
    description: t.meta.description,
  };
}

export default async function LocaleLayout({ children, params }: { children: React.ReactNode; params: Promise<{ locale: string }> }) {
  const { locale } = await params;
  if (!isLocale(locale)) notFound();
  return children;
}

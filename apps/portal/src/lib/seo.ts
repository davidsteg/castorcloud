import type { Metadata } from "next";
import { LOCALES, type Locale } from "@/i18n";
import { BRAND_NAME, appUrl } from "./config";

/** Public pages listed in the sitemap, as paths after the locale ("" = home). */
export const PUBLIC_PATHS = ["", "/features", "/pricing", "/security", "/assistant", "/open-source", "/faq", "/beta"] as const;

const OG_LOCALES: Record<Locale, string> = { de: "de_CH", fr: "fr_CH", it: "it_CH", en: "en_GB" };
const OG_IMAGE = { url: "/hero-lodge.webp", alt: BRAND_NAME };

/** Absolute origin of the public site (app.<domain>); relative metadata URLs resolve against it. */
export function siteOrigin(): URL {
  return new URL(appUrl("/"));
}

/**
 * hreflang alternates of one page: every locale, plus x-default pointing at
 * the unprefixed path, which the middleware sends to the visitor's language.
 */
export function languageAlternates(path: string): Record<string, string> {
  return {
    ...Object.fromEntries(LOCALES.map((l) => [`${l}-CH`, `/${l}${path}`])),
    "x-default": path || "/",
  };
}

/**
 * Title, description, canonical URL, hreflang and social preview of a public
 * page. Next merges metadata shallowly, so openGraph is always set in full.
 */
export function pageMetadata(
  locale: Locale,
  path: string,
  { title, description, index = true }: { title: string; description: string; index?: boolean },
): Metadata {
  const canonical = `/${locale}${path}`;
  return {
    title: path ? title : { absolute: title },
    description,
    alternates: { canonical, languages: languageAlternates(path) },
    openGraph: {
      type: "website",
      siteName: BRAND_NAME,
      url: canonical,
      title,
      description,
      locale: OG_LOCALES[locale],
      alternateLocale: LOCALES.filter((l) => l !== locale).map((l) => OG_LOCALES[l]),
      images: [OG_IMAGE],
    },
    twitter: { card: "summary_large_image", title, description, images: [OG_IMAGE.url] },
    ...(index ? {} : { robots: { index: false, follow: true } }),
  };
}

/** For pages behind a login or tied to one visitor: keep them out of search results. */
export const PRIVATE_PAGE: Metadata = { robots: { index: false, follow: false } };

/** Serialises structured data for a <script type="application/ld+json">, safe against "</script>". */
export function jsonLd(data: object): string {
  return JSON.stringify(data).replace(/</g, "\\u003c");
}

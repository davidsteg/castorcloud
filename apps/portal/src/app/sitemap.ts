import type { MetadataRoute } from "next";
import { LOCALES } from "@/i18n";
import { appUrl } from "@/lib/config";
import { PUBLIC_PATHS, languageAlternates } from "@/lib/seo";

// Built per request: the domain and scheme come from the runtime environment.
export const dynamic = "force-dynamic";

export default function sitemap(): MetadataRoute.Sitemap {
  return PUBLIC_PATHS.flatMap((path) => {
    const languages = Object.fromEntries(Object.entries(languageAlternates(path)).map(([lang, url]) => [lang, appUrl(url)]));
    return LOCALES.map((locale) => ({
      url: appUrl(`/${locale}${path}`),
      changeFrequency: "weekly" as const,
      priority: path ? 0.8 : 1,
      alternates: { languages },
    }));
  });
}

import type { Dictionary } from "./en";
import de from "./de";
import en from "./en";
import fr from "./fr";
import it from "./it";

export const LOCALES = ["de", "fr", "it", "en"] as const;
export type Locale = (typeof LOCALES)[number];
export const DEFAULT_LOCALE: Locale = "de";

export const LOCALE_NAMES: Record<Locale, string> = { de: "Deutsch", fr: "Français", it: "Italiano", en: "English" };

const dictionaries: Record<Locale, Dictionary> = { de, fr, it, en };

export function isLocale(value: string | undefined): value is Locale {
  return !!value && (LOCALES as readonly string[]).includes(value);
}

export function getDictionary(locale: Locale): Dictionary {
  return dictionaries[locale];
}

/** Best match from an Accept-Language header, e.g. "fr-CH,fr;q=0.9,en;q=0.8". */
export function negotiateLocale(acceptLanguage: string | null): Locale {
  const ranked = (acceptLanguage ?? "")
    .split(",")
    .map((part) => {
      const [tag, q] = part.trim().split(";q=");
      return { lang: tag.slice(0, 2).toLowerCase(), q: q ? Number(q) : 1 };
    })
    .sort((a, b) => b.q - a.q);
  return ranked.find((r) => isLocale(r.lang))?.lang as Locale ?? DEFAULT_LOCALE;
}

/** "Hello {n}" + { n: 3 } */
export function fill(text: string, vars: Record<string, string | number>): string {
  return text.replace(/\{(\w+)\}/g, (_, k) => String(vars[k] ?? `{${k}}`));
}

export type { Dictionary };

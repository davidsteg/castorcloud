import Link from "next/link";
import { BRAND_NAME } from "@/lib/config";
import { LOCALES, type Dictionary, type Locale } from "@/i18n";

/** castorcloud.ch: beaver mark + wordmark (text, so it stays crisp at any size). */
export function Logo({ size = 34 }: { size?: number }) {
  return (
    <span className="logo" aria-label={BRAND_NAME}>
      {/* eslint-disable-next-line @next/next/no-img-element */}
      <img className="logo-mark" src="/logo.svg" alt="" height={size} width={Math.round(size * 1.42)} />
      <span className="wordmark" aria-hidden>
        castorcloud<span className="tld">.ch</span>
      </span>
    </span>
  );
}

export function SiteHeader({ locale, t, path = "" }: { locale: Locale; t: Dictionary; path?: string }) {
  const home = `/${locale}`;
  return (
    <header className="site-header">
      <div className="container">
        <Link href={home} aria-label={BRAND_NAME}>
          <Logo />
        </Link>
        <nav className="site-nav">
          <Link href={`${home}/features`}>{t.nav.features}</Link>
          <Link href={`${home}/assistant`}>{t.nav.assistant}</Link>
          <Link href={`${home}/security`}>{t.nav.security}</Link>
          <Link href={`${home}/pricing`}>{t.nav.pricing}</Link>
          <Link href={`${home}/faq`}>{t.nav.faq}</Link>
        </nav>
        <div className="header-actions">
          <div className="lang-switch">
            {LOCALES.map((l) => (
              <Link key={l} href={`/${l}${path}`} aria-current={l === locale}>
                {l}
              </Link>
            ))}
          </div>
          <Link className="button secondary small" href={`${home}/account`}>
            {t.nav.login}
          </Link>
          <Link className="button small" href={`${home}/pricing`}>
            {t.nav.start}
          </Link>
        </div>
      </div>
    </header>
  );
}

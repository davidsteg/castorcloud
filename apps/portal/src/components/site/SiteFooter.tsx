import Link from "next/link";
import type { Dictionary, Locale } from "@/i18n";
import { Logo } from "./SiteHeader";
import { NewsletterForm } from "./NewsletterForm";

export function SiteFooter({ locale, t }: { locale: Locale; t: Dictionary }) {
  return (
    <footer className="site-footer">
      <div className="container footer-newsletter">
        <div>
          <strong>{t.newsletter.title}</strong>
          <p>{t.newsletter.lead}</p>
        </div>
        <NewsletterForm locale={locale} t={t.newsletter} />
      </div>
      <div className="container">
        <Logo />
        <span>{t.footer.tagline}</span>
        <nav>
          <Link href={`/${locale}/features`}>{t.nav.features}</Link>
          <Link href={`/${locale}/pricing`}>{t.nav.pricing}</Link>
          <Link href={`/${locale}/open-source`}>Open Source</Link>
          <Link href={`/${locale}/security`}>{t.footer.security}</Link>
          <Link href={`/${locale}/beta`}>{t.beta.tag}</Link>
          <Link href={`/${locale}/legal`}>{t.footer.legal}</Link>
          <Link href={`/${locale}/privacy`}>{t.footer.privacy}</Link>
          <Link href={`/${locale}/terms`}>{t.footer.terms}</Link>
        </nav>
      </div>
    </footer>
  );
}

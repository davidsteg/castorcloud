import Link from "next/link";
import type { Dictionary, Locale } from "@/i18n";
import { SiteHeader } from "./SiteHeader";
import { SiteFooter } from "./SiteFooter";

/** Closing call to action, used on the home page and every topic page. */
export function StartBand({ locale, t }: { locale: Locale; t: Dictionary }) {
  return (
    <section className="start-band">
      <div className="container">
        <div>
          <h2>{t.home.startTitle}</h2>
          <p>{t.home.startText}</p>
        </div>
        <Link className="button" href={`/${locale}/pricing`}>{t.home.startCta}</Link>
      </div>
    </section>
  );
}

/** Layout of a topic page linked from the home page. */
export function SubPage({
  locale, t, path, eyebrow, title, lead, children, hideStart = false,
}: {
  locale: Locale;
  t: Dictionary;
  path: string;
  eyebrow?: string;
  title: string;
  lead?: string;
  children: React.ReactNode;
  hideStart?: boolean;
}) {
  return (
    <>
      <SiteHeader locale={locale} t={t} path={path} />
      <main>
        <section className="section">
          <div className="container">
            <div className="section-head">
              {eyebrow && <p className="eyebrow">{eyebrow}</p>}
              <h1 className="page-title">{title}</h1>
              {lead && <p>{lead}</p>}
            </div>
            {children}
          </div>
        </section>
        {!hideStart && <StartBand locale={locale} t={t} />}
      </main>
      <SiteFooter locale={locale} t={t} />
    </>
  );
}

/** "Learn more →" link at the end of a home-page topic. */
export function MoreLink({ href, label }: { href: string; label: string }) {
  return <Link className="text-link more-link" href={href}>{label} →</Link>;
}

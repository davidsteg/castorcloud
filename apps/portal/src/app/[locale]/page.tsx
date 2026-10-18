import type { Metadata } from "next";
import Image from "next/image";
import Link from "next/link";
import { SERVICES, SERVICE_KEYS, formatChf, quote } from "@pc/pricing";
import { fill, getDictionary, type Locale } from "@/i18n";
import { BRAND_NAME, appUrl } from "@/lib/config";
import { jsonLd, pageMetadata } from "@/lib/seo";
import { SiteHeader } from "@/components/site/SiteHeader";
import { SiteFooter } from "@/components/site/SiteFooter";
import { Showcase } from "@/components/site/Showcase";
import { ServiceCards } from "@/components/site/ServiceCards";
import { MoreLink, StartBand } from "@/components/site/SubPage";
import { OssGallery } from "@/components/site/OssGallery";
import { FaqList } from "@/components/site/FaqList";

export async function generateMetadata({ params }: { params: Promise<{ locale: Locale }> }): Promise<Metadata> {
  const { locale } = await params;
  const t = getDictionary(locale);
  return pageMetadata(locale, "", { title: `${BRAND_NAME} – ${t.meta.title}`, description: t.meta.description });
}

// Home page: every important topic in brief; details live on topic pages.
export default async function Landing({ params }: { params: Promise<{ locale: Locale }> }) {
  const { locale } = await params;
  const t = getDictionary(locale);
  const at = (path: string) => `/${locale}${path}`;

  // Entry price: base + 100 GB + the cheapest service (derived from the price list).
  const cheapest = SERVICE_KEYS.reduce((a, b) => (SERVICES[a].priceRp <= SERVICES[b].priceRp ? a : b));
  const fromPrice = formatChf(quote({ services: [cheapest], storageUnits: 1 }).monthlyRp);

  return (
    <>
      <SiteHeader locale={locale} t={t} />
      <main>
        <script
          type="application/ld+json"
          dangerouslySetInnerHTML={{
            __html: jsonLd({
              "@context": "https://schema.org",
              "@graph": [
                { "@type": "Organization", "@id": appUrl("/#org"), name: BRAND_NAME, url: appUrl("/"), logo: appUrl("/logo.svg"), areaServed: "CH" },
                { "@type": "WebSite", name: BRAND_NAME, url: appUrl(`/${locale}`), inLanguage: locale, publisher: { "@id": appUrl("/#org") } },
              ],
            }),
          }}
        />
        <section className="hero">
          <div className="container">
            <div>
              <span className="swiss-badge">
                <span className="ch">CH</span>
                {t.hero.badge}
              </span>
              <h1>{t.hero.title}</h1>
              <p className="lead">{t.hero.lead}</p>
              <div className="hero-actions">
                <Link className="button" href={at("/pricing")}>{t.hero.cta}</Link>
                <Link className="button secondary" href={at("/security")}>{t.hero.secondary}</Link>
              </div>
              <ul className="trust">
                {t.hero.trust.map((item) => <li key={item}>{item}</li>)}
              </ul>
            </div>
            {/* Beaver guarding its lodge: photos, documents, videos and places behind a lock. */}
            <Image className="hero-art" src="/hero-lodge.webp" alt="" width={1492} height={892} priority
              sizes="(max-width: 960px) 100vw, 560px" />
          </div>
        </section>

        {/* Product */}
        <section className="section" id="demo">
          <div className="container">
            <div className="section-head centered">
              <h2>{t.showcase.title}</h2>
              <p>{t.showcase.lead}</p>
            </div>
            <Showcase t={t} brand={BRAND_NAME} />
          </div>
        </section>

        {/* Services */}
        <section className="section alt" id="features">
          <div className="container">
            <div className="section-head">
              <p className="eyebrow">{t.nav.features}</p>
              <h2>{t.services.title}</h2>
              <p>{t.services.lead}</p>
            </div>
            <ServiceCards t={t} compact />
            <MoreLink href={at("/features")} label={t.home.more} />
          </div>
        </section>

        {/* Assistant */}
        <section className="section" id="assistant">
          <div className="container topic">
            <div className="section-head">
              <p className="eyebrow">{t.services.items.assistant.name}</p>
              <h2>{t.assistantInfo.title}</h2>
              <p>{t.assistantInfo.lead}</p>
              <MoreLink href={at("/assistant")} label={t.home.more} />
            </div>
            <ul className="check-list panel">
              {t.home.assistantPoints.map((p) => <li key={p}>{p}</li>)}
            </ul>
          </div>
        </section>

        {/* Security & Switzerland */}
        <section className="section alt" id="security">
          <div className="container">
            <div className="section-head">
              <p className="eyebrow">{t.hero.badge}</p>
              <h2>{t.swiss.title}</h2>
              <p>{t.swiss.lead}</p>
            </div>
            <div className="points">
              {t.swiss.points.map((point) => (
                <div className="point" key={point.title}>
                  <h3>{point.title}</h3>
                  <p>{point.text}</p>
                </div>
              ))}
            </div>
            <MoreLink href={at("/security")} label={t.swiss.more} />
          </div>
        </section>

        {/* Pricing */}
        <section className="section" id="pricing">
          <div className="container topic">
            <div className="section-head">
              <p className="eyebrow">{t.nav.pricing}</p>
              <h2>{t.pricing.title}</h2>
              <p>{t.pricing.lead}</p>
            </div>
            <div className="panel price-teaser">
              <p className="price-from">{fill(t.home.pricingFrom, { price: fromPrice })}</p>
              <ul className="check-list">
                {t.home.pricingPoints.map((p) => <li key={p}>{p}</li>)}
              </ul>
              <Link className="button block" href={at("/pricing")}>{t.home.pricingCta}</Link>
              <p className="small-print">{t.pricing.vatIncluded}</p>
            </div>
          </div>
        </section>

        {/* Open source */}
        <section className="section alt" id="open-source">
          <div className="container">
            <div className="section-head">
              <p className="eyebrow">Open Source</p>
              <h2>{t.openSource.title}</h2>
              <p>{t.openSource.lead}</p>
            </div>
            <OssGallery t={t} compact />
            <MoreLink href={at("/open-source")} label={t.home.more} />
          </div>
        </section>

        {/* FAQ */}
        <section className="section" id="faq">
          <div className="container">
            <div className="section-head">
              <h2>{t.faq.title}</h2>
            </div>
            <FaqList t={t} limit={3} />
            <MoreLink href={at("/faq")} label={t.home.faqAll} />
          </div>
        </section>

        <StartBand locale={locale} t={t} />
      </main>
      <SiteFooter locale={locale} t={t} />
    </>
  );
}

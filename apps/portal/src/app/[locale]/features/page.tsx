import type { Metadata } from "next";
import Image from "next/image";
import Link from "next/link";
import { OSS_PROJECTS, SERVICES, SERVICE_KEYS, formatChf, type ServiceKey } from "@pc/pricing";
import { fill, getDictionary, type Dictionary, type Locale } from "@/i18n";
import { SERVICE_ICONS } from "@/lib/plan";
import { SCREENSHOTS } from "@/lib/screenshots";
import { SubPage } from "@/components/site/SubPage";
import { ServiceApps } from "@/components/site/MobileApps";
import { pageMetadata } from "@/lib/seo";

export async function generateMetadata({ params }: { params: Promise<{ locale: Locale }> }): Promise<Metadata> {
  const { locale } = await params;
  const t = getDictionary(locale);
  return pageMetadata(locale, "/features", { title: t.services.title, description: t.services.lead });
}

// One sales section per tool: benefit, screenshot, features, how to move in,
// apps, price and a direct start with that tool pre-selected.
export default async function FeaturesPage({ params }: { params: Promise<{ locale: Locale }> }) {
  const { locale } = await params;
  const t = getDictionary(locale);
  const f = t.featuresPage;

  return (
    <SubPage locale={locale} t={t} path="/features" eyebrow={t.nav.features} title={t.services.title} lead={t.services.lead}>
      <nav className="tool-jump" aria-label={f.jump}>
        <span className="muted">{f.jump}:</span>
        {SERVICE_KEYS.map((s) => (
          <a key={s} href={`#${s}`}><span aria-hidden>{SERVICE_ICONS[s]}</span> {t.services.items[s].name}</a>
        ))}
      </nav>

      {SERVICE_KEYS.map((s, i) => (
        <ToolSection key={s} service={s} t={t} locale={locale} flip={i % 2 === 1} />
      ))}
    </SubPage>
  );
}

function ToolSection({ service, t, locale, flip }: { service: ServiceKey; t: Dictionary; locale: Locale; flip: boolean }) {
  const f = t.featuresPage;
  const item = t.featuresPage.items[service];
  const name = t.services.items[service].name;
  const project = SERVICES[service].project;
  const shot = project && project !== "platform" ? SCREENSHOTS[project] : undefined;
  const projectName = project && project !== "platform" ? OSS_PROJECTS[project].name : "";

  return (
    <section className={flip ? "tool flip" : "tool"} id={service}>
      <div className="tool-media">
        {shot ? (
          <figure>
            <Image src={shot.src} alt={`${name} – ${projectName}`} width={shot.width} height={shot.height}
              sizes="(max-width: 960px) 100vw, 560px" />
            <figcaption className="small-print">
              <a href={shot.source} target="_blank" rel="noreferrer">
                {fill(t.openSourcePage.credit, { name: projectName, license: shot.license })}
              </a>
            </figcaption>
          </figure>
        ) : (
          <div className="tool-chat" aria-hidden>
            <span className="bubble user">{t.showcase.sample.question}</span>
            <span className="bubble bot">{t.showcase.sample.answer}</span>
          </div>
        )}
      </div>

      <div className="tool-text">
        <p className="eyebrow"><span aria-hidden>{SERVICE_ICONS[service]}</span> {name} · {projectName}</p>
        <h2>{item.headline}</h2>
        <p className="tool-lead">{item.lead}</p>
        <ul className="check-list">
          {item.bullets.map((b) => <li key={b}>{b}</li>)}
        </ul>

        <dl className="tool-facts">
          <div><dt>{f.importLabel}</dt><dd>{item.import}</dd></div>
          <div><dt>{f.appsLabel}</dt><dd>{item.apps}<ServiceApps t={t} service={service} /></dd></div>
          <div><dt>{t.services.replaces}</dt><dd>{t.services.items[service].replaces}</dd></div>
        </dl>

        <div className="tool-buy">
          <span className="tool-price">{fill(f.from, { price: formatChf(SERVICES[service].priceRp) })}</span>
          <Link className="button" href={`/${locale}/pricing?s=${service}`}>{fill(f.cta, { name })}</Link>
        </div>
      </div>
    </section>
  );
}

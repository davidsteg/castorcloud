import type { ServiceKey } from "@pc/pricing";
import type { Dictionary } from "@/i18n";
import { MOBILE_APPS } from "@/lib/apps";

function StoreLinks({ ios, android }: { ios?: string; android?: string }) {
  return (
    <span className="store-links">
      {ios && <a className="store" href={ios} target="_blank" rel="noreferrer"><span aria-hidden></span> App Store</a>}
      {android && <a className="store" href={android} target="_blank" rel="noreferrer"><span aria-hidden>▶</span> Google Play</a>}
    </span>
  );
}

/** Apps for one service; services without an app are used in the browser. */
export function ServiceApps({ t, service }: { t: Dictionary; service: ServiceKey }) {
  const apps = MOBILE_APPS[service];
  if (!apps) return <p className="small-print">{t.appsInfo.web}</p>;
  return (
    <ul className="app-list">
      {apps.map((app) => (
        <li key={app.name}>
          <span>
            <b>{app.name}</b> <small className="muted">· {app.official ? t.appsInfo.official : t.appsInfo.community}</small>
          </span>
          <StoreLinks ios={app.ios} android={app.android} />
        </li>
      ))}
    </ul>
  );
}

/** All mobile apps, grouped by service (FAQ). */
export function MobileAppsOverview({ t }: { t: Dictionary }) {
  const services: ServiceKey[] = ["photos", "docs", "places", "flights", "assistant"];
  return (
    <div className="apps-overview">
      {services.map((s) => (
        <div key={s} className="apps-row">
          <b>{t.services.items[s].name}</b>
          <ServiceApps t={t} service={s} />
        </div>
      ))}
      <p className="small-print">{t.appsInfo.connect}</p>
    </div>
  );
}

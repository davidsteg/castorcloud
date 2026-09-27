import { SERVICE_KEYS } from "@pc/pricing";
import type { Dictionary } from "@/i18n";
import { SERVICE_ICONS } from "@/lib/plan";

/** Service overview; `compact` = name + one line, for the home page. */
export function ServiceCards({ t, compact = false }: { t: Dictionary; compact?: boolean }) {
  return (
    <div className={compact ? "service-grid compact" : "service-grid"}>
      {SERVICE_KEYS.map((s) => {
        const item = t.services.items[s];
        return (
          <article className="service-card" key={s}>
            <span className="icon" aria-hidden>{SERVICE_ICONS[s]}</span>
            <h3>{item.name}</h3>
            <p>{item.text}</p>
            {!compact && <p className="replaces">{t.services.replaces}: <b>{item.replaces}</b></p>}
          </article>
        );
      })}
    </div>
  );
}

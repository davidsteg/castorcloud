import type { Dictionary } from "@/i18n";
import { MobileAppsOverview } from "./MobileApps";

/** Index of the "Which apps do I use on my phone?" question in every language. */
const APPS_QUESTION = 4;

export function FaqList({ t, limit }: { t: Dictionary; limit?: number }) {
  return (
    <div className="faq">
      {t.faq.items.slice(0, limit).map((item, i) => (
        <details key={item.q}>
          <summary>{item.q}</summary>
          <p>{item.a}</p>
          {i === APPS_QUESTION && <MobileAppsOverview t={t} />}
        </details>
      ))}
    </div>
  );
}

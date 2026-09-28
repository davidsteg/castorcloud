import type { Tenant } from "@pc/db";
import { formatStorage, storageLevel, storageQuotaMb } from "@pc/pricing";
import { fill, type Dictionary } from "@/i18n";

const PARTS = ["photos", "docs", "places", "flights", "system"] as const;
const COLORS: Record<(typeof PARTS)[number], string> = {
  photos: "var(--pc-series-4)",
  docs: "var(--pc-series-1)",
  places: "var(--pc-ok)",
  flights: "var(--pc-series-3)",
  system: "var(--pc-series-2)",
};

type Props = {
  tenant: Pick<Tenant, "storageUnits" | "storageUsedMb" | "storageVolumeMb" | "storageByService">;
  t: Dictionary;
  /** Shown under warnings, e.g. a link to change the booking. */
  action?: React.ReactNode;
};

/** Used vs. booked storage, split by service, with warnings near the limit. */
export function StorageMeter({ tenant, t, action }: Props) {
  const s = t.storage;
  const quotaMb = storageQuotaMb(tenant.storageUnits);
  const usedMb = tenant.storageUsedMb;
  if (usedMb === null) {
    return (
      <div className="storage">
        <b>{s.title}</b> <span className="muted">· {formatStorage(quotaMb)}</span>
        <p className="small-print">{s.notMeasured}</p>
      </div>
    );
  }

  // A held-back downgrade: the volume is larger than the booking.
  const held = (tenant.storageVolumeMb ?? 0) > quotaMb;
  const capMb = held ? tenant.storageVolumeMb! : quotaMb;
  const level = storageLevel(usedMb, capMb);
  const byService = (tenant.storageByService ?? {}) as Partial<Record<(typeof PARTS)[number], number>>;
  const percent = Math.min(100, Math.round((usedMb / capMb) * 100));

  return (
    <div className="storage">
      <div className="storage-head">
        <b>{s.title}</b>
        <span className="muted">{fill(s.used, { used: formatStorage(usedMb), quota: formatStorage(capMb) })}</span>
      </div>
      <div className="storage-bar" role="meter" aria-valuemin={0} aria-valuemax={capMb} aria-valuenow={usedMb} aria-label={s.title}>
        {PARTS.map((p) => {
          const mb = byService[p] ?? 0;
          return mb > 0 ? <span key={p} style={{ width: `${(mb / capMb) * 100}%`, background: COLORS[p] }} /> : null;
        })}
      </div>
      <ul className="storage-legend">
        {PARTS.filter((p) => (byService[p] ?? 0) > 0).map((p) => (
          <li key={p}><i style={{ background: COLORS[p] }} />{s.parts[p]} <span className="muted">{formatStorage(byService[p]!)}</span></li>
        ))}
      </ul>
      {(held || level !== "ok") && (
        <p className="notice" style={{ marginTop: 12 }}>
          {held
            ? fill(s.held, { quota: formatStorage(quotaMb), used: formatStorage(usedMb), volume: formatStorage(capMb) })
            : level === "full" ? s.full : fill(s.warn, { percent })}
        </p>
      )}
      {action}
    </div>
  );
}

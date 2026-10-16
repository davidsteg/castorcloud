"use client";

import { useMemo, useState } from "react";
import {
  AI_CREDIT_TIERS_CHF, DEFAULT_AI_CREDIT_CHF, MAX_EXTRA_MEMBERS, MAX_STORAGE_UNITS, OSS_PROJECTS, SERVICES,
  SERVICE_KEYS, STORAGE_UNIT_GB, BASE, STORAGE, MEMBER, NO_STACK_UNITS, formatChf, quote,
  type Config, type OssProject, type ServiceKey,
} from "@pc/pricing";
import { fill, type Dictionary, type Locale } from "@/i18n";
import { SERVICE_ICONS, configToQuery } from "@/lib/plan";

const PARTS = [
  ["infrastructure", "var(--pc-series-1)"],
  ["aiCredit", "var(--pc-series-6)"],
  ["operations", "var(--pc-series-5)"],
  ["openSource", "var(--pc-series-3)"],
  ["paymentFees", "var(--pc-series-4)"],
  ["vat", "var(--pc-series-2)"],
] as const;

function storageLabel(units: number) {
  const gb = units * STORAGE_UNIT_GB;
  return gb >= 1000 ? `${(gb / 1000).toLocaleString("de-CH")} TB` : `${gb} GB`;
}

export function PricingCalculator({ locale, t, initial }: { locale: Locale; t: Dictionary; initial?: Config }) {
  const [config, setConfig] = useState<Config>(
    initial ?? { services: ["photos", "docs"], storageUnits: 2, extraMembers: 0, interval: "month", aiCreditChf: 0, stacks: NO_STACK_UNITS },
  );
  const q = useMemo(() => quote(config), [config]);
  const p = t.pricing;

  const toggle = (s: ServiceKey) =>
    setConfig((c) => {
      const services = c.services.includes(s)
        ? c.services.filter((x) => x !== s)
        : SERVICE_KEYS.filter((k) => k === s || c.services.includes(k));
      const assistant = services.includes("assistant");
      return { ...c, services, aiCreditChf: assistant ? c.aiCreditChf || DEFAULT_AI_CREDIT_CHF : 0 };
    });

  const breakdown = {
    infrastructure: q.breakdown.infrastructureRp,
    aiCredit: q.breakdown.aiCreditRp,
    operations: q.breakdown.operationsRp,
    openSource: q.breakdown.openSourceRp,
    paymentFees: q.breakdown.paymentFeesRp,
    vat: q.breakdown.vatRp,
  };

  return (
    <div className="pricing">
      <div className="panel">
        <div className="interval" role="group">
          {(["month", "year"] as const).map((i) => (
            <button key={i} type="button" aria-pressed={config.interval === i} onClick={() => setConfig((c) => ({ ...c, interval: i }))}>
              {i === "month" ? p.monthly : p.yearly}
              {i === "year" && <span className="save">{p.yearlyHint}</span>}
            </button>
          ))}
        </div>

        <div className="panel-block">
          <p className="base-line">
            <span>{p.base}</span>
            <span>{formatChf(BASE.priceRp)}</span>
          </p>
        </div>

        <div className="panel-block">
          <h3>{p.services}</h3>
          {SERVICE_KEYS.map((s) => (
            <label className="choice" key={s}>
              <input type="checkbox" checked={config.services.includes(s)} onChange={() => toggle(s)} />
              <span aria-hidden>{SERVICE_ICONS[s]}</span>
              <span className="name">{t.services.items[s].name}</span>
              <span className="price">
                {formatChf(SERVICES[s].priceRp)} {p.perMonth}
              </span>
            </label>
          ))}
          {config.services.includes("assistant") && (
            <div className="credit">
              <h3>{p.aiCredit}</h3>
              <div className="tiers" role="radiogroup" aria-label={p.aiCredit}>
                {AI_CREDIT_TIERS_CHF.map((chf) => (
                  <button key={chf} type="button" role="radio" aria-checked={config.aiCreditChf === chf}
                    onClick={() => setConfig((c) => ({ ...c, aiCreditChf: chf }))}>
                    CHF {chf}
                  </button>
                ))}
              </div>
              <p className="small-print">{p.aiCreditHint}</p>
            </div>
          )}
        </div>

        <div className="panel-block">
          <h3>
            {p.storage} <span className="muted">· {formatChf(STORAGE.priceRp)} / {STORAGE_UNIT_GB} GB</span>
          </h3>
          <div className="range-row">
            <input
              type="range"
              min={1}
              max={MAX_STORAGE_UNITS}
              value={config.storageUnits}
              aria-label={p.storage}
              onChange={(e) => setConfig((c) => ({ ...c, storageUnits: Number(e.target.value) }))}
            />
            <output>{storageLabel(config.storageUnits)}</output>
          </div>
        </div>

        <div className="panel-block">
          <h3>
            {p.members} <span className="muted">· {formatChf(MEMBER.priceRp)}</span>
          </h3>
          <div className="range-row">
            <span className="muted" style={{ flex: 1 }}>
              {fill(p.membersHint, { n: config.extraMembers })}
            </span>
            <div className="stepper">
              <button type="button" aria-label="−" disabled={config.extraMembers === 0}
                onClick={() => setConfig((c) => ({ ...c, extraMembers: c.extraMembers - 1 }))}>−</button>
              <span>{config.extraMembers}</span>
              <button type="button" aria-label="+" disabled={config.extraMembers === MAX_EXTRA_MEMBERS}
                onClick={() => setConfig((c) => ({ ...c, extraMembers: c.extraMembers + 1 }))}>+</button>
            </div>
          </div>
        </div>
      </div>

      <div className="panel">
        <div className="total">
          <strong>{formatChf(q.chargeRp)}</strong>
          <span className="per">{config.interval === "year" ? p.perYear : p.perMonth}</span>
        </div>
        <p className="small-print">
          {config.interval === "year" && <>{fill(p.effective, { price: formatChf(q.effectiveMonthlyRp) })} · </>}
          {p.vatIncluded}
        </p>

        <a
          className="button block"
          style={{ margin: "22px 0 28px" }}
          href={config.services.length ? `/${locale}/signup?${configToQuery(q.config)}` : undefined}
          aria-disabled={!config.services.length}
        >
          {config.services.length ? p.continue : p.minService}
        </a>

        <h3 style={{ fontSize: 17 }}>{p.whereTitle}</h3>
        <p className="small-print">{p.whereLead}</p>
        <div className="breakdown-bar" role="img" aria-label={p.whereTitle}>
          {PARTS.map(([key, color]) => (
            <span key={key} style={{ flexGrow: Math.max(breakdown[key], 1), background: color }} />
          ))}
        </div>
        <ul className="legend">
          {PARTS.filter(([key]) => key !== "aiCredit" || breakdown.aiCredit > 0).map(([key, color]) => (
            <li key={key}>
              <span className="swatch" style={{ background: color }} />
              <span>{p.parts[key]}</span>
              <span className="amount">{formatChf(breakdown[key], false)}</span>
            </li>
          ))}
        </ul>

        <div className="donations">
          <h4>{p.donationsTitle}</h4>
          <ul>
            {(Object.entries(q.donations) as [OssProject, number][])
              .sort((a, b) => b[1] - a[1])
              .map(([project, rp]) => (
                <li key={project}>
                  <a href={OSS_PROJECTS[project].url} target="_blank" rel="noreferrer">{OSS_PROJECTS[project].name}</a>
                  <span className="amount">{formatChf(rp)}</span>
                </li>
              ))}
          </ul>
          <p className="small-print">{p.donationsNote}</p>
        </div>
      </div>
    </div>
  );
}

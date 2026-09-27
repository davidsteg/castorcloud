"use client";

import { useState } from "react";
import type { Dictionary } from "@/i18n";
import { SERVICE_ICONS } from "@/lib/plan";

// Product preview: the tenant shell with each service, drawn with sample
// data. Real screenshots from a demo tenant can replace the panes later.
const TABS = ["photos", "docs", "places", "flights", "assistant"] as const;
type Tab = (typeof TABS)[number];

export function Showcase({ t, brand }: { t: Dictionary; brand: string }) {
  const [tab, setTab] = useState<Tab>("photos");
  const s = t.showcase.sample;

  return (
    <div className="showcase">
      <div className="showcase-tabs" role="tablist">
        {TABS.map((key) => (
          <button key={key} role="tab" type="button" aria-selected={tab === key} onClick={() => setTab(key)}>
            <span aria-hidden>{SERVICE_ICONS[key]}</span> {t.services.items[key].name}
          </button>
        ))}
      </div>

      <div className="device" role="tabpanel">
        <div className="device-bar" aria-hidden>
          <i /><i /><i />
          <span>{tab === "assistant" ? "" : `${tab}-`}muster.castorcloud.ch</span>
        </div>
        <div className="device-body">
          <aside className="mock-nav" aria-hidden>
            <div className="mock-brand">
              {/* eslint-disable-next-line @next/next/no-img-element */}
              <img src="/logo.svg" alt="" height={18} width={26} /> {brand}
            </div>
            {TABS.map((key) => (
              <div key={key} className={key === tab ? "active" : ""}>
                <span>{SERVICE_ICONS[key]}</span> {t.services.items[key].name}
              </div>
            ))}
          </aside>
          <div className="mock-main">
            {tab === "photos" && <PhotosPane s={s} />}
            {tab === "docs" && <DocsPane s={s} />}
            {tab === "places" && <PlacesPane s={s} />}
            {tab === "flights" && <FlightsPane s={s} />}
            {tab === "assistant" && <AssistantPane s={s} t={t} />}
          </div>
        </div>
      </div>
      <p className="showcase-caption">{t.showcase.captions[tab]}</p>
      <p className="small-print" style={{ textAlign: "center" }}>{t.showcase.note}</p>
    </div>
  );
}

type Sample = Dictionary["showcase"]["sample"];

function Toolbar({ s, title }: { s: Sample; title: string }) {
  return (
    <div className="mock-toolbar">
      <strong>{title}</strong>
      <span className="mock-search">⌕ {s.search}</span>
    </div>
  );
}

const PHOTO_TONES = [
  ["#f6c28b", "#d9734e"], ["#9ec9ea", "#4c7fb5"], ["#bcdcae", "#5d9658"], ["#efd9a0", "#b8933f"],
  ["#d9c7ee", "#8467b0"], ["#a9d7d9", "#3d8e92"], ["#f3c4c4", "#bf5757"], ["#cfd4dc", "#7a8190"],
  ["#f7d9b0", "#cf8b3e"], ["#b6d2f0", "#5577b8"], ["#c9e3c0", "#6c9f62"], ["#e8c4d9", "#a2577f"],
];

function PhotosPane({ s }: { s: Sample }) {
  return (
    <>
      <Toolbar s={s} title={s.albumTitle} />
      <div className="mock-photos">
        {PHOTO_TONES.map(([a, b], i) => (
          <i key={i} className={i === 0 || i === 7 ? "wide" : ""} style={{ background: `linear-gradient(135deg, ${a}, ${b})` }} />
        ))}
      </div>
    </>
  );
}

function DocsPane({ s }: { s: Sample }) {
  const rows = [
    [s.docTitle, s.docMeta, true],
    ["Krankenkasse Prämie 2026", "Versicherung · 03.01.2026", false],
    ["Steuererklärung 2025", "Steuern · 28.03.2026", false],
    ["Mietvertrag Wohnung", "Wohnen · 01.10.2024", false],
    ["Garantie Velo", "Garantie · 17.06.2025", false],
  ] as const;
  return (
    <>
      <Toolbar s={s} title="Paperless" />
      <div className="mock-docs">
        <ul>
          {rows.map(([title, meta, active]) => (
            <li key={title} className={active ? "active" : ""}>
              <span className="doc-icon" aria-hidden>▤</span>
              <span><b>{title}</b><small>{meta}</small></span>
            </li>
          ))}
        </ul>
        <div className="mock-invoice" aria-hidden>
          <b>HOTEL LISBOA</b>
          <i style={{ width: "60%" }} /><i style={{ width: "40%" }} />
          <div className="inv-lines">
            <span /><span /><span /><span />
          </div>
          <b className="inv-total">EUR 612.00</b>
        </div>
      </div>
    </>
  );
}

function PlacesPane({ s }: { s: Sample }) {
  return (
    <>
      <Toolbar s={s} title={s.placesTitle} />
      <div className="mock-map">
        <svg viewBox="0 0 400 220" aria-hidden>
          <rect width="400" height="220" fill="#dfe9ee" />
          <path d="M0 40 C60 20 110 60 170 45 S290 10 400 35 V220 H0 Z" fill="#eef2e6" />
          <path d="M40 150 C90 120 150 170 210 140 S320 100 380 130" fill="none" stroke="#c9d6b8" strokeWidth="18" strokeLinecap="round" />
          <path d="M60 170 L120 120 L170 132 L230 90 L290 104 L340 70" fill="none" stroke="#d52b1e" strokeWidth="4" strokeLinecap="round" strokeLinejoin="round" />
          {[[60, 170], [120, 120], [170, 132], [230, 90], [290, 104], [340, 70]].map(([x, y]) => (
            <circle key={`${x}-${y}`} cx={x} cy={y} r="6" fill="#fff" stroke="#d52b1e" strokeWidth="3" />
          ))}
        </svg>
        <span className="map-chip">{s.placesStat}</span>
      </div>
    </>
  );
}

function FlightsPane({ s }: { s: Sample }) {
  const zrh = [205, 92];
  const dests = [[120, 128], [300, 70], [262, 150], [70, 80], [345, 120]];
  return (
    <>
      <Toolbar s={s} title="AirTrail" />
      <div className="mock-map dark">
        <svg viewBox="0 0 400 220" aria-hidden>
          <rect width="400" height="220" fill="#1b2330" />
          <path d="M20 70 C60 40 120 50 150 70 S170 120 130 150 S40 150 20 110 Z" fill="#26303f" />
          <path d="M170 50 C220 30 300 30 360 55 S380 140 320 170 S200 170 180 120 Z" fill="#26303f" />
          {dests.map(([x, y]) => (
            <path key={`${x}-${y}`} d={`M${zrh[0]} ${zrh[1]} Q${(zrh[0] + x) / 2} ${Math.min(zrh[1], y) - 45} ${x} ${y}`}
              fill="none" stroke="#f29a1f" strokeWidth="2" strokeDasharray="4 4" />
          ))}
          {dests.map(([x, y]) => <circle key={`d${x}`} cx={x} cy={y} r="4" fill="#fff" />)}
          <circle cx={zrh[0]} cy={zrh[1]} r="6" fill="#d52b1e" stroke="#fff" strokeWidth="2" />
        </svg>
        <span className="map-chip">{s.flightsStat}</span>
      </div>
    </>
  );
}

function AssistantPane({ s, t }: { s: Sample; t: Dictionary }) {
  return (
    <div className="mock-chat">
      <div className="bubble user">{s.question}</div>
      <div className="bubble bot">
        {s.answer}
        <div className="sources">
          <span>▦ 48 {t.services.items.photos.name}</span>
          <span>▤ {s.docTitle}</span>
          <span>✈ LX 2084</span>
        </div>
      </div>
      <div className="mock-input">✦ …</div>
    </div>
  );
}

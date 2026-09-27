// A booked configuration travels from the pricing page to signup in the URL.
import { normalize, type Config } from "@pc/pricing";

export const SERVICE_ICONS: Record<string, string> = {
  photos: "▦",
  docs: "▤",
  places: "◉",
  flights: "✈",
  assistant: "✦",
};

export function configToQuery(c: Config): string {
  return new URLSearchParams({
    s: c.services.join(","),
    st: String(c.storageUnits),
    m: String(c.extraMembers),
    i: c.interval,
    ...(c.aiCreditChf ? { c: String(c.aiCreditChf) } : {}),
  }).toString();
}

export function configFromQuery(q: Record<string, string | string[] | undefined>): Config {
  const one = (v: string | string[] | undefined) => (Array.isArray(v) ? v[0] : v) ?? "";
  return normalize({
    services: one(q.s).split(",").filter(Boolean) as Config["services"],
    storageUnits: Number(one(q.st) || 1),
    extraMembers: Number(one(q.m) || 0),
    interval: one(q.i) === "year" ? "year" : "month",
    aiCreditChf: Number(one(q.c) || 0),
  });
}

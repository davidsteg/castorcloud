// Timeline (Dawarich). Answers "where was I" with visits and city-level
// stretches of time, never with raw GPS tracks.
import { z } from "zod";
import { pool } from "../db.ts";
import { TtlCache, getJson } from "../http.ts";
import { ToolError, clampLimit, compact, isoDate, roundCoord } from "../limits.ts";
import type { Member } from "../members.ts";
import { READ_ONLY, answer, type ServiceModule } from "../service.ts";

const env = { url: process.env.DAWARICH_URL, db: process.env.DAWARICH_DB_URL };
const LABEL = "Timeline";

type Visit = {
  started_at: string;
  ended_at: string;
  duration?: number;
  name?: string;
  status?: string;
  place?: { latitude?: number | string | null; longitude?: number | string | null };
};
export type Point = { timestamp: number; city?: string | null; country_name?: string | null; country?: string | null };

const keys = new TtlCache<string>(3600_000);

/** Dawarich keeps one API key per user in plain text (users.api_key). */
async function apiKey(member: Member) {
  return keys.get(member.id, async () => {
    const res = await pool(env.db!).query<{ api_key: string }>(
      "SELECT api_key FROM users WHERE lower(email) = lower($1)",
      [member.email],
    );
    const key = res.rows[0]?.api_key;
    if (!key) throw new ToolError("Timeline has not been opened by this person yet, so there is nothing to search");
    return key;
  });
}

async function dawarich<T>(member: Member, path: string, query: Record<string, string | number | undefined>) {
  const key = await apiKey(member);
  try {
    return await getJson<T>(env.url!, path, { headers: { Authorization: `Bearer ${key}` }, query, label: LABEL });
  } catch (err) {
    keys.delete(member.id);
    throw err;
  }
}

/** Consecutive points in the same city become one stretch of time. */
export function stretches(points: Point[], max = 25) {
  const out: { place: string; from: string; to: string }[] = [];
  for (const p of [...points].sort((a, b) => a.timestamp - b.timestamp)) {
    const place = [p.city, p.country_name ?? p.country].filter(Boolean).join(", ");
    if (!place) continue;
    const at = new Date(p.timestamp * 1000).toISOString().slice(0, 16);
    const last = out.at(-1);
    if (last?.place === place) last.to = at;
    else out.push({ place, from: at, to: at });
  }
  return out.length > max ? [...out.slice(0, max - 1), out.at(-1)!] : out;
}

function range(args: { date?: string; from?: string; to?: string }) {
  const from = isoDate(args.date ?? args.from);
  const to = isoDate(args.date ?? args.to, true);
  if (!from || !to) throw new ToolError("give a date, or both from and to (YYYY-MM-DD)");
  if (Date.parse(to) - Date.parse(from) > 400 * 86400_000) throw new ToolError("ask about at most about a year at a time");
  return { from, to };
}

export const places: ServiceModule = {
  key: "places",
  configured: Boolean(env.url && env.db),
  register(server, { member, portalOrigin }) {
    const dateArgs = {
      date: z.string().optional().describe("A single day (YYYY-MM-DD)"),
      from: z.string().optional().describe("Start (YYYY-MM-DD)"),
      to: z.string().optional().describe("End (YYYY-MM-DD)"),
    };

    server.registerTool(
      "where_was_i",
      {
        title: "Where was I",
        description:
          "Where the user was during a day or period, from their location timeline: the cities/countries " +
          "with time ranges, plus confirmed visits to named places.",
        inputSchema: dateArgs,
        annotations: READ_ONLY,
      },
      (args) =>
        answer(async () => {
          const { from, to } = range(args);
          const start = Math.floor(Date.parse(from) / 1000);
          const end = Math.floor(Date.parse(to) / 1000);
          const [points, visits] = await Promise.all([
            dawarich<Point[]>(member, "/api/v1/points", { start_at: start, end_at: end, per_page: 5000, order: "asc" }),
            dawarich<Visit[]>(member, "/api/v1/visits", { start_at: from, end_at: to }),
          ]);
          return {
            places: stretches(points),
            visits: visits
              .filter((v) => v.status !== "declined")
              .slice(0, 25)
              .map((v) => compact({ name: v.name, from: v.started_at?.slice(0, 16), to: v.ended_at?.slice(0, 16) })),
            ...(points.length === 0 ? { note: "no location data recorded in this period" } : {}),
            link: `${portalOrigin}/places/map?start_at=${from.slice(0, 10)}&end_at=${to.slice(0, 10)}`,
          };
        }),
    );

    server.registerTool(
      "list_visits",
      {
        title: "List visits",
        description: "Visits to places (restaurants, hotels, friends' homes, ...) detected in the timeline for a period.",
        inputSchema: { ...dateArgs, limit: z.number().int().min(1).max(25).optional() },
        annotations: READ_ONLY,
      },
      (args) =>
        answer(async () => {
          const { from, to } = range(args);
          const visits = await dawarich<Visit[]>(member, "/api/v1/visits", { start_at: from, end_at: to });
          const items = visits.filter((v) => v.status !== "declined").slice(0, clampLimit(args.limit));
          return {
            count: items.length,
            items: items.map((v) =>
              compact({
                name: v.name,
                from: v.started_at?.slice(0, 16),
                to: v.ended_at?.slice(0, 16),
                minutes: v.duration,
                lat: roundCoord(Number(v.place?.latitude)),
                lon: roundCoord(Number(v.place?.longitude)),
              }),
            ),
          };
        }),
    );

    server.registerTool(
      "travel_stats",
      {
        title: "Travel statistics",
        description: "Distance travelled and number of countries/cities visited, in total and per year.",
        inputSchema: {},
        annotations: READ_ONLY,
      },
      () =>
        answer(async () => {
          type Stats = {
            totalDistanceKm: number;
            totalCountriesVisited: number;
            totalCitiesVisited: number;
            yearlyStats: { year: number; totalDistanceKm: number; totalCountriesVisited: number; totalCitiesVisited: number }[];
          };
          const s = await dawarich<Stats>(member, "/api/v1/stats", {});
          return {
            totalKm: Math.round(s.totalDistanceKm),
            countries: s.totalCountriesVisited,
            cities: s.totalCitiesVisited,
            years: s.yearlyStats.slice(0, 10).map((y) => ({
              year: y.year, km: Math.round(y.totalDistanceKm), countries: y.totalCountriesVisited, cities: y.totalCitiesVisited,
            })),
          };
        }),
    );
  },
};

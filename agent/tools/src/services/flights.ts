// Flights (AirTrail). Uses AirTrail's REST API v1 with a per-member key that
// only has read scopes.
import { z } from "zod";
import { KEY_NAME, newApiKey, pool, sha256 } from "../db.ts";
import { TtlCache, getJson } from "../http.ts";
import { ToolError, clampLimit, compact, isoDate } from "../limits.ts";
import type { Member } from "../members.ts";
import { READ_ONLY, answer, type ServiceModule } from "../service.ts";

const env = { url: process.env.AIRTRAIL_URL, db: process.env.AIRTRAIL_DB_URL };
const LABEL = "Flights";

export const AIRTRAIL_SCOPES = ["profile.read", "flight.read.own", "stats.read", "reference_data.read"];

type Airport = { iata?: string | null; icao: string; name: string; municipality?: string | null; country: string };
export type Flight = {
  id: number;
  date: string;
  departure?: string | null;
  arrival?: string | null;
  duration?: number | null;
  flightNumber?: string | null;
  from?: Airport | null;
  to?: Airport | null;
  airline?: { name: string } | null;
  aircraft?: { name: string } | null;
  passengers?: { seatClass?: string | null; seat?: string | null }[];
};

const keys = new TtlCache<string>(12 * 3600_000);

/**
 * Issues a read-only AirTrail key. AirTrail links SSO accounts by the OIDC
 * subject and stores base64(SHA-256(key)) (see src/lib/server/utils/hash.ts).
 */
async function apiKey(member: Member) {
  return keys.get(member.id, async () => {
    const db = pool(env.db!);
    const user = await db.query<{ id: string }>(`SELECT id FROM "user" WHERE oauth_id = $1`, [member.sub]);
    const userId = user.rows[0]?.id;
    if (!userId) throw new ToolError("Flights has not been opened by this person yet, so there is nothing to search");
    const key = newApiKey();
    await db.query(`DELETE FROM api_key WHERE user_id = $1 AND name = $2`, [userId, KEY_NAME]);
    await db.query(`INSERT INTO api_key (name, user_id, key, scopes) VALUES ($1, $2, $3, $4)`, [
      KEY_NAME, userId, sha256(key.normalize("NFKC")).toString("base64"), AIRTRAIL_SCOPES,
    ]);
    return key;
  });
}

async function airtrail<T>(member: Member, path: string, query: Record<string, string | number | undefined> = {}) {
  const key = await apiKey(member);
  try {
    return await getJson<T>(env.url!, path, { headers: { Authorization: `Bearer ${key}` }, query, label: LABEL });
  } catch (err) {
    keys.delete(member.id);
    throw err;
  }
}

const airport = (a?: Airport | null) =>
  a ? `${a.municipality ?? a.name} (${a.iata ?? a.icao}, ${a.country})` : undefined;

export function summarizeFlight(f: Flight) {
  return compact({
    date: f.date,
    from: airport(f.from),
    to: airport(f.to),
    flight: f.flightNumber ?? undefined,
    airline: f.airline?.name,
    aircraft: f.aircraft?.name,
    departure: f.departure?.slice(0, 16),
    minutes: f.duration ? Math.round(f.duration / 60) : undefined,
    seat: f.passengers?.[0]?.seat ?? undefined,
    class: f.passengers?.[0]?.seatClass ?? undefined,
  });
}

const matchesAirport = (a: Airport | null | undefined, q: string) =>
  !!a && [a.iata, a.icao, a.name, a.municipality, a.country].some((v) => v?.toLowerCase().includes(q));

export function filterFlights(flights: Flight[], opts: { from?: string; to?: string; airport?: string }) {
  const q = opts.airport?.toLowerCase();
  return flights.filter(
    (f) =>
      (!opts.from || f.date >= opts.from) &&
      (!opts.to || f.date <= opts.to) &&
      (!q || matchesAirport(f.from, q) || matchesAirport(f.to, q)),
  );
}

export const flights: ServiceModule = {
  key: "flights",
  configured: Boolean(env.url && env.db),
  register(server, { member, portalOrigin }) {
    server.registerTool(
      "list_flights",
      {
        title: "List flights",
        description:
          "The user's flights, newest first, optionally filtered by date range and by an airport, city or country " +
          "(matches departure or arrival).",
        inputSchema: {
          from: z.string().optional().describe("On or after (YYYY-MM-DD)"),
          to: z.string().optional().describe("On or before (YYYY-MM-DD)"),
          airport: z.string().max(80).optional().describe("IATA/ICAO code, airport, city or country"),
          limit: z.number().int().min(1).max(25).optional(),
        },
        annotations: READ_ONLY,
      },
      (args) =>
        answer(async () => {
          const all: Flight[] = [];
          let cursor: string | undefined;
          // A frequent flyer has a few hundred flights; stop at 1000.
          for (let i = 0; i < 10; i++) {
            const page = await airtrail<{ data: Flight[]; page: { nextCursor: string | null } }>(member, "/api/v1/flights", {
              scope: "mine", limit: 100, cursor,
            });
            all.push(...page.data);
            if (!page.page.nextCursor) break;
            cursor = page.page.nextCursor;
          }
          const matched = filterFlights(all, {
            from: isoDate(args.from)?.slice(0, 10),
            to: isoDate(args.to, true)?.slice(0, 10),
            airport: args.airport,
          }).sort((a, b) => b.date.localeCompare(a.date));
          const items = matched.slice(0, clampLimit(args.limit));
          return { total: matched.length, items: items.map(summarizeFlight), link: `${portalOrigin}/flights/` };
        }),
    );

    server.registerTool(
      "flight_stats",
      {
        title: "Flight statistics",
        description: "Totals over all of the user's flights: count, distance, time in the air, airports, top route/airline.",
        inputSchema: {},
        annotations: READ_ONLY,
      },
      () =>
        answer(async () => {
          type Stats = {
            flights: number; distanceKm: number; durationSeconds: number; airports: number;
            topAirline?: unknown; topAirport?: unknown; topRoute?: unknown;
          };
          const { data } = await airtrail<{ data: Stats }>(member, "/api/v1/stats", { scope: "mine" });
          if (!data) throw new ToolError("no statistics yet");
          return compact({
            flights: data.flights,
            distanceKm: data.distanceKm,
            hoursInAir: Math.round(data.durationSeconds / 360) / 10,
            airports: data.airports,
            topAirline: data.topAirline,
            topAirport: data.topAirport,
            topRoute: data.topRoute,
          });
        }),
    );
  },
};

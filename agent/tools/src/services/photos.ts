// Photos (Immich). Search runs inside Immich: text queries use its own CLIP
// model on the node, so no image ever leaves the stack. Answers are metadata
// only (date, place, people, caption) plus a link to open the photo.
import { z } from "zod";
import { KEY_NAME, newApiKey, pool, sha256 } from "../db.ts";
import { TtlCache, getJson } from "../http.ts";
import { ToolError, clampLimit, compact, excerpt, isoDate, localTime } from "../limits.ts";
import type { Member } from "../members.ts";
import { READ_ONLY, answer, type ServiceModule } from "../service.ts";

const env = { url: process.env.IMMICH_URL, db: process.env.IMMICH_DB_URL };
const LABEL = "Photos";

/** Enough to search and name people; nothing that changes or downloads. */
export const IMMICH_PERMISSIONS = ["asset.read", "person.read", "album.read"];

type Asset = {
  id: string;
  type: string;
  localDateTime?: string;
  fileCreatedAt?: string;
  isFavorite?: boolean;
  exifInfo?: { city?: string | null; state?: string | null; country?: string | null; description?: string | null };
  people?: { name?: string }[];
};
type SearchResult = { assets: { items: Asset[]; total: number } };
type Person = { id: string; name: string; isHidden?: boolean };

const keys = new TtlCache<string>(12 * 3600_000);

/**
 * Issues the member a read-only Immich API key (replacing the previous one).
 * Immich stores the SHA-256 digest of the key (see its auth.service.ts).
 */
async function apiKey(member: Member): Promise<string> {
  return keys.get(member.id, async () => {
    const db = pool(env.db!);
    const user = await db.query<{ id: string }>(
      `SELECT id FROM "user" WHERE lower(email) = lower($1) AND "deletedAt" IS NULL`,
      [member.email],
    );
    const userId = user.rows[0]?.id;
    if (!userId) throw new ToolError("Photos has not been opened by this person yet, so there is nothing to search");
    const key = newApiKey();
    await db.query(`DELETE FROM api_key WHERE "userId" = $1 AND name = $2`, [userId, KEY_NAME]);
    await db.query(`INSERT INTO api_key (name, key, "userId", permissions) VALUES ($1, $2, $3, $4)`, [
      KEY_NAME, sha256(key), userId, IMMICH_PERMISSIONS,
    ]);
    return key;
  });
}

async function immich<T>(member: Member, path: string, opts: { method?: "GET" | "POST"; body?: unknown; query?: Record<string, string> } = {}) {
  const key = await apiKey(member);
  try {
    return await getJson<T>(env.url!, path, { ...opts, headers: { "x-api-key": key }, label: LABEL });
  } catch (err) {
    keys.delete(member.id); // a revoked key is re-issued on the next call
    throw err;
  }
}

export function summarizeAsset(a: Asset, portalOrigin: string) {
  const e = a.exifInfo ?? {};
  return compact({
    // localDateTime is the wall-clock time where the photo was taken.
    taken: a.localDateTime?.slice(0, 16) ?? (a.fileCreatedAt ? localTime(a.fileCreatedAt) : undefined),
    type: a.type === "VIDEO" ? "video" : undefined,
    place: [e.city, e.country].filter(Boolean).join(", ") || undefined,
    caption: excerpt(e.description, 160),
    people: a.people?.map((p) => p.name).filter(Boolean),
    favorite: a.isFavorite || undefined,
    link: `${portalOrigin}/photos/photos/${a.id}`,
  });
}

export const photos: ServiceModule = {
  key: "photos",
  configured: Boolean(env.url && env.db),
  register(server, { member, portalOrigin }) {
    server.registerTool(
      "search_photos",
      {
        title: "Search photos",
        description:
          "Find the user's photos and videos. `query` is a free-text description of what is in the picture " +
          "(e.g. 'dog on the beach', 'birthday cake'); combine with person, place and date filters. " +
          "Returns date, place, people and a link per item, never the image itself.",
        inputSchema: {
          query: z.string().max(200).optional().describe("What the photo shows"),
          person: z.string().max(100).optional().describe("Name of a person recognised in Photos"),
          city: z.string().max(100).optional(),
          country: z.string().max(100).optional(),
          from: z.string().optional().describe("Taken on or after (YYYY-MM-DD)"),
          to: z.string().optional().describe("Taken on or before (YYYY-MM-DD)"),
          limit: z.number().int().min(1).max(25).optional(),
        },
        annotations: READ_ONLY,
      },
      (args) =>
        answer(async () => {
          const size = clampLimit(args.limit);
          let personIds: string[] | undefined;
          if (args.person) {
            const found = await immich<Person[]>(member, "/api/search/person", { query: { name: args.person } });
            if (!found.length) throw new ToolError(`no person named "${args.person}" in Photos`);
            personIds = found.slice(0, 3).map((p) => p.id);
          }
          const filters = compact({
            city: args.city,
            country: args.country,
            takenAfter: isoDate(args.from),
            takenBefore: isoDate(args.to, true),
            personIds,
            withExif: true,
            withPeople: true,
            size,
          });
          const result = args.query
            ? await immich<SearchResult>(member, "/api/search/smart", { method: "POST", body: { ...filters, query: args.query } })
            : await immich<SearchResult>(member, "/api/search/metadata", { method: "POST", body: { ...filters, order: "desc" } });
          const items = result.assets.items.slice(0, size);
          return { count: items.length, items: items.map((a) => summarizeAsset(a, portalOrigin)) };
        }),
    );

    server.registerTool(
      "list_people",
      {
        title: "List people in photos",
        description: "Names of the people the user has named in Photos (face recognition runs locally).",
        inputSchema: { limit: z.number().int().min(1).max(25).optional() },
        annotations: READ_ONLY,
      },
      (args) =>
        answer(async () => {
          const res = await immich<{ people: Person[] }>(member, "/api/people", { query: { size: "100", withHidden: "false" } });
          const named = res.people.filter((p) => p.name && !p.isHidden).map((p) => p.name);
          return { people: named.slice(0, clampLimit(args.limit, 25)) };
        }),
    );
  },
};

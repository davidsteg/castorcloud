// Documents (Paperless-ngx). Paperless has no per-user API keys for SSO
// accounts, so the tools use the stack's service account and apply Paperless'
// own visibility rules for the asking member (owner, shared with the member or
// one of their groups, or unowned; superusers see everything).
import { z } from "zod";
import { TtlCache, getJson } from "../http.ts";
import { DOCUMENT_CHARS, ToolError, clampLimit, compact, excerpt, isoDate, localDay } from "../limits.ts";
import type { Member } from "../members.ts";
import { READ_ONLY, answer, type ServiceModule } from "../service.ts";

const env = { url: process.env.PAPERLESS_URL, user: process.env.PAPERLESS_USER, password: process.env.PAPERLESS_PASSWORD };
const LABEL = "Documents";

type Perms = { view?: { users?: number[]; groups?: number[] }; change?: { users?: number[]; groups?: number[] } };
export type PaperlessDoc = {
  id: number;
  title: string;
  created?: string;
  created_date?: string;
  correspondent?: number | null;
  document_type?: number | null;
  tags?: number[];
  owner?: number | null;
  permissions?: Perms;
  content?: string;
  __search_hit__?: { highlights?: string };
};
export type PaperlessUser = { id: number; email?: string; username: string; is_superuser?: boolean; groups?: number[] };
type Page<T> = { results: T[]; next?: string | null };
type Named = { id: number; name: string };

const auth = () => ({ Authorization: `Basic ${Buffer.from(`${env.user}:${env.password}`).toString("base64")}` });
const paperless = <T>(path: string, query?: Record<string, string | number | boolean | undefined>) =>
  getJson<T>(env.url!, path, { headers: auth(), query, label: LABEL });

const users = new TtlCache<PaperlessUser | undefined>(5 * 60_000);
const names = new TtlCache<Map<number, string>>(5 * 60_000);

async function paperlessUser(member: Member) {
  const user = await users.get(member.email.toLowerCase(), async () => {
    const page = await paperless<Page<PaperlessUser>>("/api/users/", { page_size: 1000 });
    return page.results.find((u) => u.email?.toLowerCase() === member.email.toLowerCase());
  });
  if (!user) {
    users.delete(member.email.toLowerCase());
    throw new ToolError("Documents has not been opened by this person yet, so there is nothing to search");
  }
  return user;
}

/** Paperless' visibility rule for one document and one user. */
export function canView(doc: PaperlessDoc, user: PaperlessUser) {
  if (user.is_superuser) return true;
  if (doc.owner == null || doc.owner === user.id) return true;
  const p = doc.permissions ?? {};
  const inUsers = [...(p.view?.users ?? []), ...(p.change?.users ?? [])].includes(user.id);
  const groups = [...(p.view?.groups ?? []), ...(p.change?.groups ?? [])];
  return inUsers || groups.some((g) => user.groups?.includes(g));
}

const lookup = (kind: "correspondents" | "document_types" | "tags") =>
  names.get(kind, async () => {
    const page = await paperless<Page<Named>>(`/api/${kind}/`, { page_size: 1000 });
    return new Map(page.results.map((r) => [r.id, r.name]));
  });

async function describe(docs: PaperlessDoc[], portalOrigin: string) {
  const [correspondents, types, tags] = await Promise.all([lookup("correspondents"), lookup("document_types"), lookup("tags")]);
  return docs.map((d) =>
    compact({
      id: d.id,
      title: d.title,
      date: (d.created_date ?? d.created)?.slice(0, 10),
      from: d.correspondent ? correspondents.get(d.correspondent) : undefined,
      type: d.document_type ? types.get(d.document_type) : undefined,
      tags: d.tags?.map((t) => tags.get(t)).filter(Boolean),
      excerpt: excerpt(d.__search_hit__?.highlights),
      link: `${portalOrigin}/docs/documents/${d.id}/details`,
    }),
  );
}

const FIELDS = "id,title,created,created_date,correspondent,document_type,tags,owner,permissions,__search_hit__";

export const docs: ServiceModule = {
  key: "docs",
  configured: Boolean(env.url && env.user && env.password),
  register(server, { member, portalOrigin }) {
    server.registerTool(
      "search_documents",
      {
        title: "Search documents",
        description:
          "Full-text search over the user's scanned documents (invoices, contracts, letters, receipts). " +
          "Returns title, date, sender, type, tags and a short matching excerpt. Use read_document for more text.",
        inputSchema: {
          query: z.string().min(1).max(200).describe("Words to look for, e.g. 'hotel lisbon invoice'"),
          from: z.string().optional().describe("Document date on or after (YYYY-MM-DD)"),
          to: z.string().optional().describe("Document date on or before (YYYY-MM-DD)"),
          limit: z.number().int().min(1).max(25).optional(),
        },
        annotations: READ_ONLY,
      },
      (args) =>
        answer(async () => {
          const user = await paperlessUser(member);
          const limit = clampLimit(args.limit);
          const page = await paperless<Page<PaperlessDoc>>("/api/documents/", {
            query: args.query,
            full_perms: true,
            fields: FIELDS,
            page_size: 50, // filtered for this member below
            // Paperless filters by calendar day.
            created__date__gte: args.from && localDay(isoDate(args.from)!),
            created__date__lte: args.to && localDay(isoDate(args.to, true)!),
          });
          const visible = page.results.filter((d) => canView(d, user)).slice(0, limit);
          return { count: visible.length, items: await describe(visible, portalOrigin) };
        }),
    );

    server.registerTool(
      "read_document",
      {
        title: "Read a document",
        description:
          `Text (OCR) of one document found with search_documents, cut to at most ${DOCUMENT_CHARS} characters. ` +
          "Use `offset` to continue reading a long document.",
        inputSchema: {
          id: z.number().int().positive(),
          offset: z.number().int().min(0).optional(),
          maxChars: z.number().int().min(200).max(DOCUMENT_CHARS).optional(),
        },
        annotations: READ_ONLY,
      },
      (args) =>
        answer(async () => {
          const user = await paperlessUser(member);
          const doc = await paperless<PaperlessDoc>(`/api/documents/${args.id}/`, { full_perms: true });
          if (!canView(doc, user)) throw new ToolError("not found in Documents"); // same answer as a missing id
          const text = (doc.content ?? "").replace(/[ \t]+/g, " ").replace(/\n{3,}/g, "\n\n").trim();
          const offset = args.offset ?? 0;
          const max = args.maxChars ?? DOCUMENT_CHARS;
          const [meta] = await describe([doc], portalOrigin);
          return {
            ...meta,
            text: text.slice(offset, offset + max),
            ...(offset + max < text.length ? { continuesAt: offset + max, totalChars: text.length } : {}),
          };
        }),
    );
  },
};

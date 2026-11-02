import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { prisma, type Prisma } from "@pc/db";
import { AUDIT_PAGE, auditDetails, auditFilter, auditQuery } from "@pc/core";
import { getDictionary, type Locale } from "@/i18n";
import { staffUser } from "@/lib/session";
import { SiteHeader } from "@/components/site/SiteHeader";

// The audit log for staff, read-only: newest first, filtered by action,
// tenant or actor. The database lets the portal only read and add rows here
// (docs/security.md); this page only reads. Everyone else gets a 404.
export const metadata: Metadata = { robots: { index: false } };

type Search = Record<string, string | string[] | undefined>;

export default async function AuditPage({
  params,
  searchParams,
}: {
  params: Promise<{ locale: Locale }>;
  searchParams: Promise<Search>;
}) {
  const { locale } = await params;
  const staff = await staffUser();
  if (!staff) notFound();
  const t = getDictionary(locale);
  const f = auditFilter(await searchParams);

  const where: Prisma.AuditLogWhereInput = {};
  if (f.action) where.action = f.action.prefix ? { startsWith: f.action.value } : f.action.value;
  if (f.actor) where.actor = f.actor;
  if (f.before) where.createdAt = { lt: f.before };
  if (f.slug) {
    const tenant = await prisma.tenant.findUnique({ where: { slug: f.slug }, select: { id: true } });
    // An unknown slug matches nothing (a deleted application frees its slug).
    where.tenantId = tenant?.id ?? "-";
  }
  const rows = await prisma.auditLog.findMany({
    where,
    orderBy: [{ createdAt: "desc" }, { id: "desc" }],
    take: AUDIT_PAGE + 1,
  });
  const more = rows.length > AUDIT_PAGE;
  const page = rows.slice(0, AUDIT_PAGE);

  // Names for ids: users by e-mail, tenants by slug.
  const [users, tenants] = await Promise.all([
    prisma.user.findMany({ where: { id: { in: [...new Set(page.map((r) => r.actor))] } }, select: { id: true, email: true } }),
    prisma.tenant.findMany({
      where: { id: { in: [...new Set(page.flatMap((r) => (r.tenantId ? [r.tenantId] : [])))] } },
      select: { id: true, slug: true },
    }),
  ]);
  const email = new Map(users.map((u) => [u.id, u.email]));
  const slug = new Map(tenants.map((x) => [x.id, x.slug]));
  const base = `/${locale}/admin/audit`;

  return (
    <>
      <SiteHeader locale={locale} t={t} path="/admin" />
      <main className="container" style={{ padding: "48px 24px" }}>
        <p className="small-print"><a href={`/${locale}/admin`}>← Staff</a></p>
        <h1>Audit log</h1>
        <p className="small-print" style={{ margin: "8px 0 16px" }}>
          Append-only: rows can be added, never changed or removed. Newest first, {AUDIT_PAGE} per page.
        </p>

        <form method="get" action={base} className="panel" style={{ display: "flex", gap: 12, flexWrap: "wrap", alignItems: "end", margin: "0 0 24px" }}>
          <label className="small-print">Action (or a prefix like <code>staff.</code>)<br />
            <input name="action" defaultValue={f.action?.value ?? ""} placeholder="staff." maxLength={64} />
          </label>
          <label className="small-print">Cloud (slug)<br />
            <input name="slug" defaultValue={f.slug ?? ""} maxLength={63} />
          </label>
          <label className="small-print">Actor<br />
            <input name="actor" defaultValue={f.actor ?? ""} placeholder="ops-cli:david" maxLength={80} />
          </label>
          <button className="button small" type="submit">Filter</button>
          <a className="button secondary small" href={base}>Reset</a>
        </form>

        {page.length === 0 ? (
          <p className="notice">No entries.</p>
        ) : (
          <table className="admin-table">
            <thead>
              <tr><th>Time (UTC)</th><th>Action</th><th>Actor</th><th>Cloud</th><th>Details</th></tr>
            </thead>
            <tbody>
              {page.map((r) => (
                <tr key={r.id}>
                  <td className="small-print" style={{ whiteSpace: "nowrap" }}>{r.createdAt.toISOString().slice(0, 19).replace("T", " ")}</td>
                  <td><a href={`${base}${auditQuery({ action: { value: r.action, prefix: false } })}`}>{r.action}</a></td>
                  <td className="small-print">{email.get(r.actor) ?? r.actor}</td>
                  <td className="small-print">{r.tenantId ? (slug.get(r.tenantId) ?? r.tenantId) : "–"}</td>
                  <td className="small-print" style={{ maxWidth: 420, overflowWrap: "anywhere" }}><code>{auditDetails(r.details)}</code></td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {more ? (
          <p style={{ marginTop: 16 }}>
            <a className="button secondary small" href={`${base}${auditQuery(f, page[page.length - 1].createdAt)}`}>Older entries</a>
          </p>
        ) : null}
      </main>
    </>
  );
}

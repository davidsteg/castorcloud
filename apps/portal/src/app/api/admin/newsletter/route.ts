// Staff only: the confirmed newsletter subscribers as CSV, each with the
// unsubscribe link its mails must carry. Audited.
import { NextResponse } from "next/server";
import { prisma } from "@pc/db";
import { audit } from "@pc/core";
import { unsubscribeUrl } from "@/lib/newsletter";
import { staffUser } from "@/lib/session";

export const runtime = "nodejs";

/** Quoted CSV field; a leading =, +, - or @ is defused for spreadsheet apps. */
function field(value: string) {
  const safe = /^[=+\-@\t\r]/.test(value) ? `'${value}` : value;
  return `"${safe.replace(/"/g, '""')}"`;
}

export async function GET() {
  const staff = await staffUser();
  if (!staff) return new NextResponse("Not found", { status: 404 });

  const subs = await prisma.newsletterSubscriber.findMany({ where: { status: "CONFIRMED" }, orderBy: { confirmedAt: "asc" } });
  await audit(staff.id, "newsletter.export", undefined, { count: subs.length });

  const rows = [
    ["email", "locale", "confirmed_at", "unsubscribe_url"],
    ...subs.map((s) => [s.email, s.locale, s.confirmedAt?.toISOString() ?? "", unsubscribeUrl(s)]),
  ];
  return new NextResponse(rows.map((r) => r.map(field).join(",")).join("\r\n") + "\r\n", {
    headers: {
      "Content-Type": "text/csv; charset=utf-8",
      "Content-Disposition": `attachment; filename="newsletter-${new Date().toISOString().slice(0, 10)}.csv"`,
      "Cache-Control": "no-store",
    },
  });
}

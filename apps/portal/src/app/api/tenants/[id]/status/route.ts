import { NextResponse } from "next/server";
import { prisma } from "@pc/db";
import { memberTenant } from "@/lib/session";

// Polled by the welcome page while the tenant is being provisioned.
export async function GET(_req: Request, { params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  const tenant = await memberTenant(id);
  if (!tenant) return NextResponse.json({ error: "not found" }, { status: 404 });

  const job = await prisma.job.findFirst({
    where: { tenantId: id, kind: "PROVISION" },
    orderBy: { createdAt: "desc" },
    select: { status: true, step: true },
  });
  return NextResponse.json({ status: tenant.status, step: job?.step ?? null, jobStatus: job?.status ?? null });
}

// One-click unsubscribe (RFC 8058): mail clients POST here with the
// List-Unsubscribe URL from the newsletter's headers.
import { NextResponse, type NextRequest } from "next/server";
import { unsubscribeWithToken } from "@/lib/newsletter";

export const runtime = "nodejs";

export async function POST(req: NextRequest) {
  const q = req.nextUrl.searchParams;
  const ok = await unsubscribeWithToken(q.get("id"), q.get("t"));
  return new NextResponse(null, { status: ok ? 200 : 400 });
}

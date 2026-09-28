import { NextResponse } from "next/server";
import { memberAssistant, sameOrigin } from "@/lib/assistant";

export const dynamic = "force-dynamic";

type HermesSession = { id: string; title?: string | null; preview?: string | null; last_active?: number; message_count?: number };

/** The member's conversations, newest first (empty ones left out). */
export async function GET() {
  const assistant = await memberAssistant();
  if (!assistant) return NextResponse.json({ error: "not found" }, { status: 404 });
  const res = await assistant.hermes("/sessions?limit=50");
  if (!res.ok) return NextResponse.json({ error: "unavailable" }, { status: 502 });
  const { data } = (await res.json()) as { data: HermesSession[] };
  return NextResponse.json(
    data
      .filter((s) => s.message_count)
      .map((s) => ({ id: s.id, title: s.title || s.preview || "", at: s.last_active ? Math.round(s.last_active * 1000) : null })),
  );
}

/** Starts a new conversation. */
export async function POST(req: Request) {
  if (!sameOrigin(req)) return NextResponse.json({ error: "forbidden" }, { status: 403 });
  const assistant = await memberAssistant();
  if (!assistant) return NextResponse.json({ error: "not found" }, { status: 404 });
  // No title: Hermes titles must be unique and it names sessions itself.
  const res = await assistant.hermes("/sessions", { method: "POST", body: "{}" });
  if (!res.ok) return NextResponse.json({ error: "unavailable" }, { status: 502 });
  const session = (await res.json()) as { id?: string; session?: { id: string } };
  return NextResponse.json({ id: session.id ?? session.session?.id });
}

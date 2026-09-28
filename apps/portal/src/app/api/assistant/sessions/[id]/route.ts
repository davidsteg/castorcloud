import { NextResponse } from "next/server";
import { SESSION_ID_RE, memberAssistant } from "@/lib/assistant";

export const dynamic = "force-dynamic";

type HermesMessage = { role: string; content?: string | null; display_kind?: string | null };

/** A conversation's user and assistant turns (tool calls and results left out). */
export async function GET(_req: Request, { params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  const assistant = await memberAssistant();
  if (!assistant || !SESSION_ID_RE.test(id)) return NextResponse.json({ error: "not found" }, { status: 404 });
  const res = await assistant.hermes(`/sessions/${id}/messages?order=oldest`);
  if (!res.ok) return NextResponse.json({ error: "not found" }, { status: res.status === 404 ? 404 : 502 });
  const { data } = (await res.json()) as { data: HermesMessage[] };
  return NextResponse.json(
    data
      .filter((m) => (m.role === "user" || m.role === "assistant") && m.content && !m.display_kind)
      .map((m) => ({ role: m.role, text: m.content })),
  );
}

/** Deletes a conversation (the assistant's long-term memory is kept). */
export async function DELETE(_req: Request, { params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  const assistant = await memberAssistant();
  if (!assistant || !SESSION_ID_RE.test(id)) return NextResponse.json({ error: "not found" }, { status: 404 });
  const res = await assistant.hermes(`/sessions/${id}`, { method: "DELETE" });
  return new NextResponse(null, { status: res.ok ? 204 : 502 });
}

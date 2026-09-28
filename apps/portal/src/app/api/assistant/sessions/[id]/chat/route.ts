import { NextResponse } from "next/server";
import { SESSION_ID_RE, memberAssistant } from "@/lib/assistant";
import { chatEvents, type ChatEvent } from "@pc/core";

export const dynamic = "force-dynamic";

const MAX_INPUT = 4000;

/** Sends a message; answers with the reply as newline-delimited JSON events. */
export async function POST(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  const assistant = await memberAssistant();
  if (!assistant || !SESSION_ID_RE.test(id)) return NextResponse.json({ error: "not found" }, { status: 404 });
  const { input } = (await req.json().catch(() => ({}))) as { input?: unknown };
  if (typeof input !== "string" || !input.trim() || input.length > MAX_INPUT) {
    return NextResponse.json({ error: "invalid input" }, { status: 400 });
  }

  const upstream = await assistant
    .hermes(`/sessions/${id}/chat/stream`, { method: "POST", body: JSON.stringify({ input }), signal: req.signal })
    .catch(() => null);
  if (!upstream?.ok || !upstream.body) {
    return NextResponse.json({ error: "unavailable" }, { status: upstream?.status === 404 ? 404 : 502 });
  }

  const body = upstream.body;
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    async start(controller) {
      const send = (e: ChatEvent) => controller.enqueue(encoder.encode(`${JSON.stringify(e)}\n`));
      try {
        for await (const event of chatEvents(body)) send(event);
      } catch {
        send({ type: "error", code: "failed" });
      }
      controller.close();
    },
  });
  return new Response(stream, {
    headers: { "Content-Type": "application/x-ndjson; charset=utf-8", "Cache-Control": "no-store", "X-Accel-Buffering": "no" },
  });
}

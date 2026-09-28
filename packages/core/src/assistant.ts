// Turns the SSE chat stream of the tenant assistant (Hermes) into the few
// events the portal chat and the Telegram bot need. Tool arguments and results
// stay on the server; users only learn which service is being searched.
import { createHash, randomInt } from "node:crypto";

/**
 * Each member's API key for their assistant profile, kept in the Secret table
 * (written by the provisioner, read by the portal and the Telegram bot).
 */
export const assistantKeyName = (memberId: string) => `ASSISTANT_KEY_${memberId}`;

/** A member's Hermes profile API, reached through the tenant's skin proxy. */
export const assistantApi = (opts: { scheme: string; baseDomain: string; slug: string; memberId: string }) =>
  `${opts.scheme}://assistant-${opts.slug}.${opts.baseDomain}/p/${opts.memberId}/api`;

/** Telegram pairing codes: 8 characters without look-alikes, e.g. "K7M3Q9TX". */
const CODE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
export const PAIRING_MINUTES = 10;

export const newPairingCode = () => Array.from({ length: 8 }, () => CODE_ALPHABET[randomInt(CODE_ALPHABET.length)]).join("");

/** Stored instead of the code; tolerant of case, spaces and dashes. */
export const pairingCodeHash = (code: string) =>
  createHash("sha256").update(code.toUpperCase().replace(/[^A-Z0-9]/g, "")).digest("hex");

export type ChatEvent =
  | { type: "delta"; text: string }
  | { type: "tool"; service: string }
  | { type: "done"; text: string }
  | { type: "error"; code: "credit" | "failed" };

/** LiteLLM's answer once the tenant's monthly budget is used up. */
const BUDGET_RE = /budget has been exceeded|budget_exceeded/i;

/** "mcp__docs__search_documents" -> "docs"; built-in tools -> "memory". */
export function toolService(name: string | undefined) {
  const m = /^mcp__([a-z]+)__/.exec(name ?? "");
  return m ? m[1] : "memory";
}

export function translate(event: string, data: Record<string, unknown>): ChatEvent | null {
  switch (event) {
    case "assistant.delta":
      return typeof data.delta === "string" && data.delta ? { type: "delta", text: data.delta } : null;
    case "tool.started":
      return { type: "tool", service: toolService(data.tool_name as string | undefined) };
    case "assistant.completed": {
      const text = typeof data.content === "string" ? data.content : "";
      // Hermes reports provider errors as the turn's text.
      return BUDGET_RE.test(text) ? { type: "error", code: "credit" } : { type: "done", text };
    }
    case "run.failed":
      return { type: "error", code: BUDGET_RE.test(JSON.stringify(data)) ? "credit" : "failed" };
    default:
      return null;
  }
}

/** Parses an SSE body and yields translated events; stops after the first done/error. */
export async function* chatEvents(body: ReadableStream<Uint8Array>): AsyncGenerator<ChatEvent> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let end: number;
      while ((end = buffer.indexOf("\n\n")) >= 0) {
        const block = buffer.slice(0, end);
        buffer = buffer.slice(end + 2);
        let event = "message";
        let data = "";
        for (const line of block.split("\n")) {
          if (line.startsWith("event:")) event = line.slice(6).trim();
          else if (line.startsWith("data:")) data += (data ? "\n" : "") + line.slice(5).trimStart();
        }
        let parsed: Record<string, unknown> = {};
        try {
          parsed = data ? JSON.parse(data) : {};
        } catch {
          continue;
        }
        const out = translate(event, parsed);
        if (!out) continue;
        yield out;
        if (out.type === "done" || out.type === "error") return;
      }
    }
    yield { type: "error", code: "failed" }; // stream ended without an answer
  } finally {
    reader.cancel().catch(() => {});
  }
}

"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import type { Dictionary } from "@/i18n";
import { Markdown } from "@/components/Markdown";

type Labels = Dictionary["chat"];
type Message = { role: "user" | "assistant"; text: string };
type Session = { id: string; title: string; at: number | null };
type ChatEvent =
  | { type: "delta"; text: string }
  | { type: "tool"; service: string }
  | { type: "done"; text: string }
  | { type: "error"; code: "credit" | "failed" };

async function json<T>(res: Response): Promise<T> {
  if (!res.ok) throw new Error(String(res.status));
  return res.json() as Promise<T>;
}

/**
 * The assistant chat. Talks to /api/assistant/*, which forwards to the
 * member's own assistant; answers stream in as they are written.
 */
export function AssistantChat({ labels, creditExhausted, side }: { labels: Labels; creditExhausted: boolean; side: React.ReactNode }) {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [activity, setActivity] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(creditExhausted ? labels.creditExhausted : null);
  const [blocked, setBlocked] = useState(creditExhausted);
  const [sideOpen, setSideOpen] = useState(false); // phones: conversations collapsed
  const endRef = useRef<HTMLDivElement>(null);
  const abortRef = useRef<AbortController | null>(null);

  const loadSessions = useCallback(() => {
    fetch("/api/assistant/sessions").then(json<Session[]>).then(setSessions).catch(() => {});
  }, []);
  useEffect(loadSessions, [loadSessions]);
  useEffect(() => endRef.current?.scrollIntoView({ block: "end" }), [messages, activity]);
  useEffect(() => () => abortRef.current?.abort(), []);

  async function open(id: string) {
    if (busy) return;
    setError(blocked ? labels.creditExhausted : null);
    setSessionId(id);
    setMessages([]);
    setSideOpen(false);
    try {
      setMessages(await json<Message[]>(await fetch(`/api/assistant/sessions/${id}`)));
    } catch {
      setError(labels.unavailable);
    }
  }

  function startNew() {
    if (busy) return;
    setSessionId(null);
    setMessages([]);
    setSideOpen(false);
    setError(blocked ? labels.creditExhausted : null);
  }

  async function remove(id: string) {
    await fetch(`/api/assistant/sessions/${id}`, { method: "DELETE" });
    if (id === sessionId) startNew();
    loadSessions();
  }

  async function send(text: string) {
    text = text.trim();
    if (!text || busy || blocked) return;
    setBusy(true);
    setError(null);
    setInput("");
    setActivity(labels.thinking);
    setMessages((m) => [...m, { role: "user", text }, { role: "assistant", text: "" }]);
    const setReply = (reply: string) =>
      setMessages((m) => [...m.slice(0, -1), { role: "assistant", text: reply }]);

    const abort = new AbortController();
    abortRef.current = abort;
    let reply = "";
    let failed: string | null = null;
    try {
      let id = sessionId;
      if (!id) {
        id = (await json<{ id: string }>(await fetch("/api/assistant/sessions", { method: "POST" }))).id;
        setSessionId(id);
      }
      const res = await fetch(`/api/assistant/sessions/${id}/chat`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ input: text }),
        signal: abort.signal,
      });
      if (!res.ok || !res.body) throw new Error(String(res.status));

      const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
      let buffer = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += value;
        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";
        for (const line of lines.filter(Boolean)) {
          const e = JSON.parse(line) as ChatEvent;
          if (e.type === "delta") {
            reply += e.text;
            setReply(reply.trimStart());
            setActivity(null);
          } else if (e.type === "tool") {
            setActivity(labels.searching[e.service as keyof Labels["searching"]] ?? labels.thinking);
          } else if (e.type === "done") {
            reply = e.text;
            setReply(reply);
          } else if (e.code === "credit") {
            failed = labels.creditExhausted;
            setBlocked(true);
          } else {
            failed = labels.failed;
          }
        }
      }
    } catch {
      if (!abort.signal.aborted) failed = labels.unavailable;
    }
    if (failed) {
      setError(failed);
      // Keep what was written; drop an empty reply bubble.
      if (!reply.trim()) setMessages((m) => m.slice(0, -1));
    }
    setActivity(null);
    setBusy(false);
    loadSessions();
  }

  return (
    <div className="chat-page">
      <aside className={`chat-side${sideOpen ? " open" : ""}`}>
        <div className="chat-side-bar">
          <button className="button small" type="button" onClick={startNew} disabled={busy}>+ {labels.newChat}</button>
          <button className="button secondary small chat-side-toggle" type="button" aria-expanded={sideOpen} onClick={() => setSideOpen((o) => !o)}>
            {labels.history}
          </button>
        </div>
        {sessions.length > 0 && (
          <details className="chat-history" open>
            <summary>{labels.history}</summary>
            <ul>
              {sessions.map((s) => (
                <li key={s.id} aria-current={s.id === sessionId ? "true" : undefined}>
                  <button type="button" onClick={() => open(s.id)} title={s.title}>{s.title || "…"}</button>
                  <button type="button" className="chat-delete" onClick={() => remove(s.id)} aria-label={labels.delete} title={labels.delete}>×</button>
                </li>
              ))}
            </ul>
          </details>
        )}
        <div className="chat-side-extra">{side}</div>
      </aside>

      <section className="chat-main">
        <div className="chat-log" aria-live="polite">
          {messages.length === 0 ? (
            <div className="chat-empty">
              <h1>{labels.hello}</h1>
              <div className="chat-examples">
                {labels.examples.map((q) => (
                  <button key={q} type="button" className="button secondary small" onClick={() => send(q)} disabled={busy || blocked}>{q}</button>
                ))}
              </div>
            </div>
          ) : (
            messages.map((m, i) =>
              m.text || m.role === "user" ? (
                <div key={i} className={`chat-msg ${m.role}`}>
                  {m.role === "assistant" ? <Markdown text={m.text} /> : m.text}
                </div>
              ) : null,
            )
          )}
          {activity && <div className="chat-activity">{activity}</div>}
          {error && <p className="notice chat-error">{error}</p>}
          <div ref={endRef} />
        </div>

        <form
          className="chat-compose"
          onSubmit={(e) => {
            e.preventDefault();
            send(input);
          }}
        >
          <textarea
            className="input"
            rows={1}
            maxLength={4000}
            value={input}
            placeholder={labels.placeholder}
            disabled={blocked}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                send(input);
              }
            }}
          />
          <button className="button" type="submit" disabled={busy || blocked || !input.trim()}>{labels.send}</button>
        </form>
      </section>
    </div>
  );
}

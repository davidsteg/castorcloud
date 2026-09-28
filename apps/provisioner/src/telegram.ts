// The Castor Telegram bot: one bot for all tenants. Members opt in from the
// portal (Assistant -> Telegram), which shows a one-time pairing code; sending
// that code to the bot links their Telegram account to their own assistant.
// After that, every text message is forwarded to the member's Hermes profile
// and the answer is sent back.
//
// Telegram is not end-to-end encrypted, so the portal warns before pairing.
// Only private chats are served; group chats are ignored.
//
//   npm run telegram -w @pc/provisioner
import path from "node:path";
import { fileURLToPath } from "node:url";
import { config } from "dotenv";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
config({ path: path.join(root, ".env"), quiet: true });

const { prisma } = await import("@pc/db");
const { assistantApi, assistantKeyName, audit, chatEvents, openSecret, pairingCodeHash } = await import("@pc/core");

const TOKEN = process.env.TELEGRAM_BOT_TOKEN;
if (!TOKEN) throw new Error("TELEGRAM_BOT_TOKEN is not set");
const API = `${process.env.TELEGRAM_API_URL ?? "https://api.telegram.org"}/bot${TOKEN}`;
const platform = {
  scheme: process.env.PUBLIC_SCHEME ?? "https",
  baseDomain: process.env.BASE_DOMAIN ?? "brand.localhost",
};
const LLM_GATEWAY_URL = process.env.LLM_GATEWAY_URL ?? "http://localhost:4000";
const MAX_INPUT = 4000;
const MAX_MESSAGE = 4000; // Telegram allows 4096 characters per message

const TEXT = {
  en: {
    welcome: "Hello! To talk to your assistant here, open your cloud, go to Assistant → Telegram and send me the code shown there.",
    paired: "Connected. Ask your assistant anything. /new starts a new conversation, /disconnect unlinks this chat.\n\nReminder: Telegram is not end-to-end encrypted.",
    badCode: "This code is unknown or has expired. Please create a new one in your cloud under Assistant → Telegram.",
    newChat: "New conversation started.",
    disconnected: "Disconnected. Your assistant no longer answers here.",
    unavailable: "Your assistant is not available right now.",
    failed: "Something went wrong. Please try again.",
    credit: "Credit used up – resets on {date}.",
    textOnly: "I can only read text messages.",
  },
  de: {
    welcome: "Hallo! Um hier mit Ihrem Assistenten zu sprechen, öffnen Sie Ihre Cloud, gehen Sie zu Assistent → Telegram und senden Sie mir den angezeigten Code.",
    paired: "Verbunden. Fragen Sie Ihren Assistenten, was Sie möchten. /new startet eine neue Unterhaltung, /disconnect trennt diesen Chat.\n\nZur Erinnerung: Telegram ist nicht Ende-zu-Ende-verschlüsselt.",
    badCode: "Dieser Code ist unbekannt oder abgelaufen. Bitte erstellen Sie in Ihrer Cloud unter Assistent → Telegram einen neuen.",
    newChat: "Neue Unterhaltung gestartet.",
    disconnected: "Getrennt. Ihr Assistent antwortet hier nicht mehr.",
    unavailable: "Ihr Assistent ist gerade nicht verfügbar.",
    failed: "Etwas ist schiefgelaufen. Bitte versuchen Sie es nochmals.",
    credit: "Guthaben aufgebraucht – wird am {date} zurückgesetzt.",
    textOnly: "Ich kann nur Textnachrichten lesen.",
  },
  fr: {
    welcome: "Bonjour ! Pour parler ici à votre assistant, ouvrez votre cloud, allez dans Assistant → Telegram et envoyez-moi le code affiché.",
    paired: "Connecté. Posez vos questions à votre assistant. /new démarre une nouvelle discussion, /disconnect déconnecte ce chat.\n\nRappel : Telegram n'est pas chiffré de bout en bout.",
    badCode: "Ce code est inconnu ou a expiré. Veuillez en créer un nouveau dans votre cloud sous Assistant → Telegram.",
    newChat: "Nouvelle discussion démarrée.",
    disconnected: "Déconnecté. Votre assistant ne répond plus ici.",
    unavailable: "Votre assistant n'est pas disponible pour le moment.",
    failed: "Un problème est survenu. Veuillez réessayer.",
    credit: "Crédit épuisé – renouvelé le {date}.",
    textOnly: "Je ne peux lire que des messages texte.",
  },
  it: {
    welcome: "Ciao! Per parlare qui con il tuo assistente, apri il tuo cloud, vai su Assistente → Telegram e inviami il codice mostrato.",
    paired: "Collegato. Chiedi al tuo assistente quello che vuoi. /new avvia una nuova conversazione, /disconnect scollega questa chat.\n\nPromemoria: Telegram non è crittografato end-to-end.",
    badCode: "Questo codice è sconosciuto o scaduto. Creane uno nuovo nel tuo cloud in Assistente → Telegram.",
    newChat: "Nuova conversazione avviata.",
    disconnected: "Scollegato. Il tuo assistente non risponde più qui.",
    unavailable: "Il tuo assistente non è disponibile al momento.",
    failed: "Qualcosa è andato storto. Riprova.",
    credit: "Credito esaurito – si rinnova il {date}.",
    textOnly: "Posso leggere solo messaggi di testo.",
  },
};
type Lang = keyof typeof TEXT;
const lang = (...candidates: (string | null | undefined)[]): Lang =>
  (candidates.map((c) => c?.slice(0, 2)).find((c) => c && c in TEXT) as Lang | undefined) ?? "de";

type TgUser = { id: number; username?: string; first_name?: string; language_code?: string };
type TgMessage = { message_id: number; chat: { id: number; type: string }; from?: TgUser; text?: string };
type TgUpdate = { update_id: number; message?: TgMessage };

async function tg<T>(method: string, body: object, timeoutMs = 30_000): Promise<T> {
  const res = await fetch(`${API}/${method}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  });
  const data = (await res.json()) as { ok: boolean; result: T; description?: string };
  if (!data.ok) throw new Error(`Telegram ${method}: ${data.description}`);
  return data.result;
}

/** Plain text (no parse mode, so nothing in an answer is interpreted), split to fit. */
async function send(chatId: number, text: string) {
  for (let i = 0; i < text.length; i += MAX_MESSAGE) {
    await tg("sendMessage", { chat_id: chatId, text: text.slice(i, i + MAX_MESSAGE), link_preview_options: { is_disabled: true } });
  }
}

const linkFor = (telegramUserId: number) =>
  prisma.telegramLink.findUnique({
    where: { telegramUserId: BigInt(telegramUserId) },
    include: { member: { include: { user: true, tenant: { include: { services: true } } } } },
  });
type Link = NonNullable<Awaited<ReturnType<typeof linkFor>>>;

async function pair(msg: TgMessage, from: TgUser, code: string) {
  const t = TEXT[lang(from.language_code)];
  const pending = await prisma.telegramLink.findUnique({
    where: { pairingCodeHash: pairingCodeHash(code) },
    include: { member: { include: { user: true, tenant: true } } },
  });
  if (!pending?.pairingExpiresAt || pending.pairingExpiresAt < new Date()) return send(msg.chat.id, t.badCode);

  // One Telegram account talks to one assistant: the newest pairing wins.
  await prisma.$transaction([
    prisma.telegramLink.deleteMany({ where: { telegramUserId: BigInt(from.id), NOT: { id: pending.id } } }),
    prisma.telegramLink.update({
      where: { id: pending.id },
      data: {
        telegramUserId: BigInt(from.id),
        telegramUsername: from.username ?? from.first_name ?? null,
        linkedAt: new Date(),
        pairingCodeHash: null,
        pairingExpiresAt: null,
        sessionId: null,
      },
    }),
  ]);
  await audit(pending.member.userId, "assistant.telegram_linked", pending.member.tenantId);
  const m = pending.member;
  await send(msg.chat.id, TEXT[lang(m.user.locale, m.tenant.locale)].paired);
}

async function secret(tenantId: string, key: string) {
  const row = await prisma.secret.findUnique({ where: { tenantId_key: { tenantId, key } } });
  return row ? openSecret(tenantId, key, row.ciphertext) : undefined;
}

/** When the tenant's monthly AI credit resets, formatted for the member. */
async function resetDate(tenantId: string, locale: Lang) {
  let reset: Date | undefined;
  const key = await secret(tenantId, "LLM_VIRTUAL_KEY");
  if (key) {
    const info = await fetch(`${LLM_GATEWAY_URL}/key/info`, { headers: { Authorization: `Bearer ${key}` }, signal: AbortSignal.timeout(5000) })
      .then((r) => (r.ok ? (r.json() as Promise<{ info?: { budget_reset_at?: string } }>) : undefined))
      .catch(() => undefined);
    if (info?.info?.budget_reset_at) reset = new Date(info.info.budget_reset_at);
  }
  const now = new Date();
  reset ??= new Date(now.getFullYear(), now.getMonth() + 1, 1);
  return new Intl.DateTimeFormat(`${locale}-CH`, { day: "numeric", month: "long", year: "numeric" }).format(reset);
}

/** Forwards one message to the member's assistant; returns the text to send back. */
async function ask(link: Link, input: string, t: (typeof TEXT)[Lang], locale: Lang): Promise<string> {
  const { member } = link;
  const tenant = member.tenant;
  const booked = tenant.services.some((s) => s.kind === "ASSISTANT" && s.enabled);
  const apiKey = tenant.status === "ACTIVE" && booked ? await secret(tenant.id, assistantKeyName(member.id)) : undefined;
  if (!apiKey) return t.unavailable;

  const base = assistantApi({ ...platform, slug: tenant.slug, memberId: member.id });
  const hermes = (p: string, body: object) =>
    fetch(`${base}${p}`, {
      method: "POST",
      headers: { Authorization: `Bearer ${apiKey}`, "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(10 * 60_000),
    });

  let sessionId = link.sessionId;
  for (let attempt = 0; attempt < 2; attempt++) {
    if (!sessionId) {
      const res = await hermes("/sessions", {});
      if (!res.ok) return t.unavailable;
      const created = (await res.json()) as { id?: string; session?: { id: string } };
      sessionId = created.id ?? created.session?.id ?? null;
      if (!sessionId) return t.unavailable;
      await prisma.telegramLink.update({ where: { id: link.id }, data: { sessionId } });
    }
    const res = await hermes(`/sessions/${sessionId}/chat/stream`, { input });
    if (res.status === 404) {
      sessionId = null; // conversation deleted in the portal: start a new one
      continue;
    }
    if (!res.ok || !res.body) return t.unavailable;
    for await (const event of chatEvents(res.body)) {
      if (event.type === "done") return event.text.trim() || t.failed;
      if (event.type === "error") return event.code === "credit" ? t.credit.replace("{date}", await resetDate(tenant.id, locale)) : t.failed;
    }
  }
  return t.failed;
}

async function handle(msg: TgMessage) {
  const from = msg.from;
  if (msg.chat.type !== "private" || !from) return;
  const text = msg.text?.trim() ?? "";
  const [command, arg] = text.startsWith("/") ? [text.split(/\s+/)[0].split("@")[0].toLowerCase(), text.split(/\s+/).slice(1).join("")] : [null, ""];

  const link = await linkFor(from.id);
  if (!link) {
    const t = TEXT[lang(from.language_code)];
    const code = command === "/start" ? arg : text;
    if (/^[A-Za-z0-9 -]{8,11}$/.test(code)) return pair(msg, from, code);
    return send(msg.chat.id, t.welcome);
  }

  const locale = lang(link.member.user.locale, link.member.tenant.locale);
  const t = TEXT[locale];
  if (command === "/start" && arg) return pair(msg, from, arg); // re-pairing, possibly to another member
  if (command === "/new" || command === "/start") {
    await prisma.telegramLink.update({ where: { id: link.id }, data: { sessionId: null } });
    return send(msg.chat.id, t.newChat);
  }
  if (command === "/disconnect" || command === "/stop") {
    await prisma.telegramLink.delete({ where: { id: link.id } });
    await audit(link.member.userId, "assistant.telegram_disconnect", link.member.tenantId);
    return send(msg.chat.id, t.disconnected);
  }
  if (!text) return send(msg.chat.id, t.textOnly);
  if (text.length > MAX_INPUT) return send(msg.chat.id, t.failed);

  // "typing…" lasts 5 seconds; renew it until the answer is there.
  const typing = () => tg("sendChatAction", { chat_id: msg.chat.id, action: "typing" }).catch(() => {});
  void typing();
  const timer = setInterval(typing, 4500);
  try {
    await send(msg.chat.id, await ask(link, text, t, locale).catch(() => t.unavailable));
  } finally {
    clearInterval(timer);
  }
}

// Messages of one user are answered in order; different users in parallel.
const queues = new Map<number, Promise<void>>();
function enqueue(msg: TgMessage) {
  const key = msg.from?.id ?? msg.chat.id;
  const next = (queues.get(key) ?? Promise.resolve())
    .then(() => handle(msg))
    .catch((err) => console.error("telegram: message failed", (err as Error).message))
    .finally(() => {
      if (queues.get(key) === next) queues.delete(key);
    });
  queues.set(key, next);
}

let offset = 0;
let stopping = false;
for (const signal of ["SIGINT", "SIGTERM"] as const) process.on(signal, () => (stopping = true));

const me = await tg<{ username: string }>("getMe", {});
console.log(`telegram: @${me.username} polling`);
while (!stopping) {
  try {
    const updates = await tg<TgUpdate[]>("getUpdates", { offset, timeout: 50, allowed_updates: ["message"] }, 60_000);
    for (const u of updates) {
      offset = u.update_id + 1;
      if (u.message) enqueue(u.message);
    }
  } catch (err) {
    console.error("telegram: polling failed", (err as Error).message);
    await new Promise((r) => setTimeout(r, 5000));
  }
}
await Promise.allSettled(queues.values());
await prisma.$disconnect();

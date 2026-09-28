"use server";

import { PAIRING_MINUTES, audit, newPairingCode, pairingCodeHash } from "@pc/core";
import { prisma } from "@pc/db";
import { memberAssistant } from "@/lib/assistant";
import { TELEGRAM_BOT } from "@/lib/telegram";

/**
 * Starts (or restarts) Telegram pairing for the signed-in member: any earlier
 * link is replaced, so a lost phone is unlinked by pairing again.
 */
export async function startTelegramPairing(): Promise<{ code: string; link: string } | null> {
  const assistant = await memberAssistant();
  if (!assistant || !TELEGRAM_BOT) return null;
  const code = newPairingCode();
  const pending = {
    pairingCodeHash: pairingCodeHash(code),
    pairingExpiresAt: new Date(Date.now() + PAIRING_MINUTES * 60_000),
    telegramUserId: null,
    telegramUsername: null,
    sessionId: null,
    linkedAt: null,
  };
  await prisma.telegramLink.upsert({
    where: { memberId: assistant.member.id },
    create: { memberId: assistant.member.id, ...pending },
    update: pending,
  });
  await audit(assistant.member.userId, "assistant.telegram_pairing", assistant.tenant.id);
  return { code, link: `https://t.me/${TELEGRAM_BOT}?start=${code}` };
}

export async function disconnectTelegram() {
  const assistant = await memberAssistant();
  if (!assistant) return;
  await prisma.telegramLink.deleteMany({ where: { memberId: assistant.member.id } });
  await audit(assistant.member.userId, "assistant.telegram_disconnect", assistant.tenant.id);
}

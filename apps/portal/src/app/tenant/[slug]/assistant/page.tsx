import { notFound } from "next/navigation";
import { prisma } from "@pc/db";
import { AI_PROVIDER } from "@/lib/config";
import { nextReset, tenantCredit } from "@/lib/assistant";
import { requireTenantMember } from "@/lib/tenant";
import { TELEGRAM_BOT } from "@/lib/telegram";
import { AssistantChat } from "@/components/AssistantChat";
import { TelegramPanel } from "@/components/TelegramPanel";

export const dynamic = "force-dynamic";

export default async function AssistantPage({ params }: { params: Promise<{ slug: string }> }) {
  const { tenant, member, enabledServices, locale, t } = await requireTenantMember((await params).slug);
  if (!enabledServices.includes("assistant")) notFound();
  const c = t.chat;

  const credit = await tenantCredit(tenant.id);
  const resetDate = new Intl.DateTimeFormat(`${locale}-CH`, { day: "numeric", month: "long", year: "numeric" }).format(nextReset(credit));
  const exhaustedText = c.creditExhausted.replace("{date}", resetDate);
  const link = TELEGRAM_BOT ? await prisma.telegramLink.findUnique({ where: { memberId: member.id } }) : null;

  return (
    <AssistantChat
      labels={{ ...c, creditExhausted: exhaustedText }}
      creditExhausted={credit?.state === "exhausted"}
      side={
        <>
          {credit && (
            <div className="chat-credit">
              <div className="credit-bar" data-state={credit.state}>
                <span style={{ width: `${Math.round(credit.used * 100)}%` }} />
              </div>
              <p className="small">
                {c.creditUsed.replace("{percent}", `${Math.round(credit.used * 100)}%`).replace("{chf}", String(tenant.aiCreditChf))}
              </p>
              {credit.state === "low" && <p className="small warn">{c.creditLow}</p>}
            </div>
          )}
          {TELEGRAM_BOT && (
            <TelegramPanel
              labels={c.telegram}
              linkedAs={link?.linkedAt ? (link.telegramUsername ? `@${link.telegramUsername}` : "Telegram") : null}
            />
          )}
          <p className="small muted">{c.privacy[AI_PROVIDER]}</p>
        </>
      }
    />
  );
}

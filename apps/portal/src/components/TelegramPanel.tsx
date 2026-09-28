"use client";

import { useState, useTransition } from "react";
import { useRouter } from "next/navigation";
import type { Dictionary } from "@/i18n";
import { disconnectTelegram, startTelegramPairing } from "@/app/tenant/[slug]/assistant/actions";

/**
 * Opt-in Telegram access: a warning first (Telegram is not end-to-end
 * encrypted), then a one-time code the member sends to the Castor bot.
 */
export function TelegramPanel({ labels, linkedAs }: { labels: Dictionary["chat"]["telegram"]; linkedAs: string | null }) {
  const router = useRouter();
  const [step, setStep] = useState<"idle" | "warn" | "code">("idle");
  const [pairing, setPairing] = useState<{ code: string; link: string } | null>(null);
  const [pending, startTransition] = useTransition();

  const pair = () =>
    startTransition(async () => {
      const p = await startTelegramPairing();
      if (p) {
        setPairing(p);
        setStep("code");
      }
    });
  const disconnect = () =>
    startTransition(async () => {
      await disconnectTelegram();
      setStep("idle");
      router.refresh();
    });

  return (
    <details className="telegram">
      <summary>{labels.title}</summary>
      {linkedAs && step !== "code" ? (
        <>
          <p className="small">{labels.connected.replace("{name}", linkedAs)}</p>
          <button className="button secondary small" type="button" onClick={disconnect} disabled={pending}>{labels.disconnect}</button>
        </>
      ) : step === "idle" ? (
        <>
          <p className="small muted">{labels.off}</p>
          <button className="button secondary small" type="button" onClick={() => setStep("warn")}>{labels.title} …</button>
        </>
      ) : step === "warn" ? (
        <>
          <p className="notice small">{labels.warning}</p>
          <div className="row">
            <button className="button small" type="button" onClick={pair} disabled={pending}>{labels.confirm}</button>
            <button className="button secondary small" type="button" onClick={() => setStep("idle")}>{labels.cancel}</button>
          </div>
        </>
      ) : (
        pairing && (
          <>
            <p className="small">{labels.pairing}</p>
            <p className="pairing-code">{pairing.code.slice(0, 4)} {pairing.code.slice(4)}</p>
            <div className="row">
              <a className="button small" href={pairing.link} target="_blank" rel="noopener noreferrer">{labels.openBot}</a>
              <button className="button secondary small" type="button" onClick={() => { setStep("idle"); router.refresh(); }}>{labels.cancel}</button>
            </div>
          </>
        )
      )}
    </details>
  );
}

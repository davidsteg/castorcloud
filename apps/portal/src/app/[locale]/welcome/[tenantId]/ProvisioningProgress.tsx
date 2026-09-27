"use client";

import { useEffect, useState } from "react";
import type { Dictionary } from "@/i18n";

// Must match the step names the provisioner records on the Job row.
const STEPS = ["payment", "identity", "storage", "services", "ready"] as const;

type Status = { status: string; step: string | null; jobStatus: string | null };

export function ProvisioningProgress({
  tenantId, t, cloudUrl, initialStatus,
}: { tenantId: string; t: Dictionary["welcome"]; cloudUrl: string; initialStatus: string }) {
  const [state, setState] = useState<Status>({ status: initialStatus, step: null, jobStatus: null });
  const ready = state.status === "ACTIVE";
  const failed = state.status === "FAILED" || state.jobStatus === "FAILED";

  useEffect(() => {
    if (ready) return;
    const timer = setInterval(async () => {
      const res = await fetch(`/api/tenants/${tenantId}/status`, { cache: "no-store" });
      if (res.ok) setState(await res.json());
    }, 2000);
    return () => clearInterval(timer);
  }, [tenantId, ready]);

  // "payment" is done once the tenant left PENDING; later steps come from the job.
  const doneIndex = ready
    ? STEPS.length
    : state.status === "PENDING"
      ? 0
      : Math.max(1, STEPS.indexOf((state.step ?? "payment") as (typeof STEPS)[number]) + 1);

  return (
    <div className="stack">
      <h1>{ready ? t.readyTitle : t.title}</h1>
      {!ready && <p className="lead">{t.lead}</p>}
      <ul className="steps">
        {STEPS.map((step, i) => (
          <li key={step} className={i < doneIndex ? "done" : i === doneIndex && !failed ? "active" : ""}>
            <span className="dot">{i < doneIndex ? "✓" : ""}</span>
            {t.steps[step]}
          </li>
        ))}
      </ul>
      {failed && <p className="notice">{t.failed}</p>}
      {ready && <a className="button block" href={cloudUrl}>{t.open}</a>}
    </div>
  );
}

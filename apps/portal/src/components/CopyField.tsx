"use client";

import { useState } from "react";

/** Read-only value with a copy button (server addresses on the setup page). */
export function CopyField({ label, value, copyLabel, copiedLabel }: { label: string; value: string; copyLabel: string; copiedLabel: string }) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard blocked: the value stays selectable.
    }
  }

  return (
    <div className="copy-field">
      <span className="copy-label">{label}</span>
      <code>{value}</code>
      <button type="button" className="button secondary small" onClick={copy}>
        {copied ? `✓ ${copiedLabel}` : copyLabel}
      </button>
    </div>
  );
}

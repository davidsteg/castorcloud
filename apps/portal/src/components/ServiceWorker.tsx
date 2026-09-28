"use client";

import { useEffect } from "react";

/** Registers the service worker (offline page for the installed app). */
export function ServiceWorker() {
  useEffect(() => {
    if ("serviceWorker" in navigator) navigator.serviceWorker.register("/sw.js").catch(() => {});
  }, []);
  return null;
}

"use client";

import { useEffect, useRef, useState } from "react";
import { isBridgeEvent } from "@pc/brand/bridge-protocol";
import type { ServiceKey } from "@/lib/config";

// Embeds a tenant service and keeps the shell URL in sync with the iframe,
// using the bridge script the node's skin proxy injects into the service.
export function ServiceFrame({ service, origin, initialPath }: { service: ServiceKey; origin: string; initialPath: string }) {
  const frameRef = useRef<HTMLIFrameElement>(null);
  // Fixed after the first render: changing src would reload the service.
  const [src] = useState(origin + initialPath);

  useEffect(() => {
    function onMessage(event: MessageEvent) {
      if (event.origin !== origin || !isBridgeEvent(event.data)) return;
      const { path, title } = event.data;
      // Update the address bar without re-rendering the iframe.
      window.history.replaceState(null, "", `/${service}${path === "/" ? "" : path}`);
      if (title) document.title = title;

      const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
      frameRef.current?.contentWindow?.postMessage({ type: "pc:theme", mode: dark ? "dark" : "light" }, origin);
    }
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [origin, service]);

  return (
    <iframe
      ref={frameRef}
      className="service-frame"
      src={src}
      title={service}
      allow="fullscreen; clipboard-write; geolocation"
    />
  );
}

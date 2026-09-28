"use client";

import { useEffect, useRef, useState } from "react";
import { isBridgeEvent } from "@pc/brand/bridge-protocol";
import type { ServiceKey } from "@/lib/config";

// Embeds a tenant service and keeps the shell URL in sync with the iframe,
// using the bridge script the node's skin proxy injects into the service.
//
// Every service page carries the bridge, which answers pc:ping. A page that
// doesn't answer is not the service: in practice Authentik's login page,
// which refuses to be framed (the user's Authentik session ended while the
// portal's is still valid). Then we offer to sign in again, top-level.
export function ServiceFrame({ service, origin, initialPath, expired }: {
  service: ServiceKey;
  origin: string;
  initialPath: string;
  expired: { title: string; body: string; button: string; signIn: (form: FormData) => Promise<void> };
}) {
  const frameRef = useRef<HTMLIFrameElement>(null);
  // Fixed after the first render: changing src would reload the service.
  const [src] = useState(origin + initialPath);
  const [stuck, setStuck] = useState(false);
  const answered = useRef(false);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);

  function ping(waitMs: number) {
    clearTimeout(timer.current);
    answered.current = false;
    // Dropped by the browser unless the frame shows the service's origin.
    frameRef.current?.contentWindow?.postMessage({ type: "pc:ping" }, origin);
    timer.current = setTimeout(() => setStuck(!answered.current), waitMs);
  }

  useEffect(() => {
    function onMessage(event: MessageEvent) {
      if (event.origin !== origin || !isBridgeEvent(event.data)) return;
      answered.current = true;
      setStuck(false);
      const { path, title } = event.data;
      // Update the address bar without re-rendering the iframe.
      window.history.replaceState(null, "", `/${service}${path === "/" ? "" : path}`);
      if (title) document.title = title;

      const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
      frameRef.current?.contentWindow?.postMessage({ type: "pc:theme", mode: dark ? "dark" : "light" }, origin);
    }
    window.addEventListener("message", onMessage);
    // The frame may have finished loading before this component hydrated.
    ping(8000);
    return () => {
      window.removeEventListener("message", onMessage);
      clearTimeout(timer.current);
    };
  }, [origin, service]);

  return (
    <div className="frame-wrap">
      <iframe
        ref={frameRef}
        className="service-frame"
        src={src}
        title={service}
        allow="fullscreen; clipboard-write; geolocation"
        onLoad={() => ping(3000)}
      />
      {stuck && (
        <div className="frame-expired" role="alert">
          <form className="panel" action={expired.signIn}>
            <h2>{expired.title}</h2>
            <p>{expired.body}</p>
            <input type="hidden" name="path" value={window.location.pathname} />
            <button className="button" type="submit">{expired.button}</button>
          </form>
        </div>
      )}
    </div>
  );
}

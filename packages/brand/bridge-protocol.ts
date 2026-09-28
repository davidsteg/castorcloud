// Messages exchanged between the portal shell (parent window) and the
// bridge script injected into each embedded service (iframe).

export type ServiceKey = "photos" | "docs" | "places" | "flights";

/** iframe -> shell */
export type BridgeEvent =
  | { type: "pc:ready"; service: ServiceKey; path: string; title: string }
  | { type: "pc:navigated"; service: ServiceKey; path: string; title: string };

/** shell -> iframe */
export type ShellCommand =
  | { type: "pc:theme"; mode: "light" | "dark" }
  | { type: "pc:go"; path: string }
  /** answered with pc:ready; tells the shell the frame shows the service */
  | { type: "pc:ping" };

export function isBridgeEvent(data: unknown): data is BridgeEvent {
  return (
    typeof data === "object" &&
    data !== null &&
    "type" in data &&
    (data.type === "pc:ready" || data.type === "pc:navigated")
  );
}

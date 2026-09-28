import { NextResponse, type NextRequest } from "next/server";
import { BASE_DOMAIN, PUBLIC_SCHEME } from "@/lib/config";

// Signing out everywhere. Portal sessions are host-only cookies, one on
// app.<domain> and one on each <slug>.<domain> the user opened, so the browser
// hops through /api/logout on each of those hosts (?hops=app,muster,...), and
// the last hop ends the Authentik session. Its logout flow (portal-logout in
// authentik/blueprints/10-portal.yaml) then returns to the site.
// Started by signOutBrowser() and signOutAllDevices() (lib/signout.ts).

const LABEL_RE = /^[a-z0-9-]{1,63}$/;
const SESSION_COOKIE_RE = /^(__Secure-)?authjs\.session-token(\.\d+)?$/;

export function GET(request: NextRequest) {
  const hops = (request.nextUrl.searchParams.get("hops") ?? "").split(",").filter((l) => LABEL_RE.test(l));
  const next = hops.shift();
  const target = next
    ? `${PUBLIC_SCHEME}://${next}.${BASE_DOMAIN}/api/logout?hops=${hops.join(",")}`
    : `${PUBLIC_SCHEME}://auth.${BASE_DOMAIN}/application/o/portal/end-session/`;

  const response = NextResponse.redirect(target);
  for (const { name } of request.cookies.getAll()) {
    if (!SESSION_COOKIE_RE.test(name)) continue;
    // __Secure- cookies can only be overwritten with the Secure attribute.
    response.cookies.set(name, "", { path: "/", maxAge: 0, httpOnly: true, sameSite: "lax", secure: name.startsWith("__Secure-") });
  }
  return response;
}

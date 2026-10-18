import { NextResponse, type NextRequest } from "next/server";
import { tenantSlugFromHost } from "@/lib/config";
import { DEFAULT_LOCALE, isLocale, negotiateLocale } from "@/i18n";

// Two kinds of hosts:
//   <slug>.<domain>  tenant shell   ->  rewritten to /tenant/<slug>/<path>
//   app.<domain>     public site    ->  /<locale>/<path> (locale from URL,
//                                      else cookie, else Accept-Language)
export function proxy(request: NextRequest) {
  const url = request.nextUrl.clone();
  const slug = tenantSlugFromHost(request.headers.get("host"));

  if (slug) {
    url.pathname = `/tenant/${slug}${url.pathname === "/" ? "" : url.pathname}`;
    const response = NextResponse.rewrite(url, { request: { headers: withLocale(request, DEFAULT_LOCALE) } });
    // A customer's cloud is private: keep it out of search engines.
    response.headers.set("X-Robots-Tag", "noindex, nofollow");
    return response;
  }

  const first = url.pathname.split("/")[1];
  if (isLocale(first)) {
    const response = NextResponse.next({ request: { headers: withLocale(request, first) } });
    response.cookies.set("pc-locale", first, { path: "/", maxAge: 60 * 60 * 24 * 365, sameSite: "lax" });
    return response;
  }

  const cookie = request.cookies.get("pc-locale")?.value;
  const locale = isLocale(cookie) ? cookie : negotiateLocale(request.headers.get("accept-language"));
  url.pathname = `/${locale}${url.pathname === "/" ? "" : url.pathname}`;
  return NextResponse.redirect(url);
}

/** Lets the root layout set <html lang>. */
function withLocale(request: NextRequest, locale: string) {
  const headers = new Headers(request.headers);
  headers.set("x-pc-locale", locale);
  return headers;
}

export const config = {
  // Skip API routes, Next internals and static files (anything with a dot).
  matcher: ["/((?!api/|_next/|.*\\..*).*)"],
};

import type { MetadataRoute } from "next";
import { headers } from "next/headers";
import { appUrl, tenantSlugFromHost } from "@/lib/config";

// Same file on every host the portal serves: tenant clouds are closed to
// crawlers, the public site is open except its API. Private pages (account,
// admin, signup) carry noindex instead of being listed here, so this file
// doesn't advertise them.
export default async function robots(): Promise<MetadataRoute.Robots> {
  if (tenantSlugFromHost((await headers()).get("host"))) {
    return { rules: { userAgent: "*", disallow: "/" } };
  }
  return {
    rules: { userAgent: "*", allow: "/", disallow: "/api/" },
    sitemap: appUrl("/sitemap.xml"),
  };
}

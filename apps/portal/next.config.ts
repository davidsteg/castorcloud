import type { NextConfig } from "next";
import { config } from "dotenv";
import path from "node:path";

// Single .env at the monorepo root, shared with docker compose.
config({ path: path.resolve(process.cwd(), "../../.env") });

// On every portal page (app.<domain> and the tenant shells). Nothing frames
// the portal; the shell frames the services, which set their own
// frame-ancestors (stack-templates/tenant/skin); geolocation stays allowed so
// the shell can delegate it to them. Browsers ignore HSTS over plain HTTP
// (development), and next.config is read at build time, so it is always sent.
const securityHeaders = [
  { key: "Content-Security-Policy", value: "frame-ancestors 'none'; base-uri 'self'; object-src 'none'" },
  { key: "X-Frame-Options", value: "DENY" },
  { key: "X-Content-Type-Options", value: "nosniff" },
  { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
  { key: "Permissions-Policy", value: "camera=(), microphone=(), payment=(), usb=()" },
  { key: "Strict-Transport-Security", value: "max-age=31536000" },
];

const nextConfig: NextConfig = {
  transpilePackages: ["@pc/brand", "@pc/db", "@pc/pricing", "@pc/core", "@pc/stacks"],
  env: { NEXT_PUBLIC_BASE_DOMAIN: process.env.BASE_DOMAIN },
  // Worker-only packages must not be bundled.
  serverExternalPackages: ["nodemailer"],
  poweredByHeader: false,
  async headers() {
    return [{ source: "/:path*", headers: securityHeaders }];
  },
};

export default nextConfig;

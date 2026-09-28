import type { NextConfig } from "next";
import { config } from "dotenv";
import path from "node:path";

// Single .env at the monorepo root, shared with docker compose.
config({ path: path.resolve(process.cwd(), "../../.env") });

const nextConfig: NextConfig = {
  transpilePackages: ["@pc/brand", "@pc/db", "@pc/pricing", "@pc/core"],
  env: { NEXT_PUBLIC_BASE_DOMAIN: process.env.BASE_DOMAIN },
  // Worker-only packages must not be bundled.
  serverExternalPackages: ["bullmq"],
};

export default nextConfig;

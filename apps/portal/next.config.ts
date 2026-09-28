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
  // Container builds (apps/portal/Dockerfile) ship a self-contained server.
  ...(process.env.PC_STANDALONE
    ? { output: "standalone" as const, outputFileTracingRoot: path.resolve(process.cwd(), "../..") }
    : {}),
};

export default nextConfig;

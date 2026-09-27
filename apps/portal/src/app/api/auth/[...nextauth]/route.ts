import { NextRequest } from "next/server";
import { handlers } from "@/auth";

// Behind Traefik, Next.js reports the internal URL (localhost:3000). Auth.js
// derives callback URLs from it, so rebuild it from the forwarded headers:
// the portal serves app.<domain> and every <slug>.<domain>.
function withPublicUrl(handler: (req: NextRequest) => Promise<Response>) {
  return (req: NextRequest) => {
    const host = req.headers.get("x-forwarded-host") ?? req.headers.get("host");
    const proto = req.headers.get("x-forwarded-proto") ?? "https";
    if (!host) return handler(req);
    const url = new URL(req.nextUrl.pathname + req.nextUrl.search, `${proto}://${host}`);
    return handler(new NextRequest(url, req));
  };
}

export const GET = withPublicUrl(handlers.GET);
export const POST = withPublicUrl(handlers.POST);

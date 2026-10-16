// The GitHub App's "Setup URL": GitHub sends the customer here after they
// installed the app or changed its repositories. The installation_id GitHub
// appends is not trusted; linking starts over with a GitHub sign-in, which
// proves whose installation it is (api/github/callback).
import { NextResponse, type NextRequest } from "next/server";
import { DEFAULT_LOCALE, isLocale } from "@/i18n";
import { appUrl } from "@/lib/config";
import { startGitHubConnect } from "@/lib/github";
import { currentUser } from "@/lib/session";

export const runtime = "nodejs";

export async function GET(request: NextRequest) {
  const cookieLocale = request.cookies.get("pc-locale")?.value;
  const locale = isLocale(cookieLocale) ? cookieLocale : DEFAULT_LOCALE;
  const user = await currentUser();
  const url = user ? await startGitHubConnect(user.id, locale) : null;
  return NextResponse.redirect(url ?? appUrl(`/${locale}/account#github`));
}

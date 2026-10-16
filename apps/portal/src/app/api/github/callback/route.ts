// End of "Connect GitHub" (docs/github.md): GitHub returns here after the
// customer signed in. Only installations on their own GitHub account are
// linked; with none yet, they continue to GitHub to install the app.
import { NextResponse, type NextRequest } from "next/server";
import { prisma } from "@pc/db";
import { audit, githubAppConfig, installUrl, verifiedOwnInstallations } from "@pc/core";
import { DEFAULT_LOCALE } from "@/i18n";
import { GITHUB_CALLBACK_URL, accountUrl, takeGitHubConnect } from "@/lib/github";
import { currentUser } from "@/lib/session";

export const runtime = "nodejs";

export async function GET(request: NextRequest) {
  const params = request.nextUrl.searchParams;
  const user = await currentUser();
  const pending = await takeGitHubConnect(params.get("state"), user?.id);
  const cfg = githubAppConfig();
  const code = params.get("code");
  if (!user || !pending || !cfg || !code) return NextResponse.redirect(accountUrl(pending?.locale ?? DEFAULT_LOCALE, "error"));

  let result;
  try {
    result = await verifiedOwnInstallations(code, pending.verifier, GITHUB_CALLBACK_URL);
  } catch (err) {
    console.error("github connect failed", err);
    return NextResponse.redirect(accountUrl(pending.locale, "error"));
  }
  const { githubUser, installations } = result;
  if (installations.length === 0) return NextResponse.redirect(installUrl(cfg));

  for (const i of installations) {
    const data = {
      userId: user.id,
      accountId: i.accountId,
      accountLogin: i.accountLogin,
      repositorySelection: i.repositorySelection,
      suspendedAt: i.suspended ? new Date() : null,
    };
    await prisma.gitHubInstallation.upsert({ where: { installationId: i.installationId }, update: data, create: { ...data, installationId: i.installationId } });
  }
  // Installations of this GitHub account that are gone on GitHub.
  await prisma.gitHubInstallation.deleteMany({
    where: { accountId: githubUser.id, installationId: { notIn: installations.map((i) => i.installationId) } },
  });
  await audit(user.id, "github.connected", undefined, { githubUserId: String(githubUser.id), login: githubUser.login });
  return NextResponse.redirect(accountUrl(pending.locale, "connected"));
}

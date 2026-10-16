// GitHub App webhook (docs/github.md): keeps linked installations in step
// when the customer uninstalls, suspends or renames on GitHub. Signed with
// GITHUB_APP_WEBHOOK_SECRET; anything unsigned is refused.
import { NextResponse, type NextRequest } from "next/server";
import { prisma } from "@pc/db";
import { audit, githubAppConfig, verifyWebhookSignature } from "@pc/core";

export const runtime = "nodejs";

type InstallationEvent = {
  action?: string;
  installation?: { id?: number; repository_selection?: string };
  /** installation_target: the renamed account */
  account?: { login?: string };
};

export async function POST(request: NextRequest) {
  const cfg = githubAppConfig();
  if (!cfg) return NextResponse.json({ error: "github not configured" }, { status: 503 });
  const body = await request.text();
  if (!verifyWebhookSignature(body, request.headers.get("x-hub-signature-256"), cfg.webhookSecret)) {
    return NextResponse.json({ error: "invalid signature" }, { status: 401 });
  }

  const event = request.headers.get("x-github-event");
  let payload: InstallationEvent;
  try {
    payload = JSON.parse(body);
  } catch {
    return NextResponse.json({ error: "invalid body" }, { status: 400 });
  }
  const id = payload.installation?.id;
  if (typeof id !== "number") return NextResponse.json({ ignored: true });
  const installationId = BigInt(id);

  if (event === "installation" && payload.action === "deleted") {
    const gone = await prisma.gitHubInstallation.findUnique({ where: { installationId } });
    if (gone) {
      await prisma.gitHubInstallation.delete({ where: { installationId } });
      await audit(gone.userId, "github.uninstalled", undefined, { installationId: String(id) });
    }
  } else if (event === "installation" && (payload.action === "suspend" || payload.action === "unsuspend")) {
    await prisma.gitHubInstallation.updateMany({
      where: { installationId },
      data: { suspendedAt: payload.action === "suspend" ? new Date() : null },
    });
  } else if (event === "installation_repositories") {
    const selection = payload.installation?.repository_selection === "all" ? "all" : "selected";
    await prisma.gitHubInstallation.updateMany({ where: { installationId }, data: { repositorySelection: selection } });
  } else if (event === "installation_target" && payload.action === "renamed" && payload.account?.login) {
    await prisma.gitHubInstallation.updateMany({ where: { installationId }, data: { accountLogin: payload.account.login } });
  }
  return NextResponse.json({ received: true });
}

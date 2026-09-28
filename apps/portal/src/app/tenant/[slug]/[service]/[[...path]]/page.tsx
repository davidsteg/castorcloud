import { notFound } from "next/navigation";
import { isServiceKey, serviceOrigin } from "@/lib/config";
import { ServiceFrame } from "@/components/ServiceFrame";
import { requireTenantMember } from "@/lib/tenant";
import { signIn } from "@/auth";

export default async function ServicePage({
  params,
}: {
  params: Promise<{ slug: string; service: string; path?: string[] }>;
}) {
  const { slug, service, path = [] } = await params;
  if (!isServiceKey(service)) notFound();
  const { enabledServices, t } = await requireTenantMember(slug);
  if (!enabledServices.includes(service)) notFound();

  // Signs in again top-level (Authentik can't be shown in the frame), then
  // comes back to the page the user was on.
  async function signInAgain(form: FormData) {
    "use server";
    const path = String(form.get("path") ?? "/");
    await signIn("authentik", { redirectTo: path.startsWith("/") && !path.startsWith("//") ? path : "/" });
  }

  return (
    <ServiceFrame
      service={service}
      origin={serviceOrigin(slug, service)}
      initialPath={"/" + path.map(encodeURIComponent).join("/")}
      expired={{ title: t.setup.frameExpiredTitle, body: t.setup.frameExpiredBody, button: t.setup.signInAgain, signIn: signInAgain }}
    />
  );
}

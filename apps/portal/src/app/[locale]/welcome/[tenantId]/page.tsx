import { notFound } from "next/navigation";
import { getDictionary, type Locale } from "@/i18n";
import { BASE_DOMAIN, PUBLIC_SCHEME } from "@/lib/config";
import { memberTenant } from "@/lib/session";
import { SiteHeader } from "@/components/site/SiteHeader";
import { ProvisioningProgress } from "./ProvisioningProgress";

export default async function WelcomePage({ params }: { params: Promise<{ locale: Locale; tenantId: string }> }) {
  const { locale, tenantId } = await params;
  const tenant = await memberTenant(tenantId);
  if (!tenant) notFound();
  const t = getDictionary(locale);

  return (
    <>
      <SiteHeader locale={locale} t={t} />
      <main className="flow">
        <ProvisioningProgress
          tenantId={tenant.id}
          t={t.welcome}
          cloudUrl={`${PUBLIC_SCHEME}://${tenant.slug}.${BASE_DOMAIN}`}
          initialStatus={tenant.status}
        />
      </main>
    </>
  );
}

import { BRAND_NAME, SERVICES } from "@/lib/config";
import { requireTenantMember } from "@/lib/tenant";
import { ServiceWorker } from "@/components/ServiceWorker";
import { ShellNav, type ShellNavItem } from "@/components/ShellNav";
import { signOutBrowser } from "@/lib/signout";
import { stacksEnabled } from "@/lib/stacks";

export default async function TenantLayout({
  children,
  params,
}: {
  children: React.ReactNode;
  params: Promise<{ slug: string }>;
}) {
  const { slug } = await params;
  const { tenant, session, enabledServices, t } = await requireTenantMember(slug);

  const items: ShellNavItem[] = [
    { href: "/", label: t.setup.home, icon: "⌂" },
    ...Object.entries(SERVICES)
      .filter(([key]) => enabledServices.includes(key))
      .map(([key, s]) => ({ href: `/${key}`, label: t.services.items[key as keyof typeof SERVICES].name, icon: s.icon })),
    ...(enabledServices.includes("assistant") ? [{ href: "/assistant", label: t.chat.title, icon: "✦" }] : []),
    ...(stacksEnabled() ? [{ href: "/stacks", label: t.stacks.nav, icon: "◇" }] : []),
    { href: "/apps", label: t.setup.nav, icon: "▯" },
  ];

  return (
    <div className="shell">
      <ServiceWorker />
      <ShellNav brand={BRAND_NAME} tenantName={tenant.displayName} email={session.user?.email ?? ""} items={items} signOut={{ label: t.account.logout, action: signOutBrowser }} />
      <main className="shell-main">
        {tenant.status === "ACTIVE" ? children : <div className="page"><p className="notice">{tenant.status}</p></div>}
      </main>
    </div>
  );
}

import Link from "next/link";
import { redirect } from "next/navigation";
import { prisma } from "@pc/db";
import { MAX_STORAGE_UNITS, fitsStorage, formatStorage, minStorageUnits, storageQuotaMb } from "@pc/pricing";
import { signIn, signOut } from "@/auth";
import { fill, getDictionary, type Locale } from "@/i18n";
import { APP_LABEL, BASE_DOMAIN, PUBLIC_SCHEME } from "@/lib/config";
import { currentUser, memberTenant, ownedTenant } from "@/lib/session";
import { itemKeyFromLookup, stripe } from "@/lib/stripe";
import { SiteHeader } from "@/components/site/SiteHeader";
import { StorageMeter } from "@/components/StorageMeter";

export default async function AccountPage({ params }: { params: Promise<{ locale: Locale }> }) {
  const { locale } = await params;
  const t = getDictionary(locale);
  const user = await currentUser();

  if (!user) {
    async function login() {
      "use server";
      await signIn("authentik", { redirectTo: `/${locale}/account` });
    }
    return (
      <>
        <SiteHeader locale={locale} t={t} path="/account" />
        <main className="flow">
          <h1>{t.nav.login}</h1>
          <p className="lead">{t.signup.signInFirst}</p>
          <form action={login}>
            <button className="button block" type="submit">{t.signup.signIn}</button>
          </form>
        </main>
      </>
    );
  }

  const tenants = await prisma.tenant.findMany({
    where: { members: { some: { userId: user.id } }, status: { not: "DELETED" } },
    include: { members: { where: { userId: user.id }, select: { role: true } } },
    orderBy: { createdAt: "asc" },
  });

  async function billing(form: FormData) {
    "use server";
    const tenant = await memberTenant(String(form.get("tenantId")));
    const s = stripe();
    if (!tenant?.stripeCustomerId || !s) return;
    const portal = await s.billingPortal.sessions.create({
      customer: tenant.stripeCustomerId,
      locale,
      return_url: `${PUBLIC_SCHEME}://${APP_LABEL}.${BASE_DOMAIN}/${locale}/account`,
    });
    redirect(portal.url);
  }

  // Storage changes go through here rather than the Stripe portal, so a
  // downgrade is checked against what is stored. The subscription webhook then
  // updates the tenant and the provisioner resizes the volume.
  async function changeStorage(form: FormData) {
    "use server";
    const tenant = await ownedTenant(String(form.get("tenantId")));
    const units = Math.round(Number(form.get("storageUnits")));
    const s = stripe();
    if (!tenant?.stripeSubscriptionId || !s) return;
    if (!Number.isFinite(units) || units < 1 || units > MAX_STORAGE_UNITS || units === tenant.storageUnits) return;
    if (tenant.storageUsedMb !== null && !fitsStorage(tenant.storageUsedMb, units)) return;

    const sub = await s.subscriptions.retrieve(tenant.stripeSubscriptionId);
    const item = sub.items.data.find((i) => itemKeyFromLookup(i.price.lookup_key) === "storage");
    if (!item) return;
    await s.subscriptionItems.update(item.id, { quantity: units, proration_behavior: "create_prorations" });
    redirect(`/${locale}/account`);
  }

  async function logout() {
    "use server";
    await signOut({ redirectTo: `/${locale}` });
  }

  return (
    <>
      <SiteHeader locale={locale} t={t} path="/account" />
      <main className="flow">
        <h1>{t.account.title}</h1>
        <p className="lead">{user.email}</p>
        <div className="stack">
          {tenants.length === 0 && <p className="notice">{t.account.empty}</p>}
          {tenants.map((tenant) => (
            <div className="panel" key={tenant.id}>
              <h3 style={{ fontSize: 18, marginBottom: 4 }}>{tenant.displayName}</h3>
              <p className="small-print" style={{ marginBottom: 16 }}>
                {tenant.slug}.{BASE_DOMAIN} · {t.account.status[tenant.status]}
              </p>
              <div style={{ display: "flex", gap: 10, flexWrap: "wrap" }}>
                {tenant.status === "ACTIVE" && (
                  <a className="button small" href={`${PUBLIC_SCHEME}://${tenant.slug}.${BASE_DOMAIN}`}>{t.account.open}</a>
                )}
                {tenant.status === "PROVISIONING" && (
                  <Link className="button small" href={`/${locale}/welcome/${tenant.id}`}>{t.welcome.title}</Link>
                )}
                {tenant.stripeCustomerId && (
                  <form action={billing}>
                    <input type="hidden" name="tenantId" value={tenant.id} />
                    <button className="button secondary small" type="submit">{t.account.billing}</button>
                  </form>
                )}
              </div>
              {tenant.status === "ACTIVE" && (
                <div style={{ marginTop: 20 }}>
                  <StorageMeter
                    tenant={tenant}
                    t={t}
                    action={
                      tenant.stripeSubscriptionId && tenant.members[0]?.role === "OWNER" ? (
                        <form action={changeStorage} className="storage-change">
                          <input type="hidden" name="tenantId" value={tenant.id} />
                          <label htmlFor={`storage-${tenant.id}`}>{t.storage.change}</label>
                          <select className="input" id={`storage-${tenant.id}`} name="storageUnits" defaultValue={tenant.storageUnits}>
                            {storageOptions(tenant.storageUsedMb).map((u) => (
                              <option key={u} value={u}>{formatStorage(storageQuotaMb(u))}</option>
                            ))}
                          </select>
                          <button className="button secondary small" type="submit">{t.storage.changeSubmit}</button>
                          {tenant.storageUsedMb !== null && (
                            <span className="small-print">
                              {fill(t.storage.changeHint, { min: formatStorage(storageQuotaMb(minStorageUnits(tenant.storageUsedMb))) })}
                            </span>
                          )}
                        </form>
                      ) : null
                    }
                  />
                </div>
              )}
            </div>
          ))}
          <Link className="button secondary block" href={`/${locale}/pricing`}>{t.account.create}</Link>
          <form action={logout}>
            <button className="button secondary block" type="submit">{t.account.logout}</button>
          </form>
        </div>
      </main>
    </>
  );
}

/** Bookable sizes: from the smallest that holds the current data up to the maximum. */
function storageOptions(usedMb: number | null) {
  const min = usedMb === null ? 1 : minStorageUnits(usedMb);
  return Array.from({ length: MAX_STORAGE_UNITS - min + 1 }, (_, i) => min + i);
}

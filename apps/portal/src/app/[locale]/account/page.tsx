import Link from "next/link";
import { redirect } from "next/navigation";
import { prisma } from "@pc/db";
import { normalizeEmail } from "@pc/core";
import { MAX_STORAGE_UNITS, fitsStorage, formatStorage, minStorageUnits, storageQuotaMb } from "@pc/pricing";
import { signIn } from "@/auth";
import { fill, getDictionary, type Locale } from "@/i18n";
import { APP_LABEL, BASE_DOMAIN, PUBLIC_SCHEME } from "@/lib/config";
import { withdrawApplication } from "@/lib/applications";
import { checkoutUrl } from "@/lib/checkout";
import { subscribe, unsubscribe } from "@/lib/newsletter";
import { currentUser, memberTenant, ownedTenant } from "@/lib/session";
import { signOutAllDevices, signOutBrowser } from "@/lib/signout";
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

  const email = normalizeEmail(user.email);
  const newsletter = email ? await prisma.newsletterSubscriber.findUnique({ where: { email } }) : null;

  // An approved application (or, outside the beta, an interrupted signup) waits for payment.
  async function pay(form: FormData) {
    "use server";
    const me = await currentUser();
    if (!me) return;
    const url = await checkoutUrl(String(form.get("tenantId")), me, locale, `/${locale}/account`);
    if (url) redirect(url);
  }

  async function withdraw(form: FormData) {
    "use server";
    const me = await currentUser();
    if (me) await withdrawApplication(me, String(form.get("tenantId")));
    redirect(`/${locale}/account`);
  }

  // Always the signed-in user's own address: the form carries no e-mail.
  async function newsletterSubscribe() {
    "use server";
    const me = await currentUser();
    const address = normalizeEmail(me?.email);
    if (address) await subscribe(address, locale, "account");
    redirect(`/${locale}/account`);
  }

  async function newsletterUnsubscribe() {
    "use server";
    const me = await currentUser();
    const address = normalizeEmail(me?.email);
    const sub = address ? await prisma.newsletterSubscriber.findUnique({ where: { email: address } }) : null;
    if (sub) await unsubscribe(sub.id);
    redirect(`/${locale}/account`);
  }

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
              {tenant.status === "APPLIED" && <p className="small-print" style={{ marginBottom: 16 }}>{t.apply.reviewing}</p>}
              {tenant.status === "PENDING" && tenant.reviewedAt && <p className="small-print" style={{ marginBottom: 16 }}>{t.apply.approved}</p>}
              <div style={{ display: "flex", gap: 10, flexWrap: "wrap" }}>
                {tenant.status === "PENDING" && tenant.members[0]?.role === "OWNER" && (
                  <form action={pay}>
                    <input type="hidden" name="tenantId" value={tenant.id} />
                    <button className="button small" type="submit">{t.apply.pay}</button>
                  </form>
                )}
                {tenant.status === "APPLIED" && tenant.members[0]?.role === "OWNER" && (
                  <form action={withdraw}>
                    <input type="hidden" name="tenantId" value={tenant.id} />
                    <button className="button secondary small" type="submit">{t.apply.withdraw}</button>
                  </form>
                )}
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
          <div className="panel">
            <h3 style={{ fontSize: 18, marginBottom: 4 }}>{t.newsletter.title}</h3>
            <p className="small-print" style={{ marginBottom: 16 }}>
              {newsletter?.status === "CONFIRMED" ? t.newsletter.accountSubscribed
                : newsletter?.status === "PENDING" ? t.newsletter.accountPending
                : t.newsletter.accountNone}
            </p>
            {newsletter?.status === "CONFIRMED" || newsletter?.status === "PENDING" ? (
              <form action={newsletterUnsubscribe}>
                <button className="button secondary small" type="submit">{t.newsletter.unsubscribeButton}</button>
              </form>
            ) : (
              <form action={newsletterSubscribe}>
                <button className="button secondary small" type="submit">{t.newsletter.subscribe}</button>
              </form>
            )}
          </div>
          <form action={signOutBrowser}>
            <button className="button secondary block" type="submit">{t.account.logout}</button>
          </form>
          <form action={signOutAllDevices}>
            <button className="button secondary block" type="submit">{t.account.logoutAll}</button>
            <p className="small-print" style={{ marginTop: 8 }}>{t.account.logoutAllHint}</p>
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

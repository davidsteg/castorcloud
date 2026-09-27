import Link from "next/link";
import type { ServiceKey } from "@pc/pricing";
import type { Dictionary } from "@/i18n";
import { BASE_DOMAIN, PUBLIC_SCHEME, serviceOrigin, type ServiceKey as StackService } from "@/lib/config";
import { qrSvg } from "@/lib/qr";
import { requireTenantMember } from "@/lib/tenant";
import { CopyField } from "@/components/CopyField";
import { ServiceApps } from "@/components/site/MobileApps";

// "Set up apps": how to connect each booked service's phone app to this cloud.
export default async function AppsSetupPage({ params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const { enabledServices, t } = await requireTenantMember(slug);
  const s = t.setup;
  const has = (key: string) => enabledServices.includes(key);
  const portalUrl = `${PUBLIC_SCHEME}://${slug}.${BASE_DOMAIN}`;

  return (
    <div className="page setup">
      <h1>{s.title}</h1>
      <p className="lead muted">{s.lead}</p>

      {has("photos") && (
        <SetupCard t={t} service="photos" address={serviceOrigin(slug, "photos")}>
          <Steps steps={s.photosSteps} />
        </SetupCard>
      )}

      {has("docs") && (
        <SetupCard t={t} service="docs" address={serviceOrigin(slug, "docs")}>
          <h3>{s.docsIosTitle}</h3>
          <Steps steps={s.docsIosSteps} />
          <h3>{s.docsAndroidTitle}</h3>
          <Steps steps={s.docsAndroidSteps} />
        </SetupCard>
      )}

      {has("places") && (
        <SetupCard t={t} service="places" address={serviceOrigin(slug, "places")} showQr={false}>
          <Steps steps={s.placesSteps} />
          {/* The QR code with address + API key is shown by Dawarich itself. */}
          <Link className="button" href="/places/users/edit">{s.placesOpen}</Link>
        </SetupCard>
      )}

      <section className="setup-card">
        <div className="setup-body">
          <h2>{s.webTitle}</h2>
          <p className="muted">{s.webText}</p>
          <ul className="check-list">
            <li>{s.webIos}</li>
            <li>{s.webAndroid}</li>
          </ul>
        </div>
        <Qr text={portalUrl} hint={s.scanHint} />
      </section>

      <p className="notice">{s.security}</p>
    </div>
  );
}

async function SetupCard({
  t, service, address, showQr = true, children,
}: {
  t: Dictionary;
  service: StackService;
  address: string;
  showQr?: boolean;
  children: React.ReactNode;
}) {
  const s = t.setup;
  return (
    <section className="setup-card">
      <div className="setup-body">
        <h2>{t.services.items[service].name}</h2>
        <ServiceApps t={t} service={service as ServiceKey} />
        <CopyField label={s.serverAddress} value={address} copyLabel={s.copy} copiedLabel={s.copied} />
        {children}
      </div>
      {showQr && <Qr text={address} hint={s.scanHint} />}
    </section>
  );
}

async function Qr({ text, hint }: { text: string; hint: string }) {
  const svg = await qrSvg(text);
  return (
    <figure className="setup-qr">
      <div dangerouslySetInnerHTML={{ __html: svg }} />
      <figcaption>{hint}</figcaption>
    </figure>
  );
}

function Steps({ steps }: { steps: readonly string[] }) {
  return (
    <ol className="setup-steps">
      {steps.map((step) => <li key={step}>{step}</li>)}
    </ol>
  );
}

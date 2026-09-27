import Link from "next/link";
import { SERVICES } from "@/lib/config";
import { requireTenantMember } from "@/lib/tenant";

// Dashboard. Live widgets (recent photos, latest documents, today's route,
// next flight) come later; for now: the services and the app setup.
export default async function Dashboard({ params }: { params: Promise<{ slug: string }> }) {
  const { enabledServices, t, tenant } = await requireTenantMember((await params).slug);

  return (
    <div className="page">
      <h1>{t.setup.welcome}</h1>
      <p className="lead muted" style={{ marginBottom: 24 }}>{tenant.displayName}</p>

      <div className="setup-hint">
        <div>
          <b>{t.setup.title}</b>
          <p className="muted">{t.setup.setupHint}</p>
        </div>
        <Link className="button" href="/apps">{t.setup.setupCta}</Link>
      </div>

      <div className="cards">
        {Object.entries(SERVICES).filter(([key]) => enabledServices.includes(key)).map(([key, s]) => (
          <Link className="card" href={`/${key}`} key={key}>
            <div style={{ fontSize: 24 }}>{s.icon}</div>
            <strong>{t.services.items[key as keyof typeof SERVICES].name}</strong>
          </Link>
        ))}
      </div>
    </div>
  );
}

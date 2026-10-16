import { prisma } from "@pc/db";
import { formatChf, formatStorage, stackUnits } from "@pc/pricing";
import { CATALOG } from "@pc/stacks";
import { fill } from "@/i18n";
import { requireTenantMember } from "@/lib/tenant";
import { STACKS_DOMAIN, stackOrigin, stacksEnabled, unitsPriceRp } from "@/lib/stacks";
import { uninstallStack } from "./actions";
import { CatalogInstall, CustomInstall, type StackFormText } from "./StackForms";

const EXAMPLE = `services:
  app:
    image: ghcr.io/example/app:1.2.3
    environment:
      DB_PASSWORD: \${secret:db}
    volumes:
      - data:/data
    mem_limit: 512m
    cpus: 0.5
volumes:
  data:
x-castor:
  web: app:8080
  storage_gb: 10
  egress: [api.example.com]
`;

// "My apps": the tenant's own stacks (docs/stacks.md). Everyone in the
// household sees and opens them; only the owner installs and removes.
export default async function StacksPage({ params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const { tenant, member, t } = await requireTenantMember(slug);
  const s = t.stacks;

  if (!stacksEnabled()) {
    return (
      <div className="page">
        <h1>{s.title}</h1>
        <p className="notice">{s.unavailable}</p>
      </div>
    );
  }

  const stacks = await prisma.stack.findMany({ where: { tenantId: tenant.id }, orderBy: { createdAt: "asc" } });
  const owner = member.role === "OWNER";
  const text: StackFormText = {
    nameLabel: s.nameLabel,
    nameHint: fill(s.nameHint, { example: `name-${slug}.${STACKS_DOMAIN}` }),
    install: s.install,
    customLabel: s.customLabel,
    customSubmit: s.customSubmit,
    problems: s.problems,
    installed: s.installed,
    errors: { ...s.errors },
  };
  const resources = (r: { memoryMb: number; cpus: number; storageGb: number }) =>
    fill(s.resources, { memory: formatStorage(r.memoryMb), cpus: String(r.cpus), storage: r.storageGb ? formatStorage(r.storageGb * 1024) : "–" });
  const price = (r: { memoryMb: number; cpus: number; storageGb: number }) => fill(s.perMonth, { price: formatChf(unitsPriceRp(stackUnits(r))) });

  return (
    <div className="page setup">
      <h1>{s.title}</h1>
      <p className="lead muted">{s.lead}</p>

      <section className="setup-card">
        <div className="setup-body">
          {stacks.length === 0 && <p className="muted">{s.empty}</p>}
          {stacks.map((stack) => (
            <div key={stack.id} className="panel" style={{ marginBottom: 12 }}>
              <h3 style={{ fontSize: 18, marginBottom: 4 }}>{stack.name}</h3>
              <p className="small-print">
                {s.status[stack.status]} · {resources(stack)} · {price(stack)}
              </p>
              {stack.error && stack.status !== "RUNNING" && <p className="small-print"><code>{stack.error.slice(0, 300)}</code></p>}
              <div style={{ display: "flex", gap: 10, flexWrap: "wrap", marginTop: 12 }}>
                {stack.status === "RUNNING" && (
                  // Another site: a new tab (third-party cookies would break an embed).
                  <a className="button small" href={stackOrigin(slug, stack.name)} target="_blank" rel="noopener noreferrer">{s.open}</a>
                )}
                {owner && stack.status !== "REMOVING" && (
                  <form action={uninstallStack} style={{ display: "flex", gap: 10, alignItems: "center", flexWrap: "wrap" }}>
                    <input type="hidden" name="stackId" value={stack.id} />
                    <label className="small-print"><input type="checkbox" name="confirm" required /> {s.removeConfirm}</label>
                    <button className="button secondary small" type="submit">{s.remove}</button>
                  </form>
                )}
              </div>
            </div>
          ))}
        </div>
      </section>

      {owner ? (
        <>
          <section className="setup-card">
            <div className="setup-body">
              <h2>{s.catalogTitle}</h2>
              <p className="muted">{s.catalogLead}</p>
              {CATALOG.map((app) => (
                <div key={app.key} className="panel" style={{ marginBottom: 12 }}>
                  <h3 style={{ fontSize: 18, marginBottom: 4 }}>{app.label}</h3>
                  <p className="small-print">
                    {app.description} <a href={app.homepage} target="_blank" rel="noopener noreferrer">{app.homepage.replace(/^https:\/\//, "")}</a>
                  </p>
                  <CatalogInstall t={text} catalogKey={app.key} defaultName={app.stack} />
                </div>
              ))}
            </div>
          </section>

          <section className="setup-card">
            <div className="setup-body">
              <h2>{s.customTitle}</h2>
              <p className="muted">{s.customLead}</p>
              <ul className="check-list small-print">
                {s.customRules.map((rule) => <li key={rule}>{rule}</li>)}
              </ul>
              <CustomInstall t={text} placeholder={EXAMPLE} />
            </div>
          </section>
          <p className="small-print">{s.billingNote}</p>
        </>
      ) : (
        <p className="notice">{s.ownerOnly}</p>
      )}

      <p className="notice">{s.security}</p>
    </div>
  );
}

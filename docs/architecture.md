# Plan: "Personal Cloud" – a hosted, branded Google-services alternative

## Context
Goal: sell customers a privacy-friendly replacement for Google Photos / Drive-docs / Timeline / flight history + an AI assistant, built on open-source apps:
- **Immich** (photos), **Paperless-ngx** (documents), **Dawarich** (location timeline), **AirTrail** (flights), **Hermes Agent** (AI over the user's own data).

The user must experience **one product**, not five third-party tools. The project folder is empty → greenfield.

Decisions taken (from Q&A):
| Topic | Decision |
|---|---|
| Tenancy | **Dedicated Docker stack per customer/household** |
| UI | **Own portal shell + themed, SSO'd embeds** of each service |
| Hosting | **Docker (Compose) on Linux hosts** (provider-agnostic; EU VPS/dedicated suggested) |
| LLM | **External API** (Hermes agent → hosted model) – phase 2 |
| Stack | **Next.js + TypeScript**, Postgres + Prisma |
| Identity/billing | **Authentik (OIDC SSO) + Stripe** (Stripe later) |
| MVP | **Provisioning + SSO + portal** |
| Mobile | **Upstream apps** (Immich app, OwnTracks/Overland for Dawarich) with QR onboarding |

---

## Architecture

```
                    ┌──────────────── CONTROL PLANE (1 host) ────────────────┐
 customer ──HTTPS──►│ Traefik ─► Portal (Next.js)   Authentik (IdP)          │
                    │            Provisioner worker (BullMQ)  Postgres  Redis │
                    └───────────────┬──────────────────────────────────────────┘
                                    │ SSH / Docker API (provision, update, backup)
                    ┌───────────────▼──── DATA-PLANE NODE(s) ─────────────────┐
                    │ Traefik (wildcard TLS, DNS-01) + "skin" middleware       │
                    │ shared stateless: immich-machine-learning, tika, gotenberg│
                    │ ┌─ stack: cust-<slug> ─────────────────────────────────┐ │
                    │ │ immich-server  paperless  dawarich(+sidekiq) airtrail│ │
                    │ │ postgres (all 4 DBs)  redis  volumes/<slug>/...     │ │
                    │ └──────────────────────────────────────────────────────┘ │
                    └──────────────────────────────────────────────────────────┘
```

### Domains (critical for seamless iframes + mobile apps)
All under one registrable domain (e.g. `brand.cloud`) so embeds are **same-site** (no third-party-cookie problems):
- `brand.cloud` – marketing, signup
- `<slug>.brand.cloud` – the customer's portal (same Next.js app, tenant resolved by hostname)
- `photos-<slug>.brand.cloud`, `docs-<slug>…`, `places-<slug>…`, `flights-<slug>…` – services (flat: slugs may not start with a service prefix)
- `auth.brand.cloud` – Authentik (branded login pages)
- TLS: **one** wildcard certificate `*.brand.cloud` (ACME DNS-01) for all tenants – no per-customer certificates, so Let's Encrypt rate limits never cap signups.

### UI integration ("doesn't look 3rd party")
1. **Portal shell** (Next.js): sidebar (Photos, Documents, Timeline, Flights, Assistant), unified dashboard (widgets via service APIs: recent photos, latest docs, today's route, next flight), global search (fans out to service APIs), account/household/settings, onboarding with QR codes for mobile apps.
2. **Embeds**: each service in a full-height `<iframe>` inside the shell. Routes `/photos/*` ↔ iframe path kept in sync.
3. **Skin middleware** on the data-plane Traefik (nginx `sub_filter` sidecar or Traefik rewrite-body plugin) injects into every HTML response:
   - `/_brand/theme.css` – brand fonts/colors, hides upstream logos/top-nav/duplicate account menus
   - `/_brand/bridge.js` – `postMessage` bridge: reports route/title changes to the shell, receives theme (light/dark) and navigation commands
   - rewrites `X-Frame-Options`/CSP → `frame-ancestors https://<slug>.brand.cloud`
   - native hooks where they exist: Immich custom CSS setting, `PAPERLESS_APP_TITLE/APP_LOGO`
4. Service-specific CSS lives per service in `packages/brand/services/<service>.css` (it will need maintenance on upstream upgrades → pin versions, visual regression tests).

### Identity / SSO
- Central Authentik. Per customer: a **group** `cust-<slug>`; per customer×service: an **OAuth2 provider + application** created by the provisioner via Authentik API, bound to that group only.
- Services: Immich OIDC (auto-register, auto-launch), Paperless (allauth OIDC), AirTrail OIDC, Dawarich OIDC (fallback: Traefik forward-auth + trusted header). Login once at portal → silent SSO inside every iframe.
- Household members = extra users in the customer group (plan-limited).

### Provisioning flow (MVP core)
`signup → Tenant(status=pending) → job provision_tenant`:
1. Allocate slug + pick node (least-loaded), create DNS records.
2. Authentik: group, user, 4 OIDC apps → client IDs/secrets.
3. Render compose template + `.env` (secrets generated, stored encrypted in control-plane DB).
4. Push to node over SSH, `docker compose up -d`, wait for health checks.
5. Post-setup via APIs: admin/service accounts, Immich OAuth + custom CSS config, API tokens for dashboard/agent (stored encrypted).
6. Tenant `status=active` → email + portal redirect.
Also: `suspend`, `resume`, `update` (rolling image bump), `export`, `delete` (GDPR) jobs. Idempotent steps, state machine per tenant.

### Hermes agent (phase 2, designed now)
- Per-customer agent container in the stack, using **Nous Research Hermes Agent** with an external LLM API.
- Tools via **MCP servers** we write (read-mostly): `immich` (search photos by text/place/date/person), `paperless` (full-text search, fetch doc/OCR text, tags), `dawarich` (where was I on X, trips, visits), `airtrail` (flights, stats). Cross-source questions: "When did I fly to Lisbon and show me photos + the hotel invoice."
- Chat UI inside the portal shell (streaming). Clear data-processing consent since data goes to an external LLM; redact/limit what's sent (only tool results, not bulk data).

### Storage quotas
- The booked storage (units of 100 GB, `packages/pricing/src/storage.ts`) **is** the size of the tenant's LUKS volume, so a full volume is a full quota: no per-app quota settings to keep in sync.
- Hourly the provisioner records usage per tenant (total from `df`, per service from `du` over the data directories) for the portal's storage meter, which warns at 80% and 95%.
- Upgrades grow the volume online. Downgrades shrink it offline (stack stopped for a few minutes: `e2fsck`, `resize2fs`, LUKS resize) and only when the data fits with 10% to spare; otherwise the volume keeps its size, the job is audited (`volume.shrink_held`) and the portal asks the customer to delete files or book more.
- Customers change storage on the account page, which only offers sizes that fit their data. The Stripe billing portal can still lower the quantity; the provisioner guard above covers that case.

---

## Repository layout (pnpm + Turborepo monorepo)
```
apps/portal/            Next.js (App Router): marketing, signup, tenant shell, admin console
apps/provisioner/       TS worker (BullMQ): tenant jobs, Authentik/DNS/SSH clients
packages/db/            Prisma schema: Tenant, User, Node, ServiceInstance, Secret, Job, Plan
packages/brand/         theme tokens, theme.css, services/*.css, bridge.js
packages/stack-templates/ compose + env templates per service, pinned versions
packages/sdk/           typed clients for Immich/Paperless/Dawarich/AirTrail APIs (dashboard + agent)
infra/control-plane/    docker-compose: traefik, authentik, postgres, redis, portal, provisioner
infra/node/             node bootstrap (docker, traefik, skin proxy, shared ML/tika/gotenberg)
agent/                  (phase 2) hermes container + mcp servers
```

## Phases
1. **Foundation** – monorepo, control-plane compose, Authentik with branded flows, Prisma schema.
2. **Single stack by hand** – one working customer compose (all 4 services, OIDC, shared ML/tika), skin proxy + CSS; prove SSO-in-iframe works.
3. **Provisioner** – automate phase 2 end to end (create/suspend/delete).
4. **Portal shell** – nav, iframes + bridge, dashboard widgets, onboarding/QR, admin console. ← **MVP done**
5. Hermes agent + MCP tools. 6. Stripe plans/quotas. 7. restic backups, monitoring (Prometheus/Grafana/Uptime Kuma), update pipeline.

## Risks / notes
- **Licenses**: Immich/Dawarich AGPL-3, Paperless GPL-3, AirTrail GPL-3 – offering them as SaaS is allowed, but any modified source must be published; keep license/attribution reachable (e.g. "About" page). Don't use their trademarks in the brand.
- Upstream UI changes break CSS hiding → pin versions, screenshot tests before upgrades.
- Immich needs Postgres with VectorChord, Dawarich needs PostGIS → either a custom PG image with both or two PG containers per stack (start with two; optimize later).
- RAM per stack ~2–3 GB idle (mitigated by shared immich-ml/tika/gotenberg).
- Verify at build time: Dawarich OIDC maturity, which services send `X-Frame-Options`.

## Verification
- Local dev: control plane + one node on the dev machine (Docker Desktop/WSL2), domain `*.brand.localhost` with mkcert certs, Authentik local.
- Phase-2 check: log in once at portal → open each iframe → already authenticated, branded, no upstream logo, no console CSP errors.
- Provisioner: integration test creates tenant `test1`, asserts all 4 health endpoints + OIDC login, then deletes and asserts volumes/Authentik objects removed.
- Playwright E2E: signup → wait active → navigate all sections → upload photo & document → appear in dashboard widgets.
- Mobile: scan QR in Immich app → login via SSO → backup works.

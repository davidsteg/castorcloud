# Castor Cloud

A Swiss-hosted, privacy-first alternative to Google Photos, Drive scans,
Maps Timeline and flight trackers – built on Immich, Paperless-ngx, Dawarich,
AirTrail and a Hermes-based assistant. Every customer gets a dedicated,
encrypted stack; one portal embeds all services behind one login.

- Architecture & roadmap: [docs/architecture.md](docs/architecture.md)
- Security model (what we protect, and what we can't): [docs/security.md](docs/security.md)
- Assistant design (Hermes, memory, LLM, credit limit): [docs/assistant.md](docs/assistant.md)
- Beta applications and newsletter: [docs/beta.md](docs/beta.md)
- Moving the core to Rust (what moves, what stays, and why): [docs/rust-core.md](docs/rust-core.md)

## How a customer gets a cloud

1. **Pricing page** (`app.<domain>/<locale>`) – DE/FR/IT/EN. Services and
   storage are chosen individually; the page shows where every franc goes
   (VAT, Stripe fees, 10 % open-source donation, infrastructure, operations).
2. **Sign in / register** through Authentik – e-mail or "Continue with Google".
3. **Signup** – name + address (`<slug>.<domain>`), then **Stripe Checkout**.
   During the beta the signup is an **application**: staff approve it in the
   admin console first, then the owner pays from "My clouds" (docs/beta.md).
4. **Stripe webhook** → `PROVISION` job → **worker**:
   Authentik groups + one OIDC app per booked service → **encrypted LUKS
   volume** (key wrapped in the control plane, handed over on stdin) →
   containers for the booked services only → health checks → `ACTIVE`.
5. **Welcome page** shows live progress, then opens the cloud.
6. Plan changes (Stripe billing portal) → `UPDATE`; cancellation → `SUSPEND`
   (volume locked) → after 30 days `DELETE` (volume + key destroyed).

## Layout

| Path | What |
|---|---|
| `apps/portal` | Next.js: marketing site, signup, account, tenant shell, Stripe webhook |
| `apps/provisioner` | Worker executing tenant lifecycle jobs (BullMQ) + e2e test |
| `packages/pricing` | Price list (CHF) and the transparent breakdown – shared by site and checkout |
| `packages/core` | Envelope encryption (secrets, volume keys), job queue, audit log |
| `packages/db` | Prisma schema of the control plane |
| `packages/brand` | Brand tokens, skin CSS injected into services, iframe bridge |
| `packages/stack-templates` | Per-tenant compose (one profile per service), skin proxy, pinned versions |
| `infra/control-plane` | Traefik, Postgres, Redis, Authentik (+ blueprints) |
| `infra/node` | Shared node services, `cryptd` volume helper |
| `infra/dev-certs` | Local CA + wildcard certificate for development |

## Local development

Requires Docker Desktop and Node 24. `*.brand.localhost` resolves to your machine.

```bash
cp .env.example .env                                   # replace every change-me value
npm install
bash infra/dev-certs/generate.sh brand.localhost       # dev CA + *.brand.localhost cert
npm run cp:up                                          # Traefik, Postgres, Redis, Authentik
npm run db:migrate
npm run dev -w @pc/portal                              # portal on :3000 (via Traefik)
npm run worker -w @pc/provisioner                      # provisioning worker
```

Trust the dev CA once (current user only), then restart the browser:

```powershell
certutil -user -addstore Root .dev-certs\ca.crt
```

- Site: https://app.brand.localhost
- Authentik admin: https://auth.brand.localhost (`akadmin` / `AUTHENTIK_BOOTSTRAP_PASSWORD`)
- Traefik dashboard: http://localhost:8080

### Payments

Without Stripe keys set `DEV_SKIP_PAYMENT=true`: signup provisions directly.
With Stripe test keys:

```bash
npm run stripe:sync -w @pc/portal      # products + prices from packages/pricing
stripe listen --forward-to https://app.brand.localhost/api/stripe/webhook --skip-verify
```

Put the printed `whsec_…` into `STRIPE_WEBHOOK_SECRET`.

### Google login

Create an OAuth client (type *Web*) in Google Cloud Console with redirect URI
`https://auth.<BASE_DOMAIN>/source/oauth/callback/google/`, set
`GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET`, then `npm run cp:up`.

### Provisioning test

```bash
npm run e2e -w @pc/provisioner -- --delete   # provision a test tenant, verify, delete
```

## Notes

- HTTPS is required even in dev: Immich (and others) refuse plain-HTTP OIDC issuers.
- Service hosts are flat (`photos-<slug>.<domain>`) so one wildcard certificate
  covers every tenant.
- Traefik must be ≥ v3.6 with Docker Engine 29.
- Authentik config lives in `infra/control-plane/authentik/blueprints`; the
  enrollment flow is instantiated by the worker at startup.

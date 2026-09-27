# Assistant (Hermes Agent) – design

Status: designed, pricing and website done; implementation is the next phase.

## Flow

```
 user ─► chat (portal web chat / PWA, optional Telegram)
          │
          ▼
 ┌──────────── tenant stack (encrypted volume) ─────────────┐
 │ hermes-agent                                             │
 │   memory: ~/.hermes/memory  → on the tenant LUKS volume   │
 │   tools (MCP, read-only, per-tenant API tokens):          │
 │     immich · paperless · dawarich · airtrail              │
 └───────────────┬──────────────────────────────────────────┘
                 │ question + the few relevant excerpts only
                 ▼
 llm-gateway (node-level LiteLLM, one virtual key per tenant)
   - hard monthly budget = tenant.aiCreditChf
   - usage metering per tenant (shown in the portal)
                 │
                 ▼
 AI provider: OpenRouter (default) or a Swiss provider (AI_PROVIDER=swiss)
```

## Privacy rules (enforced in configuration, not only promised)

- **No training, no retention**: OpenRouter requests carry
  `provider: { data_collection: "deny", zdr: true }`; only zero-data-retention
  endpoints are used.
- **No Chinese providers**: provider deny-list in the gateway config.
- **Honest claims**: OpenRouter is a US company. The website shows the
  OpenRouter statement unless `AI_PROVIDER=swiss` (e.g. Infomaniak, Swiss
  data centres, no training). Never claim "no US access" with OpenRouter.
- **Minimal disclosure**: tools return short excerpts/metadata, not files.
  Photos are described by Immich's own (local) ML – images are not sent.
- **Memory stays home**: Hermes' memory database lives on the tenant volume
  and is deleted (crypto-shredded) with the tenant.

## Credit / monthly limit

- Booked via Stripe as `pc_credit_{month|year}` × CHF (tiers 5/10/20/50),
  never discounted, synced to `Tenant.aiCreditChf` by the webhook.
- Provisioner sets the tenant's LiteLLM virtual key `max_budget` (converted
  from CHF net of VAT and payment fees) with `budget_duration: 1mo`.
- At 80 % the user is notified; at 100 % the gateway rejects requests and the
  chat shows "credit used up – resets on <date> / raise the credit".

## Channels

| Channel | Status | Notes |
|---|---|---|
| Web chat in the portal | default | Streams from the tenant's Hermes gateway via the portal (same-site, SSO) |
| PWA ("install app") | default | Manifest + service worker on the portal; push notifications later |
| Telegram | opt-in per user | Hermes gateway supports it. Bot messages are **not** E2E encrypted and pass Telegram servers – shown as a warning before enabling. Pairing via one-time code from the portal; allowlist = paired Telegram user ids |

## To build

1. `assistant` compose profile: hermes-agent container, memory dir on the volume.
2. MCP servers for the four services (read-only API tokens created by the provisioner).
3. Node-level LiteLLM gateway; provisioner manages per-tenant virtual keys + budgets.
4. Portal chat UI (streaming) + PWA manifest.
5. Telegram pairing flow (opt-in, with warning).

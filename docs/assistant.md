# Assistant (Hermes Agent)

Status: built (phase 5). Web chat, PWA and Telegram work end to end; see
"Tested" and "Before going live" at the end.

## Flow

```
 member ─► portal chat (web / installed PWA)      Telegram (opt-in)
             │ /api/assistant/* (SSO session)        │ Castor bot (control plane)
             └──────────────┬────────────────────────┘
                            │ member's own Hermes API key (server-side only)
                            ▼
 ┌──────────── tenant stack (encrypted volume) ────────────────────────────┐
 │ skin proxy  assistant-<slug>.<domain>: only /p/<member>/api/sessions…   │
 │ assistant   Hermes Agent, one profile per member (/p/<member>/…)        │
 │             memory + sessions on the "assistant" volume                 │
 │ assistant-tools  read-only MCP servers: photos · docs · places · flights│
 │             per-member bearer token → that member's data only           │
 └───────────────┬─────────────────────────────────────────────────────────┘
                 │ question + the few relevant excerpts only
                 ▼
 llm-gateway (node-level LiteLLM, one virtual key per tenant, model "assistant")
   - hard monthly budget from tenant.aiCreditChf
                 │
                 ▼
 AI provider: OpenRouter (default) or a Swiss provider (AI_PROVIDER=swiss)
```

## Pieces

| Piece | Where |
|---|---|
| Hermes container, init, tools (compose profile `assistant`) | `packages/stack-templates/tenant/compose.yml` |
| Skin allowlist for the Hermes API | `packages/stack-templates/tenant/skin/default.conf.template` |
| Rendered Hermes config, one profile per member, persona | `apps/provisioner/src/assistant.ts` |
| Tenant key + budget on the gateway | `apps/provisioner/src/llm.ts`, `lifecycle.ts` (`prepareAssistant`) |
| Read-only MCP tools | `agent/tools/` (image `pc-assistant-tools`, built by the provisioner) |
| Gateway + its DB, provider configs | `infra/node/compose.yml`, `infra/node/litellm/*.yaml` |
| CHF credit → USD budget | `packages/pricing/src/ai-budget.ts` |
| Portal chat, credit meter, Telegram panel | `apps/portal/src/app/tenant/[slug]/assistant/`, `src/app/api/assistant/`, `src/components/AssistantChat.tsx` |
| Hermes stream → chat events, pairing codes | `packages/core/src/assistant.ts` |
| Telegram bot | `apps/provisioner/src/telegram.ts` (`npm run telegram -w @pc/provisioner`) |

## Isolation between members

- Each member has a Hermes **profile** with its own API key (Secret
  `ASSISTANT_KEY_<memberId>`); another member's key gets 401.
- Each profile calls the tools with its own token (`ASSISTANT_TOOLS_<memberId>`,
  only the SHA-256 is in `members.json`); the tools resolve it to that member
  and use per-member credentials: Immich and AirTrail API keys minted in their
  DBs with read scopes only, Dawarich's user API key, and Paperless' own
  visibility rules (owner / shared / unowned) on the service account's results.
- Members who leave lose their secrets; `assistant-init` deletes their profile
  (memory included) on the next apply.
- Hermes has no shell, files, web, browser or code execution
  (`agent.disabled_toolsets`); its tools are memory, search over its own
  sessions and our read-only MCP servers (`trust: untrusted`, tool results
  are wrapped as data). The assistant network is internal: Hermes reaches
  only the tools and the gateway.

## Privacy rules (enforced in configuration)

- **No training, no retention**: every OpenRouter request carries
  `provider: { data_collection: "deny", zdr: true }` (gateway config).
- **No Chinese providers**: `provider.ignore` list in `litellm/openrouter.yaml`.
- **Honest claims**: the site and the chat show the OpenRouter statement
  unless `AI_PROVIDER=swiss`. Never claim "no US access" with OpenRouter.
- **Minimal disclosure**: tools return at most 25 items, 280-character
  excerpts, documents in 6000-character pages, coordinates rounded to ~11 m.
  Photos are found by Immich's local CLIP model and described by metadata;
  images are never sent. The gateway does not log prompts
  (`turn_off_message_logging`, `store_prompts_in_spend_logs: false`).
- **Memory stays home**: Hermes' data (memory, sessions) lives on the tenant
  volume and is crypto-shredded with it.

## Credit / monthly limit

- Booked via Stripe as `pc_credit_{month|year}` × CHF (5/10/20/50), synced to
  `Tenant.aiCreditChf` by the webhook.
- The provisioner sets the tenant key's `max_budget` = credit net of VAT and
  Stripe fees, converted with `AI_USD_PER_CHF`, `budget_duration: 1mo`
  (LiteLLM resets on the 1st). Suspending blocks the key; deleting removes it.
- The chat reads the tenant key's own `/key/info`: a bar with the share used,
  a warning from 80 %, and at 100 % the gateway answers 429, which the chat and
  the bot show as "credit used up – resets on <date>".

## Channels

| Channel | Notes |
|---|---|
| Web chat | Portal → `/api/assistant/*` → Hermes sessions API. Streams answers; the browser only learns which service is being searched, never tool arguments or results. |
| PWA | Manifest (with an Assistant shortcut) + a service worker that only adds an offline page; nothing from the cloud is cached on the device. |
| Telegram | One Castor bot in the control plane. The member turns it on in the chat's side panel after a warning (not end-to-end encrypted), gets a one-time code (10 minutes, stored hashed) and sends it to the bot. Private chats only; `/new` starts a new conversation, `/disconnect` unlinks. Answers are sent as plain text. |

## Configuration

Control plane (`.env`): `LLM_GATEWAY_URL` (how the provisioner, portal and bot
reach the gateway), `LITELLM_MASTER_KEY`, `AI_USD_PER_CHF`,
`TELEGRAM_BOT_TOKEN` + `TELEGRAM_BOT_USERNAME` (Telegram off when unset).
Node: `LITELLM_MASTER_KEY`, `LITELLM_DB_PASSWORD`, `AI_PROVIDER`, `AI_MODEL`,
`LLM_API_KEY` (+ `LLM_BASE_URL` for the Swiss provider).

## Tested

Against real containers (Paperless, Dawarich, AirTrail, LiteLLM, Hermes, skin,
a mock LLM): per-member isolation in the tools and in Hermes, the skin
allowlist, privacy parameters reaching the provider, budget exhaustion, the
chat stream translation, and the Telegram bot (pairing, chat, `/new`, credit
used up, `/disconnect`) against a mock Telegram API. The chat UI was checked
with mocked API responses.

## Before going live

- **Photos tools** were not run against a live Immich (image not pullable in
  the build sandbox); run `npm run e2e` with Photos booked.
- **Swiss provider** (`litellm/swiss.yaml`): set the real model name and the
  per-token prices, otherwise spend (and the budget) is not counted.
- **Gateway reachability**: the gateway listens on the node's `127.0.0.1:4000`;
  the control plane reaches it through an SSH tunnel or a private network.
- Portal chat end to end with SSO on a provisioned tenant (the routes were
  exercised piecewise, not through a real login).

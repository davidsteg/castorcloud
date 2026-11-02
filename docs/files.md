# Files (OpenCloud)

Files is a household drive: personal folders, shared spaces, links with a
password. It is [OpenCloud](https://opencloud.eu) (server Apache-2.0, web app
AGPL-3.0), one instance per tenant in the tenant stack (`--profile files`),
booked like the other services (CHF 2.00 a month, `packages/pricing`).

## Naming and licences

- We sell it as **Files** ("Dateien", "Fichiers", "File"). OpenCloud is
  credited in text only ("built on OpenCloud", README, open-source page and
  donation). Its name is never part of a product name or domain, and its logo
  is not used to advertise our product: OpenCloud GmbH's terms need written
  consent for commercial use of its marks.
- We run the upstream image unchanged (`opencloudeu/opencloud`, production
  channel, `OPENCLOUD_VERSION` in `versions.env`). Configuration and the skin
  proxy's injected CSS are not changes to the code. If we ever patch the web
  app (AGPL-3.0), its source must be offered to every user.
- OpenCloud gets its share of the 10 % open-source donation.

## Sign-in and roles

| What | How |
|---|---|
| Identity | The tenant's Authentik app `<slug>-files`, a **public** client (PKCE, no secret) with 5-minute access tokens. OpenCloud's own IdP is not started; its user directory is filled on first sign-in |
| Token check | JWT signature and issuer (`/application/o/<slug>-files/`): a token of another tenant's app is refused |
| Who gets in | Role mapping in `opencloud-proxy.yaml` (castord `render::opencloud_proxy`): `cust-<slug>-owners` → Space Admin, `cust-<slug>` → User. A token without either group is refused by OpenCloud itself |
| Off | Basic auth, app tokens, guest links, group sync from Authentik (`PROXY_AUTOPROVISION_CLAIM_GROUPS` points at a claim that doesn't exist), update check |
| Ending access | Revocation ends the Authentik sessions and refresh tokens (`revoke.rs`); OpenCloud keeps no sessions of its own, so access ends when the last access token expires (5 minutes) |
| Sharing | Public links always need a password |

Desktop and phone sync apps are not offered yet: they sign in with fixed
client ids (`OpenCloudDesktop`, …) that Authentik can register only once for
all tenants. Doing that safely needs its own design.

## Storage, CSP, embedding

- Two volumes on the tenant's encrypted storage: `files-config` (OpenCloud's
  generated `opencloud.yaml` with its internal secrets) and `files-data`.
  `files-init` hands both to uid 1000; OpenCloud runs as 1000 with all
  capabilities dropped and `no-new-privileges`. Space quotas are off
  (`GRAPH_SPACES_DEFAULT_QUOTA=0`): the tenant's storage quota is the limit.
- `opencloud-csp.yaml` (castord `render::opencloud_csp`) replaces upstream's
  CSP: no third-party hosts (app store, update check, map tiles, diagrams,
  office), our sign-in host, and `frame-ancestors` the tenant portal. The skin
  proxy keeps this CSP and drops only `X-Frame-Options`.

## Checked against OpenCloud 7.2.4

With a stand-in IdP (same signing key for two tenants, like Authentik):
member → User, owner → Space Admin, token with another tenant's groups or none
→ refused, token from another tenant's issuer → 401, basic auth with the admin
password → 401, CSP as rendered. Refused tokens answer 500 rather than 403
(upstream behaviour).

## Not yet

- Desktop and phone apps (see above).
- Office editing (Collabora): needs a subscription for production hosting.
- Our logo in the web app's top bar (`packages/brand/services/files.css`).

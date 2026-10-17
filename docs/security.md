# Security model

Customer data is personal: photos, documents, location history. This document
states what we protect, how, and what is technically **not** possible. The
public security page (`/[locale]/security`) must never promise more than this.

## Threat model

| Threat | Protected? | How |
|---|---|---|
| Stolen / decommissioned disks, hosting-provider staff with disk access | Yes | Per-tenant LUKS2 volume (AES-XTS-512); key never on the node's disk |
| Backup box (or whoever steals it) reads backups | Yes | Raw ZFS sends: backups stay encrypted with the tenant's own key, which never reaches the box. See [backups.md](backups.md) |
| Someone with the provisioner's key wipes the backups | Partially | The box itself refuses to delete anything younger than 7 days or a tenant's newest backup |
| Another customer reads my data | Yes | Separate stack, private networks, separate DBs, separate OIDC apps restricted to the tenant group |
| Another household member reads my data through the assistant | Yes | One Hermes profile, API key and tools token per member; the tools use that member's own read-only credentials (or Paperless' visibility rules). See [assistant.md](assistant.md) |
| The AI provider keeps or trains on my data | By contract + config | Zero-data-retention, no-training routing enforced by the gateway; only the question and short excerpts leave the stack, never images |
| A customer's own app (custom stack) attacks other customers, the nodes or the control plane | Yes, by isolation | Separate STACKS nodes without tenant data, userns-remap + gVisor, all capabilities but a few dropped, internal networks, egress only to listed hosts via a proxy that refuses private addresses, sign-in in front, own domain. See [stacks.md](stacks.md) |
| Someone swaps Castor's node images on the registry or on the way | Yes, in production | Nodes run only images whose cosign signature from our key names that exact digest, pulled by digest; CI scans every image and dependency. See [ci.md](ci.md) |
| Network eavesdropping | Yes | TLS everywhere; OIDC over HTTPS only |
| Our staff reading data in routine operations | Organisational + audit | No standing access; break-glass requires two people, is written to `AuditLog`, customer is informed |
| Root on a running node reading mounted data | **No** | The services (search, face recognition, OCR) must process data in clear while running. Only confidential computing (AMD SEV-SNP) could narrow this — possible future premium tier |
| Compromised control plane | Partially | Holds wrapped keys + KEK: can unlock volumes. Protect it most (see below) |

We therefore never claim "zero knowledge" or "we cannot read your data".

## Encryption at rest

- **Volume per tenant**: `/srv/pc/volumes/<slug>.luks`, mounted at
  `/srv/pc/tenants/<slug>`. All containers' data (databases, photos,
  documents) live on it via `compose.volumes.yml`. The file is sized to the
  booked storage, so it doubles as the storage quota.
- **Keys**: 512 random bits per tenant (`VolumeKey`), stored only wrapped
  (AES-256-GCM, tenant id as AAD) with `VOLUME_KEK`. The provisioner unwraps
  in memory and passes the key to `pc-cryptd` on **stdin**; the helper keeps it
  in `/dev/shm` of a short-lived container and deletes it. PBKDF2 is used
  because the key is full-entropy (no password stretching needed) – this keeps
  unlock cheap.
- **Cost**: AES-NI makes dm-crypt overhead small (low single-digit % CPU for
  typical I/O). No per-file encryption, no impact on search/ML features.
- **Crypto-shredding**: deleting a tenant destroys the LUKS header *and* the
  wrapped key. Leftover copies or backups of the volume become unreadable.
  Encrypted control-plane database dumps still hold the wrapped key for up to
  30 days, so deletion is final at most 30 days later ([backups.md](backups.md)).
- **Locking**: suspending a tenant closes the volume. After a node reboot all
  volumes are closed until the provisioner re-opens them.
- **Tenant secrets** (DB passwords, OIDC client secrets): `Secret` table,
  AES-256-GCM with `SECRETS_ENCRYPTION_KEY`, bound to tenant + name.

## Keys we must protect

| Key | Where | Production requirement |
|---|---|---|
| `VOLUME_KEK` | control plane env | Move to an HSM / Vault transit engine (only `wrap/unwrap` in `packages/core/src/crypto.ts` changes). Rotation = re-wrap all `VolumeKey` rows (`kekVersion`) |
| `SECRETS_ENCRYPTION_KEY` | control plane env | Same |
| Authentik API token | control plane env | Dedicated service account with only the needed permissions (not the bootstrap token) |
| Stripe secret key, webhook secret | control plane env | Restricted key; webhook signature verified on every call |
| `LITELLM_MASTER_KEY` | control plane + node env | The one key that also lives on nodes (the gateway needs it). It manages LLM keys and budgets only, it cannot unlock data. The portal and bot use each tenant's own key |
| `TELEGRAM_BOT_TOKEN` | control plane env | Whoever holds it can read and answer messages sent to the bot |
| `GITHUB_APP_PRIVATE_KEY` | control plane env | Mints read tokens for every customer repository connected to the GitHub App. Rotate in the app settings if exposed |
| Database dump secret key | **offline only** | Opens the control-plane database dumps (`castor-admin backup db-keygen`). Keep it with offline copies of `VOLUME_KEK` and `SECRETS_ENCRYPTION_KEY`: restoring a lost control plane needs all three |

Never store these on data-plane nodes.

## Identity

- One IdP (Authentik) for the portal and every service; services have no
  local passwords (Immich password login off, Paperless regular login off,
  Dawarich e-mail login off, AirTrail password form hidden).
- Each service app is bound to the tenant's Authentik group only.
- Google and GitHub login use matching mode `email_deny`: they never attach
  to an existing account with the same e-mail (prevents account takeover).
- Social logins need an address the provider has verified (the apps identify
  people by e-mail). Authentik doesn't check this itself; a deny stage in the
  source flows does ([github.md](github.md)).
- GitHub repository access is separate from GitHub login: a GitHub App the
  customer installs on their own account, read-only contents of the
  repositories they pick, tokens minted per use for 1 hour and never stored
  ([github.md](github.md)).
- Production: SMTP configured ⇒ e-mail verification is required for
  self-registration. Staff accounts: enforce WebAuthn/TOTP.

### Ending access

The apps keep their own sessions and keys after the SSO login, so ending
access has to reach into each app (`rust/crates/castord/src/revoke.rs`, queued
as `AccessRevocation` rows and run by the `REVOKE_ACCESS` job, or by the next
start of a suspended tenant):

| Event | Portal + Authentik | Apps |
|---|---|---|
| Log out | this browser, every portal host | browser sessions end; phone apps stay connected |
| Log out on all devices | every browser (`User.sessionsRevokedAt`), every Authentik session, Telegram links | also phone app sessions and API keys (the assistant's key stays) |
| Member removed | out of the tenant's Authentik groups (next apply) | everything, including the assistant's key |

Embedded apps sign in again silently wherever the portal is still signed in.
Deleting a tenant destroys the data and keys, so it needs no revocation.

## Access by staff (policy)

1. No routine SSH access to data-plane nodes; changes go through the provisioner.
2. Break-glass (e.g. a support case the customer asked for): two people,
   time-limited, logged as `staff.break_glass` in `AuditLog` with reason,
   customer notified.
3. `AuditLog` is append-only for the application; replicate it off-host.

## Open items before launch

- Backup box off-site in Switzerland, and a restore drill before launch ([backups.md](backups.md)).
- KEK in HSM/Vault; Authentik service-account token; Stripe restricted key.
- Rate limiting on signup/login; CSP for the portal; security headers (HSTS).
- Monitoring and alerting (volume fill level ≥ 90 %, failed jobs).
- Legal texts (Impressum, privacy policy per revDSG incl. Stripe/Google/LLM
  sub-processors, terms), reviewed by counsel.
- Swiss cross / "Swiss" claims: the Swiss coat of arms and cross are protected
  (Wappenschutzgesetz); indications of Swiss origin for services require the
  company's seat and actual administration in Switzerland. The site currently
  uses only a text "CH" badge.

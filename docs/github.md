# GitHub: login and repository access

Two separate things, on purpose:

| | What it is | What we can do with it |
|---|---|---|
| **Continue with GitHub** | An Authentik login source (`infra/control-plane/authentik/blueprints/20-signup.yaml`), GitHub **OAuth App** | Read the GitHub profile and e-mail addresses at login. Nothing else: the token Authentik keeps can't read any code |
| **Connect GitHub** (account page) | The Castor **GitHub App**, installed by the customer on their own GitHub account | Read the contents of the repositories the customer picked, with tokens we mint per use |

Logging in with GitHub never grants repository access, and connecting
repositories works the same whether you log in with GitHub, Google or e-mail.

## Login

- Scopes `read:user user:email` only.
- Matching mode `email_deny`: a GitHub login never attaches to an existing
  account with the same e-mail (same as Google).
- **Verified e-mail only.** The apps identify people by e-mail, and Authentik
  takes GitHub's primary address whether or not GitHub has verified it. The
  property mapping `castor-github-verified-email` keeps only a verified
  primary address, and the deny stage `castor-social-email-deny` (first stage
  of both source flows) refuses the login otherwise. The same guard refuses a
  Google login whose address Google hasn't verified.

Setup: GitHub → Settings → Developer settings → **OAuth Apps** → New:
callback URL `https://auth.<BASE_DOMAIN>/source/oauth/callback/github/`. Put
its client id and secret into `GITHUB_LOGIN_CLIENT_ID` /
`GITHUB_LOGIN_CLIENT_SECRET`, then `npm run cp:up`.

## Repository access

### Linking

1. Account page → **Connect GitHub**. The portal stores state and a PKCE
   verifier in a 10-minute `__Host-` cookie bound to the signed-in user, and
   sends the customer to GitHub's sign-in for the app.
2. `/api/github/callback` checks state and user, exchanges the code, and asks
   GitHub which installations of the app are on **that GitHub user's own
   account**. Only those are linked (`GitHubInstallation`). The user token is
   revoked right away; it is never stored.
3. No installation yet: the customer continues to GitHub to install the app
   and pick repositories. GitHub then opens the app's Setup URL
   (`/api/github/setup`), which starts step 1 again. The `installation_id`
   GitHub appends to that URL is ignored: an id in a URL proves nothing.

Installations on organisations are not linked for now: any org member can see
the org's installation, and linking it would give them read access to
repositories they may not be allowed to read.

### Using it

`repoReadToken(userId, "owner/name")` (`packages/core/src/github.ts`) mints an
installation token for that one repository with `contents: read`, valid for 1
hour. Mint it right before use (e.g. a clone for a custom stack), never store
it, and never hand it to a tenant container.

### Ending it

- **Disconnect** on the account page uninstalls the app from the customer's
  GitHub account and deletes the link.
- Uninstalling or suspending on GitHub reaches us through the webhook
  (`/api/github/webhook`, signed with `GITHUB_APP_WEBHOOK_SECRET`): the link
  is deleted or suspended. Repository changes and account renames are synced
  the same way.
- Deleting the user deletes their links (cascade).

### Setup

GitHub → Settings → Developer settings → **GitHub Apps** → New:

| Setting | Value |
|---|---|
| Callback URL | `https://app.<BASE_DOMAIN>/api/github/callback` |
| Expire user authorization tokens | on |
| Request user authorization (OAuth) during installation | **off** (the Setup URL handles it) |
| Setup URL | `https://app.<BASE_DOMAIN>/api/github/setup`, "Redirect on update" on |
| Webhook URL | `https://app.<BASE_DOMAIN>/api/github/webhook`, with a secret (`openssl rand -hex 32`) |
| Repository permissions | **Contents: Read-only** (Metadata: Read-only is added automatically). Nothing else |
| Organization / account permissions | none |
| Subscribe to events | Installation target (installation events are always sent) |
| Where can this GitHub App be installed? | Any account |

Then set `GITHUB_APP_SLUG`, `GITHUB_APP_CLIENT_ID`, `GITHUB_APP_CLIENT_SECRET`,
`GITHUB_APP_PRIVATE_KEY` (the downloaded `.pem`, newlines as `\n`) and
`GITHUB_APP_WEBHOOK_SECRET`. The account page shows the GitHub panel once all
five are set.

The private key can mint tokens for every connected repository: treat it like
`SECRETS_ENCRYPTION_KEY` (control plane only, never on a node).

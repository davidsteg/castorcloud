# Beta: applications and newsletter

## Beta notice

While `BETA` is on (the default), every page of the public site shows a
banner linking to `/<locale>/beta`, which explains what the beta means:
the platform runs in our homelab in Switzerland ("the friendly homelab from
your neighbourhood"), not a data centre, and moves to bigger infrastructure
as the number of users grows; clouds are opened by application; features and
prices may change; and interruptions are possible, with no uptime promised.
The site's hosting claims say "in Switzerland", not "data centres", for the
same reason. When the platform moves, update those texts in `i18n/`. The signup page repeats it, and the applicant
must tick a second box acknowledging it. `BETA=off` removes all of this and
signups go straight to payment, as before.

## Applications

```
signup form ──► Tenant APPLIED ──approve──► PENDING ──Stripe checkout──► PROVISIONING ──► ACTIVE
                     │  (staff)                 (owner pays from "My clouds")
                     └──reject / withdraw──► deleted (address free again)
```

- The signup form creates the tenant as `APPLIED` with an optional note. The
  address (slug) is reserved from then on. Nothing is charged and no Stripe
  customer or stack exists yet.
- One open application per person.
- The applicant gets a confirmation e-mail; `APPLICATIONS_NOTIFY_TO` gets a
  heads-up with the address only (no personal data in staff mail).
- Staff review applications at `app.<domain>/<locale>/admin`. **Approve** sets
  `PENDING`, clears the note (data minimisation), records who approved and
  e-mails the applicant a link to "My clouds", where the owner starts the
  Stripe checkout. **Reject** deletes the application and e-mails the
  applicant. Both are conditional on the application still being open, and
  audited (`application.approve`, `application.reject`,
  `application.withdraw`).
- The Stripe webhook only provisions tenants that were `PENDING`, and a
  checkout session is only created for a `PENDING` tenant by its owner, so an
  application can't be paid for or provisioned before approval.
- A tenant has one open checkout at a time (older ones are expired) and none
  once it has a subscription. Provisioning starts only when the checkout is
  paid; delayed payment methods wait for
  `checkout.session.async_payment_succeeded`. Should a second checkout still
  be paid, the webhook keeps the first subscription, cancels the second and
  audits `billing.duplicate_subscription` so it can be refunded by hand.
- Webhook events to enable in Stripe: `checkout.session.completed`,
  `checkout.session.async_payment_succeeded`,
  `customer.subscription.updated`, `customer.subscription.deleted`.
- Only the owner can open the Stripe customer portal (invoices, payment
  method, cancellation).

### Staff

The admin console is only for users with `User.isStaff`; everyone else gets a
404, and every action checks again. Its statistics panel shows totals only
(clouds by status, estimated MRR, users, storage, backups, jobs, alerts,
nodes): no tenant or person is named there, and no tenant content is read.
`/<locale>/admin/audit` shows `AuditLog` read-only, newest first, filtered by
action (or a prefix like `staff.`), cloud or actor.
Grant it from the control plane (the
person must have signed in to the portal once):

```sh
castor-admin staff add you@example.ch
castor-admin staff list
castor-admin staff remove you@example.ch
```

## Database roles

The portal, castord, the monitor and castor-admin connect as `castor_app`,
which reads and writes rows but owns nothing: it can't change the schema, and
it can only add to `AuditLog` (docs/security.md, Database roles). The nightly
dump runs as the read-only `castor_backup`. Only Postgres, Authentik and
`migrate` use the owner (`POSTGRES_USER`).

Set both roles up once, on the NAS in this checkout's `infra/beta`, before
starting a release that has them (v0.7.4 or later). The compose file refuses
to start without `APP_DATABASE_URL` and `BACKUP_PG_PASSWORD`.

```sh
# 1. Two passwords (hex, so they need no escaping in a URL).
APP_PW=$(openssl rand -hex 32); BACKUP_PW=$(openssl rand -hex 32)

# 2. The roles with their passwords; the migration adds their grants.
docker compose exec -T postgres psql -v ON_ERROR_STOP=1 -U pc -d portal \
  -v app="$APP_PW" -v backup="$BACKUP_PW" <<'SQL'
SELECT 'CREATE ROLE castor_app' WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'castor_app') \gexec
SELECT 'CREATE ROLE castor_backup' WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'castor_backup') \gexec
ALTER ROLE castor_app LOGIN PASSWORD :'app';
ALTER ROLE castor_backup LOGIN PASSWORD :'backup';
SQL

# 3. .env: the applications' URL and the backup password.
printf 'APP_DATABASE_URL=postgresql://castor_app:%s@postgres:5432/portal\nBACKUP_PG_PASSWORD=%s\n' \
  "$APP_PW" "$BACKUP_PW" >> .env
unset APP_PW BACKUP_PW

# 4. The new release: set CASTOR_VERSION in .env, then
docker compose up -d
```

`migrate` runs first and grants the roles their rights; the other services
start after it. To check:

```sh
docker compose exec -T postgres psql -U pc -d portal -tAc \
  "SELECT has_table_privilege('castor_app', '\"AuditLog\"', 'DELETE')"   # f
docker compose logs --tail 20 castord monitor portal   # no "permission denied"
./castor-admin backup db-dump   # a dump as castor_backup
```

A row that must be changed by hand needs the owner:
`docker compose exec postgres psql -U pc -d portal`.

## castor-admin and logs

On the NAS, run castor-admin through `infra/beta/castor-admin` (e.g.
`infra/beta/castor-admin staff list`): it runs it in the castord container
with your NAS account as `CASTOR_OPERATOR`, so the audit log records
`ops-cli:<account>` as the actor instead of a bare `ops-cli`.

Every container's log is rotated (10 files of 20 MB, `x-logging` in
docker-compose.yml) and goes with the container when it is recreated. What
must last is in `AuditLog` (in the nightly sealed dump) and gatekeeper's
`audit.jsonl` below `GATEKEEPER_LOGS`.

## Newsletter

Double opt-in, as Swiss nFADP and the GDPR expect for marketing e-mail:

1. Someone subscribes with the form in the site footer, the checkbox at
   signup, or the button on "My clouds". The row is `PENDING` and a
   confirmation mail goes out. The form always answers the same way, so it
   doesn't reveal who is subscribed.
2. The link in that mail opens a page with a **Confirm** button (a click, so
   mail scanners that fetch links don't subscribe anyone). It is valid for 7
   days and works once; only the token's SHA-256 is stored.
   `confirmedAt` is the record of consent.
3. Every newsletter must carry the subscriber's unsubscribe link (a page with
   an **Unsubscribe** button) and the `List-Unsubscribe` /
   `List-Unsubscribe-Post` headers for one-click unsubscribe in mail clients
   (RFC 8058, `POST /api/newsletter/unsubscribe`). The link carries an HMAC of
   the subscriber id keyed with `AUTH_SECRET`, so no unsubscribe token is
   stored; rotating `AUTH_SECRET` invalidates links in mails already sent.

Abuse limits on the public form: a hidden honeypot field, at most one
confirmation mail per address every 15 minutes, 5 subscribe requests per
client IP (Traefik's `X-Real-IP`) and 300 overall per hour. The limits live in
the portal process, which is fine for one portal instance.

### Sending

No mailing service is chosen yet. Until then, staff export the confirmed
subscribers as CSV from the admin console (audited as `newsletter.export`);
each row has its unsubscribe URL to put in that person's mail. A sender built
into the portal would use `getMailer()` (`apps/portal/src/lib/mail.ts`) with
`unsubscribeUrl()` and `listUnsubscribeHeaders()` from
`apps/portal/src/lib/newsletter.ts`.

## E-mail

The portal sends through any SMTP provider: `MAIL_SMTP_URL`
(`smtps://user:pass@host:465`, or `smtp://…:587` with STARTTLS required, TLS
1.2 or newer) and `MAIL_FROM`. Another transport only has to implement the
`Mailer` interface. Without `MAIL_SMTP_URL`, development prints mails
(including links) to the console and production refuses to send. A mail that
can't be sent is logged and doesn't fail the action that triggered it.

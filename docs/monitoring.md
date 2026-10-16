# Monitoring and alerts

A small monitor process checks the platform every 5 minutes and tells the
operator on Telegram and/or by e-mail when something needs attention. It is
self-hosted and adds no service to run: no Prometheus, no Grafana, no
third-party monitoring SaaS.

```
npm run monitor -w @pc/provisioner              # run (every MONITOR_INTERVAL_S)
npm run monitor -w @pc/provisioner -- once      # one round, then exit
npm run monitor -w @pc/provisioner -- status    # open alerts, with details
npm run monitor -w @pc/provisioner -- test      # send a test alert
```

It runs next to the provisioner worker on the control plane but as its own
process, so it can still report a stopped worker, a lost database or a
database it can't reach.

## What it checks

| Check | Alerts when | Severity |
|---|---|---|
| Nodes | not probed successfully for 15 min; ZFS pool not `ONLINE`; under 15% pool space free (under 10%: critical) | critical / warning |
| Tenant services | a booked service doesn't answer through Traefik (the customer's own route) two rounds in a row | critical |
| Provisioning jobs | a job failed after all retries (once per job); a job running for over 3 h; jobs waiting for over 60 min | critical / warning |
| Backups | a tenant's newest backup is older than two intervals (a day for the first one); 2+ failed runs in a row; failed verification; backups switched off | critical / warning |
| Certificates | the certificate Traefik serves for `BASE_DOMAIN` and `auth.` expires within 14 days (3: critical), is expired, or doesn't validate | warning / critical |
| Sign-in security | 30+ failed sign-ins in 15 min; 10+ from one address or against one account; Authentik's suspicious-request events; an admin impersonating a user; an Authentik admin signing in | warning / critical |
| Audit log | staff break-glass access, a restored backup, an unhealthy backup box | critical / warning |
| Platform | no provisioner worker has been seen for 2 minutes (heartbeat, or a running job's lease); Authentik doesn't answer; the monitor can't reach its database | critical |

Node facts come from the provisioner's node watcher (every 5 min), which
probes each node over SSH as the locked-down `castor` user. The monitor reads
them from the database and needs no SSH access of its own. Backup status comes
from the backup runs (docs/backups.md). Service probes also update each
service's `healthy` flag in the database.

A check that fails to run (Authentik is down, a query errors) is reported
itself, and alerts it raised earlier stay open instead of reading as resolved.

## How alerts behave

- One message per round lists what is new, what escalated to critical, and
  what resolved. An open alert is repeated every `MONITOR_RENOTIFY_HOURS` (12)
  until it clears.
- One-off events (a failed job, an impersonation, an admin sign-in) are sent
  once.
- Flaky probes (services, Authentik, unreachable certificates) must fail
  two rounds in a row before they alert.
- If no channel accepts a message, it is sent again next round.
- Telegram gets every message. E-mail gets messages with anything critical in
  them, or every message when Telegram isn't configured. With neither
  configured, alerts only go to the process log (development).
- State lives in the `Alert` table. Resolved alerts are kept for 90 days.

## Privacy

Telegram's servers are outside Switzerland and messages to a bot are not
end-to-end encrypted. So the text of an alert names only nodes, tenant slugs,
hosts and counts: never e-mail addresses, usernames, IP addresses or error
output. Those stay in the alert's `details` in the control-plane database,
where `-- status` shows them. Use a Swiss mail provider for `MONITOR_SMTP_URL`
to keep e-mail alerts in Switzerland too. SMTP is only used over TLS.

The optional heartbeat (`MONITOR_HEARTBEAT_URL`) sends a bare request with no
data.

## Least privilege

The monitor reads Authentik's event log with its own token,
`MONITOR_AUTHENTIK_TOKEN`, never the bootstrap token. The blueprint
`infra/control-plane/authentik/blueprints/40-monitor.yaml` creates a
`castor-monitor` service account whose role may only view events and users;
tested on Authentik 2025.10, where it gets 403 on groups, providers, roles,
system settings and any write. The blueprint does nothing while the token is
unset.

## Setup

1. Generate the Authentik token and add it to `.env`:
   `MONITOR_AUTHENTIK_TOKEN=$(openssl rand -hex 32)`, then recreate the
   Authentik containers so the blueprint sees it
   (`npm run cp:up:prod`).
2. Telegram: send `/start` to the Castor bot from your own account, and set
   `MONITOR_TELEGRAM_CHAT_ID` to your Telegram user id (for example from
   @userinfobot).
3. E-mail (recommended for critical alerts): `MONITOR_SMTP_URL`
   (`smtps://user:pass@host:465`), `MONITOR_EMAIL_TO`, and optionally
   `MONITOR_EMAIL_FROM`.
4. Run `npm run monitor -w @pc/provisioner -- test` and check that the test
   alert arrives, then keep `npm run monitor -w @pc/provisioner` running the
   same way as the worker.
5. Recommended: a dead-man's switch. Point `MONITOR_HEARTBEAT_URL` at a
   heartbeat check (a self-hosted Healthchecks instance on another machine,
   or any service you trust with a ping) that alerts you when the pings stop,
   which covers the whole control plane going down.

Other settings: `MONITOR_INTERVAL_S` (300), `MONITOR_RENOTIFY_HOURS` (12),
`MONITOR_TLS_HOSTS` (hosts whose certificate is checked),
`MONITOR_TLS_ADDRESS` (where to connect when the public address isn't
reachable from the LAN, e.g. pfSense without NAT reflection), and
`MONITOR_AUTHENTIK_URL` (Authentik's address, when not
`https://auth.BASE_DOMAIN`).

## Not covered yet

- The control-plane database dump is not tracked (the backup runs only log
  it), nor is the backup box's free space.
- Host-level metrics of the control plane itself (disk, memory) and container
  restarts on nodes.
- Thresholds are fixed in `apps/provisioner/src/monitor-rules.ts`.

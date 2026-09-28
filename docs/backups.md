# Backups

Every tenant's data lives in one encrypted ZFS dataset on its node
(`<pool>/castor/t/<slug>` on TrueNAS, `castor/t/<slug>` on a Mac; its own key). Backups copy that dataset, still
encrypted, to a separate **backup box**: a second machine with its own ZFS pool
(a second TrueNAS, later off-site in Switzerland). The box never holds a
tenant key, so it can't read what it stores.

```
 node (TrueNAS / Mac mini)          control plane (provisioner)          backup box
 ─────────────────────────          ───────────────────────────          ──────────
 castor-snap snapshot    ◄── ssh ──  every 6 h, per active tenant
 castor-snap send  ───── raw stream (zfs send -w, ciphertext) ──────►  castor-backup recv
 castor-snap sent        ◄── ssh ──  bookmark, drop the snapshot         <pool>/castor-backup/t/<slug>/g<n>@cb-<time>
                                     prune (retention)          ── ssh ─► castor-backup prune / forget
                                     nightly: pg_dumpall, gzip, seal ──► castor-backup db-put
                                     weekly: verify             ── ssh ─► castor-backup verify
```

## Who can do what

| Party | Holds | Can | Can't |
|---|---|---|---|
| Node | Nothing of the box | Send a raw stream when the provisioner asks | Reach the backup box at all |
| Backup box | Ciphertext only | Store, list, verify checksums, send back | Decrypt anything; it has no tenant key and no database key |
| Provisioner key on the box | Its forced command (`castor-backup`) | Add backups, delete old ones | Delete anything younger than `MIN_DAYS` (default 7) or a tenant's newest backup; roll history back (no `-F`) |
| Control plane | Wrapped tenant keys, `VOLUME_KEK`, the dump **public** key | Everything above | Read a database dump (the secret key is offline) |

A hacked node can't touch the backups. A hacked control plane can read live
data anyway (it holds the keys, see [security.md](security.md)), but it can't
quietly wipe the last week of backups: the box enforces that floor itself.

## How a backup runs

1. `castor-snap snapshot <slug> cb-<UTC time>` on the node.
2. If the node still has the bookmark of the last backup and the box has that
   snapshot in the current generation, the send is **incremental** from the
   bookmark. Otherwise it's a **full** send into a new generation `g<n+1>`
   (first backup, after a restore, after a node was rebuilt, or when a tenant
   moved nodes).
3. The provisioner pipes the node's stream into the box (`zfs receive -u`,
   never mounted, never rolled back).
4. `castor-snap sent` turns the snapshot into a bookmark and destroys it. A
   bookmark takes no space, so backups don't count against the tenant's quota
   for longer than the transfer.

Snapshots are atomic across the whole dataset, so every service's data (the
Postgres databases included) is from the same instant. Postgres recovers from
that like from a power cut.

Suspended tenants aren't backed up (their data doesn't change); their last
backup is kept. When a tenant is deleted its key is destroyed, so its backups
become unreadable at once, and the box forgets them once they're past the
floor.

## Retention

Everything from the last 48 hours, then the newest backup of each of the last
14 days, 8 weeks and 12 months (`apps/provisioner/src/retention.ts`). With a
6-hour interval that's about 40 backups per tenant. Incrementals only store
what changed.

## Monitoring

- Each tenant's `TenantBackup` row: last success, last error, consecutive
  failures, bytes sent, last verification.
- The audit log gets `backup.failed` (first failure), `backup.recovered`,
  `backup.verify_failed`, `backup.box_unhealthy`, `backup.restored`.
- After every run the worker logs tenants with problems: failing, no success
  for two intervals, or failed verification. `npm run backup -- status` shows
  the same, plus the box's pool and the newest database dump.
- Weekly, the box reads every tenant's newest backup back (`zfs send` to
  `/dev/null`), which checks every block's checksum. Schedule a monthly scrub
  of the box's pool on top (TrueNAS: Data Protection > Scrub Tasks).

## Restore

```sh
npm run backup -w @pc/provisioner -- list alpha                 # what's there
npm run backup -w @pc/provisioner -- restore alpha               # newest
npm run backup -w @pc/provisioner -- restore alpha g1@cb-20260928T0600Z
```

`restore` queues a `RESTORE` job on the tenant's queue, so it never overlaps
another job of the tenant. The job stops the stack and locks the dataset,
**sets the current dataset aside** (`<slug>.before-restore-<time>`, locked,
not mounted), receives the backup, and runs the normal apply, which unlocks
the dataset with the tenant's key and starts the stack. If receiving fails, the
set-aside dataset is put back. Once the customer confirms the restored data,
free the space:

```sh
npm run backup -w @pc/provisioner -- discard alpha before-restore-20260928t1800
```

**The node is lost:** `restore alpha --node mini-2` moves the tenant to
another node and restores there. Its routes follow on apply.

## The control-plane database

The tenants' keys are in the control-plane database (wrapped with
`VOLUME_KEK`). Without it no backup can be restored, so the database is backed
up too: nightly at 02:30, `pg_dumpall` (portal and Authentik), gzipped, and
sealed to an X25519 public key (`apps/provisioner/src/db-dump.ts`). Only the
public key is on the control plane; the secret key is kept **offline**. The
box keeps dumps for 30 days (never fewer than the newest three).

Set it up once:

```sh
npm run backup -w @pc/provisioner -- db-keygen
```

Put `BACKUP_DB_PUBLIC_KEY` in `.env`. Store the secret key offline, in a
password manager and on paper, **together with** `VOLUME_KEK` and
`SECRETS_ENCRYPTION_KEY` from `.env`. A full recovery of a lost control plane
needs all three:

```sh
npm run backup -w @pc/provisioner -- db-list
npm run backup -w @pc/provisioner -- db-get cp-20260928t0230.sql.gz.enc dump.enc
npm run backup -w @pc/provisioner -- db-open dump.enc secret.txt dump.sql.gz   # on a trusted machine
gunzip -c dump.sql.gz | docker exec -i pc-control-plane-postgres-1 psql -U pc -d postgres
```

Because old dumps still hold a deleted tenant's wrapped key, a deleted
tenant's data is unrecoverable for good once the newest dump from before the
deletion is gone: 30 days after deletion at most.

## Setup

1. The backup box: [infra/backup/README.md](../infra/backup/README.md).
2. Nodes: `castor-snap` next to `castor-zfs`, allowed by the node's SSH gate
   (`infra/node/bin/castor-snap`; the TrueNAS setup and macOS bootstrap install it). It reads the pool and dataset root from
   the node's `castor.conf`, like `castor-zfs`.
3. `.env` on the control plane: `BACKUP_TARGET=castor-backup@<box>`,
   `BACKUP_DB_PUBLIC_KEY=…`, optionally `BACKUP_INTERVAL_HOURS` (default 6)
   and `BACKUP_PG_CONTAINER`. The provisioner's SSH key must be in
   `~/.ssh/config` for the box, as for the nodes.
4. `npm run backup -w @pc/provisioner -- run`, then `status`.

Needs pool feature `bookmark_v2` (raw incrementals from bookmarks), on by
default in OpenZFS 2.x and TrueNAS SCALE.

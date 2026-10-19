# TrueNAS nodes through gatekeeper

On a TrueNAS, Castor never touches the host itself: no Docker socket, no SSH
key, no TrueNAS API key, no host mount. Every host action is one fixed tool
of a gatekeeper instance of its own, `gatekeeper-castor`. The design and its
reasons are in the project's `designs/gatekeeper-data-plane.md`; this page is
how it is built and run.

## Who holds what

| | Control plane (`castord`, portal, monitor, bot) | Node role (`castor node`) | `gatekeeper-castor` |
|---|---|---|---|
| Holds | the database, `VOLUME_KEK`, the other secrets | the gatekeeper token | `docker.sock`, the TrueNAS API key |
| Talks to | the node role, over the link | gatekeeper only | the host |
| Refuses to start with | a gatekeeper token | any control-plane secret (`*SECRET*`, `*KEY*`, `*TOKEN*`, `*PASSWORD*`, `*KEK*`, `DATABASE_URL`) | |

All three run from the operator's `docker compose`; Castor starts none of
them. The node role and the control plane are the same `castor` image,
started as `castor node` or as one of the control-plane roles
(`docker-entrypoint.sh`).

## The tools

The catalog is defined once, in `rust/crates/castord/src/gatekeeper/mod.rs`:
compose up/stop/down/rm for `pc-t-<slug>`, the revoke steps with their SQL
and scripts fixed in the tool, the shared services and networks, image
pulls by digest, the tenant and node files, and read-only TrueNAS queries.
Every parameter has a check (slug, e-mail, timestamp, a path below a fixed
root), and gatekeeper gets the same check as a pattern.

- castord maps each docker command line it would run onto the one tool whose
  argv template produces exactly that line, and refuses anything else.
- The node role checks every call against the catalog again (tool known, all
  parameters present and valid, nothing extra, the secret only where the tool
  has one) before it forwards it.
- gatekeeper checks it a third time against Tier 1 and Tier 2.

Generate Tier 2 for your pool:

```sh
docker run --rm "$CASTOR_IMAGE" admin gatekeeper tools tank > tools.yaml
```

`infra/gatekeeper/tools.tank.yaml` is that output for the pool `tank`; a
test fails when it is out of date. `infra/gatekeeper/toolkits.yaml` (Tier 1)
and `identities.yaml` are examples to adapt.

## The link between castord and the node role

TLS 1.3 with client certificates. Each side pins the other's exact
certificate; there is no CA. One request per connection: a tool id, its
arguments, and at most one secret (a file's content, later a dataset
passphrase), which neither side logs. Code is never sent: the revoke SQL is
part of the tool definition.

Make the two key pairs once (Ed25519, ten years):

```sh
openssl req -x509 -newkey ed25519 -nodes -days 3650 -subj /CN=castord \
  -keyout castord.key -out castord.crt
openssl req -x509 -newkey ed25519 -nodes -days 3650 -subj /CN=castor-node \
  -keyout link.key -out link.crt
```

- castord: `LINK_CERT_FILE=castord.crt`, `LINK_KEY_FILE=castord.key` (a Docker
  secret), `LINK_PEERS_FILE` = every node role's `link.crt`, concatenated.
- Node role: `link.crt` and `link.key` as its own, `castord.crt` as its only
  peer (`infra/gatekeeper/compose.yml`).

To rotate, add the new certificate to the peer file first, then switch the
key, then remove the old certificate.

## The beta: everything on one TrueNAS

`infra/beta/docker-compose.yml` runs the control plane, the node role and
`gatekeeper-castor` as one compose project, with `beta.env.example` as the
template for its `.env`. Before the first start, move the TrueNAS web UI off
ports 80 and 443, create `<pool>/castor/node` and `<pool>/castor/t`, set up
gatekeeper's configuration (below), and put the token and the four link
files into `SECRETS_DIR`. Then `docker compose up -d` and register the node
from the castord container (step 4).

## Running a node

1. Create `/mnt/<pool>/castor/node` and `/mnt/<pool>/castor/t` on the pool.
2. Set up `gatekeeper-castor`'s configuration from `infra/gatekeeper/`, with
   your pool's paths, the generated `tools.yaml`, a token for the `castor-node`
   identity, and the NAS's CA as `truenas-ca.pem`. Store the TrueNAS API key
   as the `nas` credential and reload:

   ```sh
   gatekeeper credential-set --name nas --kind ws_api_key < key
   docker kill -s HUP gatekeeper-castor
   ```

   The instance runs `serve --headless`: no console and no admin endpoint.
3. Start `infra/gatekeeper/compose.yml` with a `node.env` (listed at its top).
4. Join castord to the `castor-link` network and register the node:

```sh
castor admin node add nas1 castor-node:7443 --kind truenas --pool tank --auth-host-ip <ip>
```

`node add` probes the pool and the system through the TrueNAS API, then
copies the templates and node files through gatekeeper's file tools.

## Tenant storage

Each tenant is one TrueNAS dataset `<pool>/castor/t/<slug>`, its own
encryption root (AES-256-GCM) with a passphrase: the 64 hex characters of
the key castor-zfs used (HKDF of the tenant's volume key). TrueNAS doesn't
store passphrases, so after a reboot the datasets stay locked until castord
opens them. Each data directory is a child dataset, which inherits the key
and gives the per-service usage. Unlock and lock are middleware jobs; castord
confirms them by querying the dataset.

## Backups

With `BACKUP_TARGET=truenas-replication` the NAS sends the backups itself.
Set up, on the NAS, one replication task to the backup box (a second
TrueNAS):

- source `<pool>/castor/t`, recursive, run manually (no schedule);
- also include snapshots named `cb-%Y%m%dT%H%MZ`;
- encryption: send the datasets raw (the box gets ciphertext, never a key);
- retention on the box: **custom** (for example four weeks), never "same as
  source": castord deletes its older snapshots on the NAS after each run, and
  with "same as source" the box would delete them too.

Generate the tools with the task's id (`castor-admin gatekeeper tools <pool>
<task-id>`). Every `BACKUP_INTERVAL_HOURS` castord snapshots each active
tenant, runs the task, waits for it and records the outcome; then it keeps
only the newest snapshot on the NAS, the base of the next incremental send.
castord can't delete anything on the box: what it keeps is the task's
retention.

The nightly control-plane dump (02:30) is sealed to `BACKUP_DB_PUBLIC_KEY`
and written to `BACKUP_DB_DIR`, the dataset `<pool>/castor/dumps` mounted
into castord (owned by uid 1000). castord runs `pg_dumpall` against
`BACKUP_PG_HOST` and keeps dumps for `BACKUP_DB_KEEP_DAYS` (30), always the
newest. To get them to the box, add a periodic snapshot task on
`<pool>/castor/dumps` (daily, after 02:30) and add that dataset and that
snapshot task to the replication task. `castor-admin backup db-list` and
`db-get` read the directory.

Not yet with replication: restoring from the box (replicate back by hand on
the NAS, then open the tenant) and the weekly verify (scrub the box's pool).

## Not through gatekeeper yet

- The custom-stacks edge (`docker cp` of routes) has no tool: a TrueNAS node
  takes no custom stacks.
- Pulling from GHCR with a token: gatekeeper's stdin is fixed text, so a
  token can't be passed per call, and a TrueNAS node can't use
  `CASTOR_IMAGE_TAG` with a pull token.
- The image keeps `docker-cli` and `openssh-client` while Mac nodes use them.

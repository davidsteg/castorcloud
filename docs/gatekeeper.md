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

## Running a node

1. Create `/mnt/<pool>/castor/node` and `/mnt/<pool>/castor/t` on the pool.
2. Set up `gatekeeper-castor`'s configuration from `infra/gatekeeper/`, with
   your pool's paths, the generated `tools.yaml`, a token for the `castor-node`
   identity, and the TrueNAS API key as its `truenas` credential.
3. Start `infra/gatekeeper/compose.yml` with a `node.env` (listed at its top).
4. Join castord to the `castor-link` network and register the node:

```sh
castor admin node add nas1 castor-node:7443 --kind truenas --pool tank --auth-host-ip <ip>
```

`node add` probes the pool and the system through the TrueNAS API, then
copies the templates and node files through gatekeeper's file tools.

## Not through gatekeeper yet

- Tenant storage (`castor-zfs`) and backups (`castor-snap`) still go over
  SSH and fail on a TrueNAS node. They move to `pool.dataset.*` and TrueNAS
  replication next.
- The custom-stacks edge (`docker cp` of routes) has no tool: a TrueNAS node
  takes no custom stacks.
- Pulling from GHCR with a token needs gatekeeper's stdin support; until
  then a TrueNAS node can't use `CASTOR_IMAGE_TAG` with a pull token.
- The image keeps `docker-cli` and `openssh-client` while Mac nodes use them.

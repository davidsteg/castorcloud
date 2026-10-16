# Custom stacks: customers' own apps

Next to Photos, Documents, Timeline and Flights, a household can run more
apps: one of ours from a reviewed catalog, or its own from Docker images. Castor
Cloud manages everything: encrypted ZFS storage, sign-in in front, routing,
start and stop with the tenant, removal. Billing is by reserved resources
(memory, CPU, storage).

Custom images are code we didn't write. The design assumes a stack is hostile
and still protects other tenants, the tenant's own photos and documents, the
nodes and the control plane.

```
 member's browser ── https://<app>-<slug>.<STACKS_DOMAIN> ──► control plane Traefik
                                                                │ castor-stack-strip: drop X-authentik-* from the client
                                                                │ castor-stack-auth:  Authentik forward auth (tenant group only)
                                                                ▼ stacks-<slug>.yml
 STACKS node (separate machine, no tenant data)        pc-stacks-edge Traefik (no Docker socket)
   Docker with userns-remap, gVisor (runsc)              │ ipAllowList: the control plane only
                                                          ▼ <project>-edge (internal; only the web service)
   pc-s-<slug>--<app>: services on <project> (internal: no way out)
                       castor-egress on <project> + <project>-out: HTTPS/HTTP to listed hosts only
   ZFS: <pool>/s/<slug>        encrypted (key derived from the tenant's volume key), quota = booked storage
        <pool>/s/<slug>/<app>  per-app quota; data/<volume> owned by the container user (userns-mapped)
```

## Decisions (David, 2026-10-16)

| Question | Decision |
|---|---|
| Where stacks come from | Reviewed catalog **and** the customer's own images / compose subset (no builds from repositories yet) |
| Who can open them | Signed-in household members only (Authentik forward auth); not public |
| Where custom images run | **Separate STACKS nodes**, never on a node with tenant data |
| Internet access | None, except host names the stack lists (`x-castor.egress`), through a per-stack proxy |
| Billing | By resources: CHF 0.50 per 256 MB memory, 1.00 per 0.25 vCPU, 0.40 per 10 GB storage (`packages/pricing/src/catalog.ts`) |
| Domain | Own registrable domain (`STACKS_DOMAIN`), see below |

## What a stack may be

A small subset of Docker Compose, parsed and validated by `packages/stacks`
(`spec.ts`). It's an allowlist: any key not listed is refused with a reason,
never ignored. Per service: `image`, `command`, `entrypoint`, `environment`,
`working_dir`, `user`, `tmpfs` (up to 4 paths), `volumes` (named only), `mem_limit` /
`cpus` (or `deploy.resources.limits`; both required), `healthcheck`,
`depends_on`. Top level: `services`, `volumes` (names only), and

```yaml
x-castor:
  web: app:8080            # the one service:port opened in the browser
  storage_gb: 10           # quota of the stack's dataset (needed with volumes)
  egress: [api.example.com, "*.example.org"]   # default: no internet
```

Environment values can use `${secret:name}` (a random value generated once
and kept sealed) and `${castor:origin}` / `${castor:host}`. Every other `$` is
taken literally.

Refused, among others: `privileged`, `cap_add`, `devices`, `network_mode`,
`pid`, `ipc`, `userns_mode`, `security_opt`, `sysctls`, `labels`, `ports`,
`build`, `extra_hosts`, host paths and bind mounts, mounts over `/etc`,
`/proc`, `/sys`, `/dev`, `/usr`, …, our proxy variables, registries outside
the list in `image.ts` (no LAN or localhost registries), YAML over 64 KB, more
than 20 aliases, duplicate keys.

## What we add (render.ts)

Every container: `cap_drop: ALL` plus `CHOWN DAC_OVERRIDE FOWNER SETGID
SETUID NET_BIND_SERVICE KILL`, `no-new-privileges`, memory = swap limit, CPU
limit, `pids_limit: 512`, logs capped at 3 x 10 MB, `runtime: runsc` when the
node has gVisor, image pinned to the digest pulled at deploy, bind mounts only
into the stack's own dataset (`create_host_path: false`), and a read-only
root filesystem with 64 MB tmpfs on `/tmp`, `/run`, `/var/tmp` and the
service's `tmpfs:` paths (`read_only: false` is refused: the writable layer
would sit outside every quota). An image whose `VOLUME` isn't covered by a
volume or tmpfs is refused for the same reason. Old image versions are removed
after a redeploy and with the stack. Networks:
`<project>` internal (no gateway), `<project>-edge` internal (the web service
and the node's edge proxy), `<project>-out` only for the egress proxy.

Images are pulled on the STACKS node, refused above 4 GB, and pinned by
digest; the stack row records what was deployed. Named users (`node`,
`www-data`) are looked up in the image's own `/etc/passwd` without running
it; data directories are owned by that uid plus the node's userns base.

## Isolation, layer by layer

- **Node**: STACKS nodes hold no tenant data (`Node.role`; placement never
  mixes them). Registration refuses a node whose Docker has no userns-remap,
  and needs gVisor unless `--no-gvisor` is passed (audited). Both are checked
  again before every deploy and on every probe; a node that lost either gets
  an error, never a quiet fallback to runc.
- **Tenants on the same node**: separate internal networks; the edge proxy
  reaches each web service only by its alias `web.<project>` on that stack's
  own edge network. Service names can't contain a dot or start with `pc-` /
  `castor-`, and `--` separates slug and stack in every name, so no stack can
  take another's alias, network or router. The edge proxy has no Docker provider, because labels baked into an
  image could otherwise claim another stack's host name.
- **Bypassing sign-in**: the node's edge accepts requests from the control
  plane's address only (ipAllowList on every route), so another container or a
  LAN device can't skip Authentik.
- **Spoofed identity**: the control plane removes `X-authentik-*` headers sent
  by the client, in every `-`/`_` spelling (CGI-style apps read
  `X_authentik_username` as the same header), before forward auth adds the
  real ones.
- **LAN and control plane**: the egress proxy (`infra/node/egress`) resolves
  each name itself, refuses private, loopback, link-local, CGNAT, multicast and
  documentation ranges, and connects to the address it checked (no DNS
  rebinding). Raw IPs are refused; only ports 443 (CONNECT) and 80.
- **Cookies**: stacks are served under `STACKS_DOMAIN`, a registrable domain
  of its own. Under `*.castorcloud.ch` a stack could set cookies for the whole
  domain (cookie tossing) and interfere with portal and Authentik sessions.
  The worker refuses `STACKS_DOMAIN = BASE_DOMAIN` with https. The portal
  opens stacks in a new tab (an embed would need third-party cookies).
- **Data at rest**: one encrypted dataset per tenant on the STACKS node,
  keyed with HKDF(volume key, "castor-zfs-stacks-v1"), so the stacks node never
  sees the key of the tenant's own data, and deleting the tenant's volume key
  shreds both. The definition (it may hold passwords) is sealed in the
  database like other tenant secrets, as the parsed document in JSON
  (`storedDefinition`): castord checks and runs exactly what the portal
  checked, without a YAML parser of its own.

## Lifecycle

- Owner installs (portal, "My apps") → the Stack row is stored sealed, the
  subscription's `stackram` / `stackcpu` / `stackdisk` quantities are set to the
  sum of all stacks (pro rata) → the Stripe webhook stores the booking and
  queues a `STACKS` job → `Stacks.apply` (apps/provisioner/src/stacks.ts):
  places the tenant on a STACKS node (most free memory, no memory overcommit),
  unlocks the dataset, deploys every stack the booking covers (in the order
  added), stops the rest, writes the routes.
- Removing deletes the containers, networks, route, Authentik app, secrets and
  the stack's data right away (the owner confirms "delete for good").
- Suspend stops all stacks and locks the dataset; resume and every tenant
  `UPDATE` queue a `STACKS` job; delete destroys everything before the volume
  key goes.
- After a node reboot the datasets are locked; the node watcher queues a
  `STACKS` job for each affected tenant.

## Set up a STACKS node

1. A separate Mac: `sudo infra/node/macos/bootstrap.sh <disk> "<key>" --stacks`
   (Colima with userns-remap and gVisor, `colima-stacks.yaml`).
2. `npm run node -w @pc/provisioner -- add stacks-1 castor@<mac> --kind mac --role stacks --pool castor --edge <vm-ip> --control-plane-ip <control-plane-ip>`
3. `.env`: `STACKS_DOMAIN`, and in production `STACKS_CERT_RESOLVER=le`; DNS
   `A *.STACKS_DOMAIN` → the control plane (DNS only); `npm run stripe:sync`.

## Not done yet / to verify on hardware

- Backups of stacks datasets (`castor-snap` covers `t/<slug>` only) and usage
  metering on the storage meter.
- Monitoring of stack health (failed stacks are visible to the owner).
- Builds from a customer's GitHub repository (waits for GitHub login), private
  registries.
- Known residual: containers resolve names through Docker's DNS, so a stack
  could leak small amounts of data through DNS queries even without egress.
- Verify on a real Mac: the stacks edge sees the control plane's real source
  address (the ipAllowList depends on it), userns-remap and gVisor under Colima, ownership of
  directories on virtiofs as seen from the remapped containers, Traefik's file
  watch on the routes volume, Authentik's forward auth on a second domain.
- Catalog apps: IT Tools, linkding (signs in via Authentik's header), Actual
  Budget. Check each under gVisor before offering them.

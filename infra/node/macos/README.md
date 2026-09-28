# Mac mini nodes

Tenant stacks run on Mac minis (Apple Silicon). Each Mac is an independent
Docker host; the control plane drives it over SSH. There's no cluster: a
tenant's data lives on one Mac, so its containers run there.

```
 control plane (provisioner)                    Mac mini
 ───────────────────────────                    ─────────────────────────────────────────
 docker compose ── ssh:// (castor-ssh) ───────► Colima VM: Docker engine
                                                 ├ pc-edge: Traefik, *.domain via DNS-01
                                                 ├ pc-node: immich-ml, tika, gotenberg
                                                 └ pc-t-<slug>: the tenant's stack
 castor-zfs ────── ssh (castor-ssh, sudo) ────► macOS: OpenZFS pool "castor"
   key on stdin                                  └ castor/t/<slug>  aes-256-gcm, quota
                                                    mounted at /Volumes/castor/t/<slug>,
                                                    shared into the VM at the same path
```

| Piece | Choice | Why |
|---|---|---|
| Storage | [OpenZFS on OS X](https://openzfsonosx.org) | Per-tenant native encryption, instant quotas (grow and shrink), snapshots and `zfs send` for backups. |
| Docker | [Colima](https://github.com/abiosoft/colima) (Apple Virtualization, virtiofs) | Free and scriptable. It runs headless from a LaunchDaemon and can give the VM its own LAN address. Docker Desktop and OrbStack can't be run unattended like that. Any Docker on macOS runs in a Linux VM; Colima's is small and invisible. |
| Access | SSH forced command (`castor-ssh`) | The provisioner's key can run exactly: Docker's `dial-stdio`, `castor-zfs`, and writing tenant config files. |

Keys: each dataset's key is derived from the tenant's volume key, which is
wrapped in the control-plane database. The key never touches the Mac's disk.
After a reboot every dataset is locked. The provisioner's node watcher (every
5 minutes) sees that and re-applies the stack, which unlocks it. Deleting a
tenant destroys the dataset and the key (crypto-shredding).

## Set up a Mac

1. **OpenZFS.** Install the Apple Silicon build from openzfsonosx.org. macOS
   only loads its kernel extension with reduced security. Hold the power button
   to boot into Recovery, open Startup Security Utility and choose *Reduced
   Security* with *Allow user management of kernel extensions*. Reboot, then
   approve the extension in System Settings under Privacy & Security, and
   reboot again. Check with `zpool version`.
2. **Homebrew**, from brew.sh.
3. **FileVault off.** Otherwise the Mac waits for a password after a power
   cut and never comes back on its own. Tenant data is encrypted by ZFS with
   keys that never leave the control plane.
4. **Run the bootstrap** with the pool disk and the provisioner's public key:
   ```sh
   sudo infra/node/macos/bootstrap.sh disk4 "ssh-ed25519 AAAA… provisioner"
   ```
   It creates the `castor` user, the pool, the SSH gate, and Colima as a boot
   service. Colima's first start may ask for your password once (bridged
   networking).
5. **Router**: give the Colima VM a fixed DHCP lease and forward 80/443 to it.

## Register it

On the control plane (`~/.ssh/config` needs the key for the Mac):

```sh
npm run node -w @pc/provisioner -- add mini-1 castor@10.0.0.21 --auth-host-ip <control-plane-ip>
npm run node -w @pc/provisioner -- list
```

`add` checks the pool and Docker, copies the templates, brand assets and CA
bundle to the Mac, and starts the edge proxy. New tenants are placed on the
node with the most free booked space (`apps/provisioner/src/placement.ts`).
`drain <name>` stops new placements.

The edge proxy needs the DNS provider's API credentials for the wildcard
certificate, in `.env.edge` at the repo root on the control plane:

```sh
ACME_EMAIL=ops@example.ch
ACME_DNS_PROVIDER=cloudflare     # any lego provider name
CF_DNS_API_TOKEN=…               # that provider's variables
```

## Not verified yet on real hardware

This was written without a Mac mini to test on. Check these first:

- Postgres data directories on virtiofs: chown to the container user, and fsync behaviour.
- Datasets mounted after Colima started show up inside the VM (nested mounts under the shared `/Volumes/castor`).
- Colima bridged networking started from the LaunchDaemon, without a login session.
- OpenZFS on OS X reads the dataset key from stdin for `zfs create` and `zfs load-key` (keyformat=hex).

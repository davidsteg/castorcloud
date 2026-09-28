# TrueNAS SCALE nodes

The proof of concept runs tenant stacks on TrueNAS SCALE. Mac minis
(`../macos/README.md`) follow later. Both are the same kind of node to the
control plane: a Docker host with a ZFS pool, reached over SSH through the
same gate (`../bin/castor-ssh`) and the same dataset helper
(`../bin/castor-zfs`). Only the paths differ, and they are kept in
`apps/provisioner/src/layout.ts` and the node's `castor.conf`.

```
 control plane (provisioner)                    TrueNAS SCALE
 ───────────────────────────                    ────────────────────────────────────────────
 docker compose ── ssh:// (castor-ssh, sudo) ──► system Docker engine (Apps must be enabled)
                                                 ├ pc-edge: Traefik on :8480
                                                 ├ pc-node: immich-ml, tika, gotenberg, LLM gateway
                                                 └ pc-t-<slug>: the tenant's stack
 castor-zfs ────── ssh (castor-ssh, sudo) ─────► <pool>/castor/t/<slug>   aes-256-gcm, quota
   key on stdin                                   mounted at /mnt/<pool>/castor/t/<slug>
```

| | TrueNAS SCALE | Mac mini |
|---|---|---|
| ZFS | native (OpenZFS on Linux) | OpenZFS on OS X (kernel extension) |
| Docker | the system engine, no VM | Colima VM |
| Tenant datasets | `<pool>/castor/t/<slug>` at `/mnt/<pool>/castor/t/<slug>` | `castor/t/<slug>` at `/Volumes/castor/t/<slug>` |
| Scripts and config | `/mnt/<pool>/castor/bin` | `/usr/local/castor/bin` |
| Templates and tenant files | `/mnt/<pool>/castor/node` | `/Users/Shared/castor` |
| Edge proxy | NAS address, port 8480 | Colima VM address, port 80 |
| Setup | `setup.sh` (uses `midclt`) | `bootstrap.sh` |

## Why SSH and not the TrueNAS API

- **Least privilege.** A TrueNAS API key is full admin. The SSH key can only
  do what `castor-ssh` allows: Docker's `dial-stdio`, `castor-zfs`, and writing
  tenant config files. Docker access is root-equivalent on any host, so that
  part is the same either way.
- **Keys stay off the NAS.** The middleware's encrypted datasets either keep
  their key in the TrueNAS database, or take a passphrase that `midclt` would
  put on a command line. `castor-zfs` creates the dataset with
  `keylocation=prompt` and gets the key on stdin. TrueNAS never stores it, and
  after a reboot the datasets stay locked until the provisioner unlocks them.
  The node watcher does that within 5 minutes.
- **One code path.** The provisioner treats TrueNAS and the Macs the same.

TrueNAS shows the tenant datasets in its UI (as locked after a reboot). Leave
them alone there: don't unlock, move or delete them from the UI. The
containers don't appear under Apps, which lists only apps installed through
the UI.

## Set up the NAS

1. **Apps:** in Apps > Configuration > Choose Pool, pick the pool that
   Docker should use. Apps must show *Running*. This is what starts Docker on
   TrueNAS.
2. **Run the setup** as root in the TrueNAS shell, with the pool for tenant
   data and the provisioner's public key:
   ```sh
   sh infra/node/truenas/setup.sh tank "ssh-ed25519 AAAA… provisioner"
   ```
   It creates `tank/castor` and `tank/castor/t` through the middleware, and
   puts the helpers and a root-owned `castor.conf` in `/mnt/tank/castor/bin`.
   It also creates the `castor` user through the middleware, with the SSH
   key restricted to `castor-ssh` and password-less sudo for only
   `castor-zfs` and `docker`. Finally it enables SSH and prints the
   registration command.
3. **Register it** from the control plane with the printed command:
   ```sh
   npm run node -w @pc/provisioner -- add nas-1 castor@10.0.0.5 --kind truenas --pool tank --edge 10.0.0.5:8480 --auth-host-ip <control-plane-ip>
   ```

Routing and DNS work as for the Macs: `*.castorcloud.ch` points at the
control plane, which forwards each tenant's service hosts to
`http://<nas>:8480` (see `../macos/README.md`, *Routing and DNS*).

## Not verified yet on a real TrueNAS

This was written without a TrueNAS system to test on. The helpers ran
against stubs. Check these on the first run:

- The user's SSH key field accepts the `command="…",restrict` prefix. The setup
  script warns if `authorized_keys` doesn't contain it.
- The `user.create` fields (`sudo_commands_nopasswd`, `password_disabled`,
  `group_create`) match the installed TrueNAS version.
- `zfs create` / `zfs load-key` from the CLI next to the middleware: datasets
  stay locked after a reboot and don't upset the UI.
- Containers started with `docker compose` outside the Apps UI survive a
  TrueNAS update.

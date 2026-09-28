# The backup box

A machine with a ZFS pool that receives raw, still-encrypted tenant backups
(see [docs/backups.md](../../docs/backups.md)). It never holds a tenant key.
TrueNAS SCALE is the reference; any Linux with OpenZFS 2.x works the same.
Put it in a different room from the nodes now, and in a different place
later (it's only useful if a fire or theft at the nodes doesn't take it too).

The provisioner reaches it over SSH as the `castor-backup` user, whose key
can run exactly one thing: `castor-backup` (a forced command). That script
re-runs itself as root through sudo for the `zfs` calls, and refuses to
delete anything younger than `MIN_DAYS` (default 7) or a tenant's newest
backup, whatever it's asked.

## TrueNAS SCALE

1. **Pool.** Create a pool for backups (Storage > Create Pool), e.g. `bpool`.
   Mirror or RAIDZ: this is the copy of last resort.
2. **Script.** Create a dataset `bpool/castor-backup-bin` and copy
   `castor-backup` into it, owned by root and **not writable by anyone else**
   (the backup user may run it as root through sudo):
   ```sh
   install -o root -g root -m 755 castor-backup /mnt/bpool/castor-backup-bin/
   chmod 755 /mnt/bpool/castor-backup-bin
   ```
3. **Datasets.** In the TrueNAS shell, as root:
   ```sh
   /mnt/bpool/castor-backup-bin/castor-backup init bpool
   ```
   It writes `castor-backup.conf` next to the script (on the pool, so TrueNAS
   updates keep it) and creates `bpool/castor-backup/t` (tenant backups) and
   `bpool/castor-backup/db` (database dumps). To change the floor, add
   `MIN_DAYS=14` to that file; `DB_DAYS` (default 30) is how long dumps stay.
4. **User.** Credentials > Users > Add: username `castor-backup`, no password
   (disable password login), shell `sh`, home under the pool. Under *Allowed
   sudo commands with no password* enter
   `/mnt/bpool/castor-backup-bin/castor-backup`. In *Authorized Keys* paste
   the provisioner's public key with the forced command in front:
   ```
   command="/mnt/bpool/castor-backup-bin/castor-backup",restrict ssh-ed25519 AAAA… provisioner
   ```
5. **SSH.** System > Services: enable SSH. No password logins.
6. **Scrub.** Data Protection > Scrub Tasks: monthly for `bpool`.
7. **Check** from the control plane:
   ```sh
   ssh castor-backup@<box> castor-backup pool     # pool <size> <used> ONLINE
   ssh castor-backup@<box> id                     # must fail: castor-backup: bad argument / unknown command
   ```

Then set `BACKUP_TARGET=castor-backup@<box>` in the control plane's `.env`.

## Plain Linux with OpenZFS

The same steps by hand: install the script as root-owned in a root-owned
directory, run `castor-backup init <pool>`, create a `castor-backup` user with
a sudoers line `castor-backup ALL=(root) NOPASSWD: /path/castor-backup`, and
put the `command="…",restrict` line in its `authorized_keys`.

## What it never does

- Mount a tenant backup, or load a key: received datasets stay `canmount=off`
  and locked.
- Receive with `-F`: incoming streams can't roll back or replace history.
- Delete recent backups on request: `prune` and `forget` check the age on the
  box itself.

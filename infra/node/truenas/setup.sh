#!/bin/sh
# Turns a TrueNAS SCALE system into a Castor Cloud node. Run once as root in
# the TrueNAS shell (System > Shell, or ssh as admin + sudo -i), from a copy
# of this repository's infra/node directory:
#
#   sh infra/node/truenas/setup.sh <pool> "<provisioner ssh public key>"
#
# Everything lives on the pool, because TrueNAS resets its system disk on
# every update: <pool>/castor (scripts, templates, the castor user's home)
# and <pool>/castor/t (one encrypted dataset per tenant). The castor user,
# its SSH key and its sudo rights go through the TrueNAS middleware (midclt),
# so they survive updates too. Safe to re-run.
set -eu
pool=${1:?usage: setup.sh <pool> "<ssh public key>"}
pubkey=${2:?usage: setup.sh <pool> "<ssh public key>"}
HERE=$(cd "$(dirname "$0")" && pwd)
BASE=/mnt/$pool/castor
BIN=$BASE/bin
USER_NAME=castor
EDGE_PORT=8480

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
command -v midclt >/dev/null || { echo "not a TrueNAS SCALE system (no midclt)" >&2; exit 1; }
case "$pool" in ""|*[!A-Za-z0-9_-]*) echo "invalid pool name" >&2; exit 1;; esac
case "$pubkey" in ssh-*|ecdsa-*) ;; *) echo "that doesn't look like an SSH public key" >&2; exit 1;; esac
zpool list -H "$pool" >/dev/null 2>&1 || { echo "no pool named $pool" >&2; exit 1; }

step() { printf '\n==> %s\n' "$*"; }
# JSON for midclt from a Python literal (only fixed strings and checked names).
json() { python3 -c 'import ast, json, sys; print(json.dumps(ast.literal_eval(sys.argv[1])))' "$1"; }

step "Docker (TrueNAS Apps)"
status=$(midclt call docker.status | python3 -c 'import json, sys; print(json.load(sys.stdin).get("status", ""))')
if [ "$status" != RUNNING ]; then
  echo "Docker isn't running ($status). In the TrueNAS UI open Apps > Configuration >" >&2
  echo "Choose Pool, pick a pool, wait until Apps shows 'Running', then re-run this script." >&2
  exit 1
fi
echo "running"

step "datasets $pool/castor and $pool/castor/t"
for ds in "$pool/castor" "$pool/castor/t"; do
  if zfs list -H "$ds" >/dev/null 2>&1; then echo "$ds exists"
  else midclt call pool.dataset.create "$(json "{'name': '$ds', 'type': 'FILESYSTEM'}")" >/dev/null; echo "$ds created"; fi
done

step "castor-zfs and the SSH gate in $BIN"
mkdir -p "$BIN" "$BASE/node" "$BASE/home"
install -o root -g root -m 755 "$HERE/../bin/castor-zfs" "$HERE/../bin/castor-ssh" "$BIN/"
# Backups (snapshots, send/receive), once that helper is in the repo.
[ ! -f "$HERE/../bin/castor-snap" ] || install -o root -g root -m 755 "$HERE/../bin/castor-snap" "$BIN/"
# Root-owned: the castor user can't change where the helpers point.
cat > "$BIN/castor.conf" <<CONF
# Written by infra/node/truenas/setup.sh. Read by castor-ssh and castor-zfs.
POOL=$pool
DATASET_ROOT=$pool/castor/t
MOUNT_ROOT=$BASE/t
ROOT=$BASE/node
DOCKER="sudo -n /usr/bin/docker"
DOCKER_HOST=
CONF
chown root:root "$BIN" "$BIN/castor.conf"
chmod 755 "$BIN"; chmod 644 "$BIN/castor.conf"

step "user $USER_NAME (SSH key restricted to the gate, sudo for the helpers and docker)"
# Values go to python as arguments, never into code, so a key comment with
# quotes can't break anything.
user=$(python3 - "$BIN" "$BASE" "$pubkey" <<'PY'
import json, sys
bin_dir, base, pubkey = sys.argv[1:]
print(json.dumps({
    "full_name": "Castor Cloud provisioner",
    "home": f"{base}/home",
    "shell": "/usr/bin/bash",  # castor-ssh runs as a forced command; nothing else is reachable
    "password_disabled": True,
    "sshpubkey": f'command="{bin_dir}/castor-ssh",restrict {pubkey}',
    "sudo_commands_nopasswd": [f"{bin_dir}/castor-zfs", f"{bin_dir}/castor-snap", "/usr/bin/docker"],
}))
PY
)
id=$(midclt call user.query "$(json "[['username', '=', '$USER_NAME']]")" | python3 -c 'import json, sys; u = json.load(sys.stdin); print(u[0]["id"] if u else "")')
if [ -z "$id" ]; then
  create=$(printf '%s' "$user" | python3 -c 'import json, sys; u = json.load(sys.stdin); u.update(username=sys.argv[1], group_create=True); print(json.dumps(u))' "$USER_NAME")
  midclt call user.create "$create" >/dev/null
  echo "created"
else
  midclt call user.update "$id" "$user" >/dev/null
  echo "updated"
fi
chown -R "$USER_NAME:$USER_NAME" "$BASE/node"
chmod 700 "$BASE/node"
if ! grep -q "command=\"$BIN/castor-ssh\"" "$BASE/home/.ssh/authorized_keys" 2>/dev/null; then
  echo "WARNING: $BASE/home/.ssh/authorized_keys doesn't restrict the key to castor-ssh." >&2
  echo "Check the user's SSH key field in Credentials > Users before registering the node." >&2
fi

step "SSH service"
midclt call service.update ssh "$(json "{'enable': True}")" >/dev/null 2>&1 || true
midclt call service.start ssh >/dev/null 2>&1 || true
midclt call service.query "$(json "[['service', '=', 'ssh']]")" | python3 -c 'import json, sys; s = json.load(sys.stdin); print("ssh:", s[0]["state"] if s else "unknown")'

ip=$(midclt call interface.ip_in_use | python3 -c 'import json, sys; print(next((a["address"] for a in json.load(sys.stdin) if a["type"] == "INET" and not a["address"].startswith("127.")), ""))')
cat <<DONE

Node ready. On the control plane:
  npm run node -w @pc/provisioner -- add <name> $USER_NAME@${ip:-<nas-ip>} --kind truenas --pool $pool --edge ${ip:-<nas-ip>}:$EDGE_PORT --auth-host-ip <control-plane-ip>
DONE

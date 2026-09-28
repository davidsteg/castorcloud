#!/bin/sh
# Turns a Mac mini into a Castor Cloud node. Run once, as an admin user, on
# the Mac itself:
#
#   sudo infra/node/macos/bootstrap.sh <disk> "<provisioner ssh public key>"
#
#   <disk>  the SSD (or partition) for the ZFS pool, e.g. disk4 or disk0s3.
#           Everything on it is erased. `diskutil list` shows the candidates.
#
# Before: OpenZFS on OS X and Homebrew are installed (see README.md).
# After:  register the node from the control plane with `npm run node -- add`.
# Safe to re-run: every step checks what's already there.
set -eu
POOL=castor
USER_NAME=castor
BIN=/usr/local/castor/bin
ROOT=/Users/Shared/castor
HERE=$(cd "$(dirname "$0")" && pwd)
ZPOOL=/usr/local/zfs/bin/zpool
ZFS=/usr/local/zfs/bin/zfs
BREW=/opt/homebrew/bin/brew

disk=${1:?usage: bootstrap.sh <disk> "<ssh public key>"}
pubkey=${2:?usage: bootstrap.sh <disk> "<ssh public key>"}
[ "$(id -u)" = 0 ] || { echo "run with sudo" >&2; exit 1; }
[ "$(uname -m)" = arm64 ] || { echo "expects an Apple Silicon Mac" >&2; exit 1; }
[ -x "$ZPOOL" ] || { echo "OpenZFS is not installed (README.md, step 1)" >&2; exit 1; }
[ -x "$BREW" ] || { echo "Homebrew is not installed (README.md, step 2)" >&2; exit 1; }
admin=${SUDO_USER:?run with sudo from an admin account}

step() { printf '\n==> %s\n' "$*"; }

step "power: never sleep, restart after power loss"
pmset -a sleep 0 disksleep 0 autorestart 1 womp 1

step "castor user"
if ! id "$USER_NAME" >/dev/null 2>&1; then
  sysadminctl -addUser "$USER_NAME" -fullName "Castor Cloud" -shell /bin/zsh \
    -password "$(openssl rand -base64 32)" -home "/Users/$USER_NAME"
  createhomedir -c -u "$USER_NAME" >/dev/null
fi
systemsetup -setremotelogin on >/dev/null 2>&1 || true
dseditgroup -o edit -a "$USER_NAME" -t user com.apple.access_ssh 2>/dev/null || true

step "ZFS pool '$POOL' on $disk"
if "$ZPOOL" list "$POOL" >/dev/null 2>&1; then
  echo "exists"
else
  diskutil info "$disk" | grep -E 'Device Node|Media Name|Disk Size' || { echo "no such disk: $disk" >&2; exit 1; }
  printf 'ERASE %s and create the pool on it? Type the disk name to confirm: ' "$disk"
  read -r answer
  [ "$answer" = "$disk" ] || { echo "aborted"; exit 1; }
  diskutil unmountDisk "$disk" >/dev/null 2>&1 || true
  # casesensitivity: Linux containers expect it; macOS defaults to insensitive.
  "$ZPOOL" create -f -o ashift=12 \
    -O compression=zstd -O atime=off -O casesensitivity=sensitive -O normalization=formD \
    -O com.apple.browse=off -O mountpoint="/Volumes/$POOL" "$POOL" "$disk"
fi
"$ZFS" list "$POOL/t" >/dev/null 2>&1 || "$ZFS" create "$POOL/t"
mdutil -i off "/Volumes/$POOL" >/dev/null 2>&1 || true   # no Spotlight on tenant data

step "castor-zfs and the SSH gate"
mkdir -p "$BIN"
install -o root -g wheel -m 755 "$HERE/../bin/castor-zfs" "$HERE/../bin/castor-ssh" "$BIN/"
printf '%s ALL=(root) NOPASSWD: %s/castor-zfs\n' "$USER_NAME" "$BIN" > /etc/sudoers.d/castor
chmod 440 /etc/sudoers.d/castor
visudo -cf /etc/sudoers.d/castor >/dev/null
home=/Users/$USER_NAME
mkdir -p "$home/.ssh" "$ROOT" "$home/Library/Logs"
printf 'command="%s/castor-ssh",restrict %s\n' "$BIN" "$pubkey" > "$home/.ssh/authorized_keys"
chmod 700 "$home/.ssh" "$ROOT"
chmod 600 "$home/.ssh/authorized_keys"
chown -R "$USER_NAME":staff "$home/.ssh" "$ROOT" "$home/Library/Logs"

step "Colima and the Docker CLI"
sudo -u "$admin" "$BREW" install colima docker >/dev/null
cpus=$(sysctl -n hw.ncpu)
mem_gb=$(( $(sysctl -n hw.memsize) / 1073741824 ))
# macOS, the ZFS ARC and the page cache keep a quarter (at least 6 GB).
vm_mem=$(( mem_gb - (mem_gb / 4 > 6 ? mem_gb / 4 : 6) ))
vm_cpu=$(( cpus > 2 ? cpus - 1 : cpus ))
mkdir -p "$home/.colima/default"
sed -e "s/__CPU__/$vm_cpu/" -e "s/__MEMORY_GB__/$vm_mem/" -e "s/__POOL__/$POOL/" \
  "$HERE/colima.yaml" > "$home/.colima/default/colima.yaml"
chown -R "$USER_NAME":staff "$home/.colima"
# Cap the ZFS ARC (an eighth of RAM) so it doesn't compete with the VM.
arc="kstat.zfs.darwin.tunable.zfs_arc_max=$(( (mem_gb / 8) * 1073741824 ))"
mkdir -p /etc/zfs
grep -q zfs_arc_max /etc/zfs/zsysctl.conf 2>/dev/null || echo "$arc" >> /etc/zfs/zsysctl.conf
sysctl -w "$arc" >/dev/null 2>&1 || true

step "start Colima at boot (as $USER_NAME, no login needed)"
install -o root -g wheel -m 644 "$HERE/ch.castorcloud.colima.plist" /Library/LaunchDaemons/
launchctl bootout system/ch.castorcloud.colima 2>/dev/null || true
launchctl bootstrap system /Library/LaunchDaemons/ch.castorcloud.colima.plist
printf 'waiting for Docker'
i=0
until sudo -u "$USER_NAME" env DOCKER_HOST="unix://$home/.colima/default/docker.sock" /opt/homebrew/bin/docker info >/dev/null 2>&1; do
  i=$((i + 1)); [ $i -gt 60 ] && { echo; echo "Docker didn't come up: see $home/Library/Logs/colima.log" >&2; exit 1; }
  printf '.'; sleep 5
done
echo " up"
vm_ip=$(sudo -u "$USER_NAME" env HOME="$home" /opt/homebrew/bin/colima ls -j 2>/dev/null | sed -n 's/.*"address":"\([^"]*\)".*/\1/p' | head -n1)

cat <<DONE

Node ready. Give the Docker VM (${vm_ip:-see 'colima ls'}) a fixed DHCP lease, then on the control plane:
  npm run node -w @pc/provisioner -- add <name> $USER_NAME@$(ipconfig getifaddr en0 || hostname) --kind mac --pool $POOL --edge ${vm_ip:-<vm-ip>} --auth-host-ip <control-plane-ip>
DONE

#!/bin/sh
# Per-tenant encrypted volumes (LUKS2, AES-XTS-512).
#
#   cryptd create  <slug> <size-mb> <dirs...>  key on stdin; creates, opens, mounts
#   cryptd open    <slug>                      key on stdin; opens + mounts (idempotent)
#   cryptd grow    <slug> <size-mb>            key on stdin; online resize
#   cryptd close   <slug>                      unmounts + locks
#   cryptd destroy <slug>                      closes, wipes header, deletes file
#   cryptd status  <slug>                      prints "open <used-mb> <size-mb>" or "closed"/"missing"
#
# Layout: /srv/pc/volumes/<slug>.luks  ->  mounted at /srv/pc/tenants/<slug>
# The key is read from stdin into a tmpfs file that exists only inside this
# short-lived container, and is removed before exit.
set -eu

ROOT=/srv/pc
cmd=${1:?command}; slug=${2:?slug}
case "$slug" in *[!a-z0-9-]*) echo "invalid slug" >&2; exit 2;; esac

img="$ROOT/volumes/$slug.luks"
mnt="$ROOT/tenants/$slug"
map="pc-$slug"
keyfile=/dev/shm/key

read_key() {
  umask 077
  cat > "$keyfile"
  trap 'rm -f "$keyfile"' EXIT
  [ -s "$keyfile" ] || { echo "no key on stdin" >&2; exit 2; }
}

# Each run is a fresh container: create /dev/mapper nodes for devices that
# earlier runs opened (they live in the host kernel, not in this /dev).
dmsetup mknodes >/dev/null 2>&1 || true

is_open() { dmsetup info "$map" >/dev/null 2>&1; }
is_mounted() { mountpoint -q "$mnt"; }

do_open() {
  is_open || cryptsetup open --type luks2 --key-file "$keyfile" "$img" "$map"
  mkdir -p "$mnt"
  is_mounted || mount -o noatime,nodev,nosuid "/dev/mapper/$map" "$mnt"
}

case "$cmd" in
  create)
    size=${3:?size-mb}; shift 3
    read_key
    mkdir -p "$ROOT/volumes"
    if [ ! -e "$img" ]; then
      truncate -s "${size}M" "$img"   # sparse: only used blocks take disk space
      cryptsetup luksFormat --type luks2 --batch-mode --cipher aes-xts-plain64 --key-size 512 \
        --pbkdf pbkdf2 --key-file "$keyfile" "$img"
      cryptsetup open --type luks2 --key-file "$keyfile" "$img" "$map"
      mkfs.ext4 -q -m 0 -L "$slug" "/dev/mapper/$map"
    fi
    do_open
    for d in "$@"; do mkdir -p "$mnt/$d"; done
    echo "open"
    ;;
  open)
    read_key
    [ -e "$img" ] || { echo "missing"; exit 3; }
    do_open
    echo "open"
    ;;
  grow)
    size=${3:?size-mb}
    read_key
    cur=$(( $(stat -c %s "$img") / 1048576 ))
    if [ "$size" -gt "$cur" ]; then
      truncate -s "${size}M" "$img"
      loop=$(losetup -j "$img" | cut -d: -f1 | head -n1)
      [ -n "$loop" ] && losetup -c "$loop"
      cryptsetup resize --key-file "$keyfile" "$map"
      resize2fs "/dev/mapper/$map" >/dev/null 2>&1
    fi
    echo "size ${size}"
    ;;
  close)
    if is_mounted; then umount "$mnt"; fi
    if is_open; then cryptsetup close "$map"; fi
    echo "closed"
    ;;
  destroy)
    if is_mounted; then umount "$mnt"; fi
    if is_open; then cryptsetup close "$map"; fi
    if [ -e "$img" ]; then
      cryptsetup erase --batch-mode "$img" || true   # wipe all key slots
      rm -f "$img"
    fi
    rmdir "$mnt" 2>/dev/null || true
    echo "destroyed"
    ;;
  status)
    if [ ! -e "$img" ]; then echo "missing"
    elif is_mounted; then
      df -Pm "$mnt" | awk 'NR==2 { print "open", $3, $2 }'  # -P: no line wrapping
    else echo "closed"; fi
    ;;
  *) echo "unknown command: $cmd" >&2; exit 2;;
esac

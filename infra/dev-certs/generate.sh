#!/usr/bin/env bash
# Local HTTPS for development. Creates (in .dev-certs/, gitignored):
#   ca.crt / ca.key         - a private development CA (valid 10 years)
#   wildcard.crt / .key     - cert for <domain>, *.<domain> and *.<slug>.<domain>
#   ca-bundle.pem           - public roots + the dev CA, mounted into containers
#
# Usage: infra/dev-certs/generate.sh [domain] [tenant-slug ...]
# Re-run with more slugs when you add dev tenants; the CA is kept.
set -euo pipefail
# Git Bash on Windows would rewrite "/CN=..." into a file path.
export MSYS_NO_PATHCONV=1

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="$root/.dev-certs"
domain="${1:-brand.localhost}"
shift || true
slugs=("${@:-demo}")
mkdir -p "$out"
cd "$out"

if [ ! -f ca.key ]; then
  openssl req -x509 -newkey rsa:3072 -nodes -days 3650 \
    -keyout ca.key -out ca.crt -subj "/CN=Personal Cloud Dev CA" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
  echo "created dev CA: $out/ca.crt"
fi

san="DNS:$domain,DNS:*.$domain"
for s in "${slugs[@]}"; do san="$san,DNS:*.$s.$domain"; done

printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth\nkeyUsage=digitalSignature,keyEncipherment\n' "$san" > wildcard.ext
openssl req -newkey rsa:2048 -nodes -keyout wildcard.key -out wildcard.csr \
  -subj "/CN=*.$domain" 2>/dev/null
openssl x509 -req -in wildcard.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -days 825 -out wildcard.crt -extfile wildcard.ext 2>/dev/null
rm -f wildcard.csr wildcard.ext

# Containers replace their trust store with this bundle (public roots + dev CA).
system_bundle=""
for f in /etc/ssl/certs/ca-certificates.crt /mingw64/etc/ssl/certs/ca-bundle.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/cert.pem; do
  [ -f "$f" ] && { system_bundle="$f"; break; }
done
{ [ -n "$system_bundle" ] && cat "$system_bundle"; cat ca.crt; } > ca-bundle.pem

echo "certificate for: $san"

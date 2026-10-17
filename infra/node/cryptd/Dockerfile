# Short-lived helper that manages per-tenant LUKS2 volumes on a node.
# Run privileged with /srv/pc mounted rshared; keys arrive on stdin only.
FROM alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8
RUN apk add --no-cache cryptsetup device-mapper e2fsprogs e2fsprogs-extra util-linux
COPY cryptd.sh /usr/local/bin/cryptd
RUN chmod +x /usr/local/bin/cryptd
ENTRYPOINT ["/usr/local/bin/cryptd"]

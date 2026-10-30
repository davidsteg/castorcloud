# Short-lived helper that manages per-tenant LUKS2 volumes on a node.
# Run privileged with /srv/pc mounted rshared; keys arrive on stdin only.
FROM alpine:3.24@sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a
RUN apk add --no-cache cryptsetup device-mapper e2fsprogs e2fsprogs-extra util-linux
COPY cryptd.sh /usr/local/bin/cryptd
RUN chmod +x /usr/local/bin/cryptd
ENTRYPOINT ["/usr/local/bin/cryptd"]

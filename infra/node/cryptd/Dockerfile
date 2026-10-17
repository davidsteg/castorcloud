# Short-lived helper that manages per-tenant LUKS2 volumes on a node.
# Run privileged with /srv/pc mounted rshared; keys arrive on stdin only.
FROM alpine:3.24@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
RUN apk add --no-cache cryptsetup device-mapper e2fsprogs e2fsprogs-extra util-linux
COPY cryptd.sh /usr/local/bin/cryptd
RUN chmod +x /usr/local/bin/cryptd
ENTRYPOINT ["/usr/local/bin/cryptd"]

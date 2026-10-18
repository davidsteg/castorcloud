# TrueNAS SCALE nodes

A TrueNAS node is reached only through gatekeeper: no SSH, no key on the
control plane. castord asks the node role (`castor node`, from the castor
image) over mutual TLS for one tool of a fixed catalog at a time; the node
role holds the only gatekeeper token, and `gatekeeper-castor` holds the
Docker socket and the TrueNAS API key.

```
 control plane                 TrueNAS SCALE
 ─────────────                 ─────────────────────────────────────────────────
 castord ── mTLS :7443 ──► castor node ── token ──► gatekeeper-castor
                                                      ├ docker: pc-edge, pc-node, pc-t-<slug>
                                                      ├ files under /mnt/<pool>/castor/node
                                                      └ TrueNAS API: <pool>/castor/t/<slug>
                                                        (aes-256-gcm, quota), snapshots,
                                                        replication to the backup box
```

Setup, the tool catalog, storage, backups and the one-box beta:
[docs/gatekeeper.md](../../../docs/gatekeeper.md). Compose files:
`infra/gatekeeper/` (node only) and `infra/beta/` (everything on one NAS).

The earlier SSH setup (`setup.sh`, a `castor` user with a forced command)
is gone; Mac nodes still use the SSH gate (`../macos/README.md`).

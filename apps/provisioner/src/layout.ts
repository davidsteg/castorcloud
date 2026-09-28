// Remote node platforms and where things live on them.

export type RemoteKind = "MAC" | "TRUENAS";

/**
 * Where things live on a remote node. Must match what the node's setup
 * script writes into castor.conf (infra/node/truenas/setup.sh) or the macOS
 * defaults in infra/node/bin/castor-*.
 *   root       templates and tenant config files (castor-ssh's ROOT)
 *   mountRoot  where tenant datasets are mounted (castor-zfs's MOUNT_ROOT)
 */
export function nodeLayout(kind: RemoteKind, pool: string) {
  return kind === "TRUENAS"
    ? { root: `/mnt/${pool}/castor/node`, mountRoot: `/mnt/${pool}/castor/t` }
    // macOS: both are mounted into Colima's VM at the same paths.
    : { root: "/Users/Shared/castor", mountRoot: `/Volumes/${pool}/t` };
}

/** "10.0.0.5:8480" -> where the control plane reaches the node's edge proxy. */
export function parseEdge(edgeHost: string) {
  const m = edgeHost.match(/^([A-Za-z0-9.-]+)(?::(\d{1,5}))?$/);
  const port = m?.[2] ? Number(m[2]) : 80;
  if (!m || port < 1 || port > 65535) throw new Error(`invalid edge host: ${edgeHost}`);
  return { host: m[1], port, url: `http://${m[1]}${port === 80 ? "" : `:${port}`}` };
}

// Which node a new tenant goes to. A tenant's data lives on one node's pool,
// so this is decided once, at provisioning; moving a tenant later is a
// separate (zfs send/receive) operation.
import { stackMemoryMb, stackStorageMb, storageQuotaMb, type StackUnits } from "@pc/pricing";

/** Quotas are limits, not usage: sell up to this multiple of the pool. */
export const POOL_OVERCOMMIT = 2;
/** Never place on a pool that is already this full, whatever the quotas say. */
export const POOL_MAX_USED = 0.8;
/** Idle RAM of one tenant stack (shared ML/tika/gotenberg not included). */
export const STACK_MEMORY_MB = 3 * 1024;
/** RAM kept for the host OS (macOS + Colima, or TrueNAS + its ZFS cache) and the node's shared services. */
export const NODE_RESERVED_MEMORY_MB = 8 * 1024;
/** A node that hasn't answered a health check for this long gets no tenants. */
export const NODE_STALE_MS = 15 * 60 * 1000;

export type PlacementNode = {
  id: string;
  name: string;
  kind: "LOCAL" | "MAC" | "TRUENAS";
  /** STACKS nodes never take tenants (customers' own code doesn't share a node with tenant data). */
  role?: "TENANTS" | "STACKS";
  enabled: boolean;
  capacity: number;
  poolSizeMb: number | null;
  poolUsedMb: number | null;
  poolHealth: string | null;
  memoryMb: number | null;
  lastSeenAt: Date | null;
  /** storageUnits of the tenants already on the node (not deleted) */
  tenants: { storageUnits: number }[];
};

export type Placement = { node: PlacementNode } | { node: null; reasons: string[] };

/** Why a node can't take a tenant of `storageUnits`, or null if it can. */
export function unfit(node: PlacementNode, storageUnits: number, now = new Date()): string | null {
  if (node.role === "STACKS") return "stacks node";
  if (!node.enabled) return "draining";
  if (node.tenants.length >= node.capacity) return "at capacity";
  if (node.kind === "LOCAL") return null; // development host: no pool or health data
  if (!node.lastSeenAt || now.getTime() - node.lastSeenAt.getTime() > NODE_STALE_MS) return "not seen recently";
  if (node.poolHealth !== "ONLINE") return `pool ${node.poolHealth ?? "unknown"}`;
  if (!node.poolSizeMb || node.poolUsedMb == null) return "pool not measured";
  if (node.poolUsedMb >= node.poolSizeMb * POOL_MAX_USED) return "pool almost full";
  if (committedMb(node) + storageQuotaMb(storageUnits) > node.poolSizeMb * POOL_OVERCOMMIT) return "pool fully booked";
  if (node.memoryMb != null && NODE_RESERVED_MEMORY_MB + (node.tenants.length + 1) * STACK_MEMORY_MB > node.memoryMb) {
    return "not enough memory";
  }
  return null;
}

function committedMb(node: PlacementNode) {
  return node.tenants.reduce((sum, t) => sum + storageQuotaMb(t.storageUnits), 0);
}

/**
 * Picks the fitting node with the most free booked space (share of the pool),
 * so tenants spread out. Remote nodes win over the local development host.
 */
export function pickNode(nodes: PlacementNode[], storageUnits: number, now = new Date()): Placement {
  const reasons: string[] = [];
  const fitting = nodes.filter((n) => {
    const why = unfit(n, storageUnits, now);
    if (why) reasons.push(`${n.name}: ${why}`);
    return !why;
  });
  const free = (n: PlacementNode) => (n.kind === "LOCAL" ? -Infinity : 1 - committedMb(n) / (n.poolSizeMb! * POOL_OVERCOMMIT));
  fitting.sort((a, b) => free(b) - free(a) || a.tenants.length - b.tenants.length);
  return fitting[0] ? { node: fitting[0] } : { node: null, reasons };
}

// ------------------------------------------------------------ custom stacks

/** Memory of a STACKS node's Docker VM kept for its edge proxy, the egress proxies and the system. */
export const STACKS_NODE_RESERVED_MEMORY_MB = 2 * 1024;

export type StacksPlacementNode = {
  id: string;
  name: string;
  role: "TENANTS" | "STACKS";
  enabled: boolean;
  poolSizeMb: number | null;
  poolUsedMb: number | null;
  poolHealth: string | null;
  /** Docker's memory on the node (the VM's on a Mac) */
  memoryMb: number | null;
  uidBase: number | null;
  lastSeenAt: Date | null;
  /** Bookings of the tenants whose stacks are already there */
  stackTenants: { stackMemoryUnits: number; stackStorageUnits: number }[];
};

/** Why a STACKS node can't take a tenant's stacks of `need`, or null. Reserved memory is a hard limit: no overcommit. */
export function unfitForStacks(node: StacksPlacementNode, need: StackUnits, now = new Date()): string | null {
  if (node.role !== "STACKS") return "not a stacks node";
  if (!node.enabled) return "draining";
  if (node.uidBase == null) return "no userns-remap";
  if (!node.lastSeenAt || now.getTime() - node.lastSeenAt.getTime() > NODE_STALE_MS) return "not seen recently";
  if (node.poolHealth !== "ONLINE") return `pool ${node.poolHealth ?? "unknown"}`;
  if (!node.poolSizeMb || node.poolUsedMb == null || !node.memoryMb) return "not measured";
  if (node.poolUsedMb >= node.poolSizeMb * POOL_MAX_USED) return "pool almost full";
  const memory = node.stackTenants.reduce((s, t) => s + stackMemoryMb(t.stackMemoryUnits), 0) + stackMemoryMb(need.memoryUnits);
  if (STACKS_NODE_RESERVED_MEMORY_MB + memory > node.memoryMb) return "not enough memory";
  const storage = node.stackTenants.reduce((s, t) => s + stackStorageMb(t.stackStorageUnits), 0) + stackStorageMb(need.storageUnits);
  if (storage > node.poolSizeMb * POOL_OVERCOMMIT) return "pool fully booked";
  return null;
}

/** The fitting STACKS node with the most free memory. */
export function pickStacksNode(nodes: StacksPlacementNode[], need: StackUnits, now = new Date()) {
  const reasons: string[] = [];
  const free = (n: StacksPlacementNode) => n.memoryMb! - n.stackTenants.reduce((s, t) => s + stackMemoryMb(t.stackMemoryUnits), 0);
  const fitting = nodes.filter((n) => {
    const why = unfitForStacks(n, need, now);
    if (why && n.role === "STACKS") reasons.push(`${n.name}: ${why}`);
    return !why;
  }).sort((a, b) => free(b) - free(a));
  return fitting[0] ? { node: fitting[0] } : { node: null, reasons };
}

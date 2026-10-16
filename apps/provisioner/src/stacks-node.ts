// A STACKS node: runs customers' own stacks, nothing else (docs/stacks.md).
// Its edge proxy (infra/node/stacks-edge.yml) has no Docker socket; its routes
// are files we copy into the proxy's routes volume, and it joins each stack's
// edge network, where only that stack's web service is.
import path from "node:path";
import { nodeMiddlewares } from "@pc/stacks";
import type { SshNode } from "./node.ts";

/** Container name of the stacks edge proxy (compose project pc-stacks-edge, service traefik). */
export const STACKS_EDGE = "pc-stacks-edge-traefik-1";
/** Built on each STACKS node from infra/node/egress; "castor.local/" so no registry ever serves this name. */
export const EGRESS_IMAGE = "castor.local/egress:1";
const ROUTE_NAME = /^[a-z0-9-]{1,100}$/;

export type StacksFacts = {
  /** Docker runs containers in a user namespace (userns-remap). */
  userns: boolean;
  /** gVisor is installed as a Docker runtime. */
  runsc: boolean;
  memoryMb: number;
  cpus: number;
  arch: string;
};

/** What the node's Docker engine offers (registration and health checks). */
export function stacksFacts(node: Pick<SshNode, "docker">): StacksFacts {
  const info = JSON.parse(node.docker(["info", "--format", "{{json .}}"])) as {
    SecurityOptions?: string[];
    Runtimes?: Record<string, unknown>;
    MemTotal?: number;
    NCPU?: number;
    Architecture?: string;
  };
  return {
    userns: (info.SecurityOptions ?? []).some((o) => o === "name=userns" || o.startsWith("name=userns,")),
    runsc: Object.keys(info.Runtimes ?? {}).includes("runsc"),
    memoryMb: Math.floor((info.MemTotal ?? 0) / (1024 * 1024)),
    cpus: info.NCPU ?? 0,
    arch: info.Architecture ?? "",
  };
}

/** A one-file tar archive (what `docker cp -` reads). */
export function tarFile(name: string, content: string, mode = 0o644): Buffer {
  const data = Buffer.from(content);
  const header = Buffer.alloc(512);
  const field = (value: string, offset: number, length: number) => header.write(value, offset, length, "ascii");
  const octal = (n: number, length: number) => n.toString(8).padStart(length - 1, "0") + "\0";
  field(name, 0, 100);
  field(octal(mode, 8), 100, 8);
  field(octal(0, 8), 108, 8); // uid
  field(octal(0, 8), 116, 8); // gid
  field(octal(data.length, 12), 124, 12);
  field(octal(Math.floor(Date.now() / 1000), 12), 136, 12);
  field("        ", 148, 8); // checksum placeholder
  field("0", 156, 1);
  field("ustar\0", 257, 6);
  field("00", 263, 2);
  let sum = 0;
  for (const b of header) sum += b;
  field(sum.toString(8).padStart(6, "0") + "\0 ", 148, 8);
  const pad = Buffer.alloc((512 - (data.length % 512)) % 512);
  return Buffer.concat([header, data, pad, Buffer.alloc(1024)]);
}

/** Writes a route file into the stacks edge proxy (it watches the directory). */
export function putRoute(node: Pick<SshNode, "docker">, name: string, content: string) {
  if (!ROUTE_NAME.test(name)) throw new Error(`invalid route name ${name}`);
  node.docker(["cp", "-", `${STACKS_EDGE}:/routes`], { input: tarFile(`${name}.yml`, content) });
}

export function removeRoute(node: Pick<SshNode, "docker">, name: string) {
  if (!ROUTE_NAME.test(name)) throw new Error(`invalid route name ${name}`);
  node.docker(["exec", STACKS_EDGE, "rm", "-f", `/routes/${name}.yml`], { allowFail: true });
}

/** Lets the edge proxy reach a stack's web service (idempotent). */
export function connectEdge(node: Pick<SshNode, "docker">, network: string) {
  const joined = node.docker(["inspect", "--format", "{{json .NetworkSettings.Networks}}", STACKS_EDGE]);
  if (Object.keys(JSON.parse(joined) as object).includes(network)) return;
  node.docker(["network", "connect", network, STACKS_EDGE]);
}

/** Before removing a stack's networks. */
export function disconnectEdge(node: Pick<SshNode, "docker">, network: string) {
  node.docker(["network", "disconnect", "-f", network, STACKS_EDGE], { allowFail: true });
}

/**
 * Once per worker process: builds the egress proxy image, (re)starts the edge
 * proxy, writes its allowlist middleware and reconnects it to every stack.
 */
export function prepareStacksNode(node: SshNode, root: string) {
  if (!node.controlPlaneIps.length) throw new Error(`${node.name}: stacks node without control-plane addresses`);
  const facts = stacksFacts(node);
  // Checked at registration too; a node whose Docker lost userns-remap gets no stacks.
  if (!facts.userns) throw new Error(`${node.name}: Docker runs without userns-remap; refusing to run stacks`);
  node.docker(["build", "-q", "-t", EGRESS_IMAGE, path.join(root, "infra/node/egress")]);
  node.docker([
    "compose", "-p", "pc-stacks-edge",
    "-f", path.join(root, "infra/node/stacks-edge.yml"),
    "up", "-d", "--remove-orphans",
  ], { env: { EDGE_PORT: String(node.edgePortNumber), CONTROL_PLANE_IPS: node.controlPlaneIps.join(",") } });
  putRoute(node, "castor", nodeMiddlewares(node.controlPlaneIps));
  const networks = node.docker(["network", "ls", "--filter", "name=^pc-s-", "--format", "{{.Name}}"]).split("\n").map((l) => l.trim());
  for (const n of networks) if (/^pc-s-[a-z0-9-]+-edge$/.test(n)) connectEdge(node, n);
}

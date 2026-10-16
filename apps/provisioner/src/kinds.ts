// Which job kinds the TypeScript worker runs: all but those castord takes.
import type { JobKind } from "@pc/db";

/** `castord`: CASTORD_KINDS, e.g. "SUSPEND,DELETE". A typo stops the worker rather than leaving jobs unrun. */
export function workerKinds(all: readonly JobKind[], castord: string | undefined): JobKind[] {
  const moved = (castord ?? "").split(",").map((k) => k.trim()).filter(Boolean);
  for (const k of moved) if (!all.includes(k as JobKind)) throw new Error(`CASTORD_KINDS: unknown job kind ${k}`);
  const kinds = all.filter((k) => !moved.includes(k));
  if (!kinds.length) throw new Error("CASTORD_KINDS takes every job kind: nothing left for this worker");
  return kinds;
}

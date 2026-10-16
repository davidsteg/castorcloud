// Which job kinds and recurring tasks the TypeScript worker runs: all but
// those castord takes.
import type { JobKind } from "@pc/db";

/** What `setting` (e.g. CASTORD_KINDS="SUSPEND,DELETE") leaves of `all`. A typo stops the worker rather than leaving work undone. */
function remaining<T extends string>(name: string, what: string, all: readonly T[], setting: string | undefined): T[] {
  const moved = (setting ?? "").split(",").map((k) => k.trim()).filter(Boolean);
  for (const k of moved) if (!all.includes(k as T)) throw new Error(`${name}: unknown ${what} ${k}`);
  return all.filter((k) => !moved.includes(k));
}

/** CASTORD_KINDS. May leave none: the worker then only runs tasks. */
export function workerKinds(all: readonly JobKind[], castord: string | undefined): JobKind[] {
  return remaining("CASTORD_KINDS", "job kind", all, castord);
}

/** CASTORD_TASKS. */
export function workerTasks<T extends string>(all: readonly T[], castord: string | undefined): T[] {
  return remaining("CASTORD_TASKS", "task", all, castord);
}

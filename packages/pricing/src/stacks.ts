// Resource units of custom stacks. A stack's containers each reserve memory
// and CPU (hard limits on the node); its storage is a quota on its dataset.
import { STACK_CPU_UNIT, STACK_MEMORY_UNIT_MB, STACK_STORAGE_UNIT_GB } from "./catalog.ts";

export type StackResources = { memoryMb: number; cpus: number; storageGb: number };
export type StackUnits = { memoryUnits: number; cpuUnits: number; storageUnits: number };

export const NO_STACK_UNITS: StackUnits = { memoryUnits: 0, cpuUnits: 0, storageUnits: 0 };

/** Units a stack books, rounded up: a 300 MB container books 512 MB. */
export function stackUnits(r: StackResources): StackUnits {
  return {
    memoryUnits: Math.ceil(r.memoryMb / STACK_MEMORY_UNIT_MB),
    // Round first so float noise (0.1 + 0.2) never books an extra unit.
    cpuUnits: Math.ceil(Math.round((r.cpus / STACK_CPU_UNIT) * 1000) / 1000),
    storageUnits: Math.ceil(r.storageGb / STACK_STORAGE_UNIT_GB),
  };
}

export function addUnits(a: StackUnits, b: StackUnits): StackUnits {
  return {
    memoryUnits: a.memoryUnits + b.memoryUnits,
    cpuUnits: a.cpuUnits + b.cpuUnits,
    storageUnits: a.storageUnits + b.storageUnits,
  };
}

/** Whether `need` fits into what is booked. */
export function fitsUnits(need: StackUnits, booked: StackUnits): boolean {
  return need.memoryUnits <= booked.memoryUnits && need.cpuUnits <= booked.cpuUnits && need.storageUnits <= booked.storageUnits;
}

export const stackMemoryMb = (units: number) => units * STACK_MEMORY_UNIT_MB;
export const stackCpus = (units: number) => units * STACK_CPU_UNIT;
export const stackStorageMb = (units: number) => units * STACK_STORAGE_UNIT_GB * 1024;

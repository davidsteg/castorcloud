// Small JSON client for the tenant's services (reached on the private stack
// network, never via the internet).
import { ToolError } from "./limits.ts";

type Query = Record<string, string | number | boolean | undefined>;

export async function getJson<T>(
  base: string,
  path: string,
  opts: { headers?: Record<string, string>; query?: Query; method?: "GET" | "POST"; body?: unknown; label: string },
): Promise<T> {
  const url = new URL(path, base);
  for (const [k, v] of Object.entries(opts.query ?? {})) if (v !== undefined) url.searchParams.set(k, String(v));
  let res: Response;
  try {
    res = await fetch(url, {
      method: opts.method ?? "GET",
      headers: { Accept: "application/json", ...(opts.body ? { "Content-Type": "application/json" } : {}), ...opts.headers },
      body: opts.body ? JSON.stringify(opts.body) : undefined,
      signal: AbortSignal.timeout(20_000),
    });
  } catch {
    throw new ToolError(`${opts.label} is not reachable right now`);
  }
  if (res.status === 401 || res.status === 403) throw new ToolError(`${opts.label} refused access`);
  if (res.status === 404) throw new ToolError(`not found in ${opts.label}`);
  if (!res.ok) throw new ToolError(`${opts.label} answered ${res.status}`);
  return (await res.json()) as T;
}

/** Remembers a value per key for a while (API keys, lookup tables). */
export class TtlCache<V> {
  private readonly entries = new Map<string, { value: V; until: number }>();
  private readonly ttlMs: number;
  // No parameter properties: the container runs this file with type stripping.
  constructor(ttlMs: number) {
    this.ttlMs = ttlMs;
  }

  async get(key: string, load: () => Promise<V>): Promise<V> {
    const hit = this.entries.get(key);
    if (hit && hit.until > Date.now()) return hit.value;
    const value = await load();
    this.entries.set(key, { value, until: Date.now() + this.ttlMs });
    return value;
  }

  delete(key: string) {
    this.entries.delete(key);
  }
}

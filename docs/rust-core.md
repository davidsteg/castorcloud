# Moving the core to Rust

The control-plane core moves from TypeScript to Rust one module at a time,
one pull request per step. Every step stays compatible with what it
replaces, so the TypeScript and Rust parts run side by side until the
TypeScript part is deleted. The rule: as much as possible in Rust, but
where something else is clearly better, use that, and say why here.

## Why Rust for the core

The core holds every key, runs as root on the nodes (through `sudo`) and
turns customer input into commands. Rust gives memory safety without a
runtime, strict types for every command and argument instead of shell word
splitting, key material that is wiped when dropped (`zeroize`), and one
static binary per platform with a short, audited dependency list
(`cargo audit` in CI).

## Module by module

| Today | Becomes | Why |
|---|---|---|
| `packages/core/src/crypto.ts` | Rust `crates/crypto` (done) | Holds the keys; byte-compatible format, tested in both directions |
| `infra/node/bin/castor-ssh`, `castor-zfs`, `castor-snap`, `infra/node/cryptd` | One Rust binary `castor-node` (gate + subcommands) | Runs as root behind the SSH forced command: parsed, typed arguments instead of shell `case` patterns, unit tests for every refusal |
| BullMQ + Redis job queue (`packages/core/src/jobs.ts`) | Postgres queue: the `Job` table, claimed with `FOR UPDATE SKIP LOCKED`, woken by `LISTEN/NOTIFY` | Postgres is already the source of truth; one service less (Redis), and a Rust worker can claim jobs without speaking BullMQ's Lua protocol. The portal enqueues by inserting a row |
| `apps/provisioner` worker: lifecycle, tenant rendering, placement, storage, nodes, ZFS, volumes, backups, retention, DB dumps | Rust daemon `castord` (tokio, sqlx), ported job kind by job kind | The privileged orchestration; each job kind moves when its Rust version passes the same tests |
| Monitoring and alert rules, Authentik and LLM-gateway clients, Telegram bot | Part of `castord` | Plain HTTP and rules; no reason to keep a second runtime once the worker is Rust |
| Admin CLIs (`node`, `backup`, `staff`, `delete-tenant`) | `castorctl` subcommands | Same code paths as the daemon |

## What stays as it is

| Part | Stays | Why |
|---|---|---|
| Portal (`apps/portal`) | Next.js + TypeScript | UI, i18n, React server components, Stripe and Auth.js SDKs; Rust has no equivalent ecosystem for this |
| Database schema and migrations | Prisma (`packages/db`) | The portal reads through Prisma; one owner of migrations. Rust reads the same tables with `sqlx` (queries checked against the schema in CI) |
| Shared pricing (`packages/pricing`) | TypeScript | The portal renders it live. The few numbers the worker needs (storage quota) are mirrored in Rust and pinned by shared test fixtures |
| SSH and Docker transport | OpenSSH and the Docker / Compose CLIs | OpenSSH is the most reviewed SSH implementation there is (known_hosts, key handling); Compose has no Rust implementation. Called with argument vectors, never through a shell |
| Node setup (`infra/node/macos`, `infra/node/truenas`) | Shell | One-time setup that drives the OS's own tools; nothing runs on it at request time |
| Hermes data tools (`agent/tools`) | TypeScript for now | Low-privilege MCP server inside each tenant's stack using the official MCP SDK; can move later (official Rust SDK `rmcp`) once the core is done |

## Order

1. Workspace and encryption (done).
2. Job queue on Postgres, still consumed by the TypeScript worker.
3. `castor-node` replacing the node scripts, installed next to them and
   switched over per node.
4. `castord` with the job claim loop, taking job kinds over one by one;
   the TypeScript worker only claims the kinds it still owns.
5. Monitoring, clients and admin CLIs; then the TypeScript provisioner is
   removed.

Work in flight elsewhere (custom stacks, GitHub login) lands in
TypeScript first and is ported in the step that covers its module.

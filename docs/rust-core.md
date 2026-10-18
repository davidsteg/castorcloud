# Moving the core to Rust

The control-plane core moved from TypeScript to Rust one module at a time,
one pull request per step. Every step stayed compatible with what it
replaced, so the TypeScript and Rust parts ran side by side until the
TypeScript part was deleted. The migration is complete: the TypeScript
worker (`apps/provisioner`) is gone, and `castord`, `castor-admin`,
`castor-monitor` and `castor-telegram` (`rust/README.md`) do its work. The
rule: as much as possible in Rust, but where something else is clearly
better, use that, and say why here.

## Why Rust for the core

The core holds every key, runs as root on the nodes (through `sudo`) and
turns customer input into commands. Rust gives memory safety without a
runtime, strict types for every command and argument instead of shell word
splitting, key material that is wiped when dropped (`zeroize`), and one
static binary per platform with a short, audited dependency list
(`cargo audit` in CI).

## Module by module

| Was | Became | Why |
|---|---|---|
| `packages/core/src/crypto.ts` | Rust `crates/crypto` (done) | Holds the keys; byte-compatible format, tested in both directions |
| `infra/node/bin/castor-ssh`, `castor-zfs`, `castor-snap` | One Rust binary `castor-node` (done, `crates/node`), now for Mac nodes only | Runs as root behind the SSH forced command: parsed, typed arguments instead of shell `case` patterns, unit tests for every refusal |
| SSH to TrueNAS nodes | The node role `castor-node-role` and gatekeeper (done, `crates/castord/src/link.rs`, `gatekeeper/`) | No host credential on the control plane: one tool of a fixed catalog per call over pinned mutual TLS, re-checked on the node ([gatekeeper.md](gatekeeper.md)) |
| BullMQ + Redis job queue (`packages/core/src/jobs.ts`) | Postgres queue (done): the `Job` table, claimed with `FOR UPDATE SKIP LOCKED`, leases, and a `Schedule` table for recurring tasks | Postgres is already the source of truth; Redis is gone (one service and one unauthenticated port less), and a Rust worker claims jobs with the same SQL. The portal enqueues by inserting a row |
| `apps/provisioner` worker: lifecycle, tenant rendering, placement, storage, nodes, ZFS, volumes, backups, retention, DB dumps | Rust daemon `castord` (tokio, sqlx), ported job kind by job kind (done) | The privileged orchestration; each job kind moved when its Rust version passed the same tests |
| Monitoring and alert rules, Authentik and LLM-gateway clients, Telegram bot | Part of `castord` (done: `castor-monitor`, `castor-telegram`) | Plain HTTP and rules; no reason to keep a second runtime once the worker is Rust |
| Admin CLIs (`node`, `backup`, `staff`, `delete-tenant`) | `castor-admin` subcommands (done) | Same code paths as the daemon |
| Hermes data tools (`agent/tools`, MCP SDK) | Rust crate of its own in `agent/tools` (done), a static binary on an empty image | Runs in every tenant stack with database access to issue API keys; same tools, schemas and answers as before (tested against the TypeScript version). The MCP layer is hand-rolled (four methods, stateless JSON) rather than `rmcp` |

## What stays as it is

| Part | Stays | Why |
|---|---|---|
| Portal (`apps/portal`) | Next.js + TypeScript | UI, i18n, React server components, Stripe and Auth.js SDKs; Rust has no equivalent ecosystem for this |
| Database schema and migrations | Prisma (`packages/db`) | The portal reads through Prisma; one owner of migrations. Rust reads the same tables with `sqlx` (queries checked against the schema in CI) |
| Shared pricing (`packages/pricing`) | TypeScript | The portal renders it live. The few numbers the worker needs (storage quota) are mirrored in Rust and pinned by shared test fixtures |
| SSH and Docker transport to Mac nodes | OpenSSH and the Docker / Compose CLIs | OpenSSH is the most reviewed SSH implementation there is (known_hosts, key handling); Compose has no Rust implementation. Called with argument vectors, never through a shell |
| Node setup (`infra/node/macos`, `infra/node/truenas`) | Shell | One-time setup that drives the OS's own tools; nothing runs on it at request time |

## Order

1. Workspace and encryption (done).
2. Job queue and recurring tasks on Postgres, Redis removed (done). The
   claim/lease/retry contract is at the top of `packages/core/src/jobs.ts`;
   the Rust worker implements the same, and each worker declares which job
   kinds it runs.
3. `castor-node` replacing the node scripts, switched over per node by
   re-running its setup script (done; see `rust/README.md`). The dev-only
   `infra/node/cryptd` (LUKS volumes on the local Docker host) stays shell
   until the local node moves to ZFS too.
4. `castord` with the job claim loop, taking job kinds over one by one
   while the TypeScript worker claimed the kinds it still owned (done;
   `rust/crates/castord`, tested against the same Postgres schema). castord
   now takes every job kind and recurring task; `CASTORD_KINDS` /
   `CASTORD_TASKS` in `.env` can narrow that. The monitor raises
   `platform:queue` when a job kind has no live worker.
5. Monitoring, clients and admin CLIs; then the TypeScript provisioner was
   removed (done).

Custom stacks and GitHub login landed in TypeScript first and were ported
in the step that covered their module.

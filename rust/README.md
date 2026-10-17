# Rust core

Plan and reasons: [docs/rust-core.md](../docs/rust-core.md).

The control-plane core: the worker that runs tenant jobs and recurring
tasks, the monitor, the Telegram bot, the operator CLI and the node tools.
It replaced the TypeScript worker `apps/provisioner`; the portal (Next.js)
enqueues jobs and seals secrets in formats these crates read.

| Crate | Replaces | State |
|---|---|---|
| `crates/crypto` | `packages/core/src/crypto.ts` | done; format-compatible, tested in both directions |
| `crates/node` | `infra/node/bin/castor-ssh`, `castor-zfs`, `castor-snap` | done; same commands and output, the scripts stay until every node runs it |
| `crates/castord` | `apps/provisioner/src/worker.ts` | done; every job kind and task (same queue SQL as `jobs.ts`) |
| `castor-monitor` (in `crates/castord`) | `apps/provisioner/src/monitor.ts` | done; same checks, rules, alert table and settings |
| `castor-telegram` (in `crates/castord`) | `apps/provisioner/src/telegram.ts` | done; same pairing, texts and settings |
| `castor-admin` (in `crates/castord`) | `node-admin.ts`, `backup-admin.ts`, `staff-admin.ts`, `delete-tenant.ts`, `e2e.ts` | done; same commands and checks |

```bash
cd rust
cargo test
cargo clippy --all-targets -- -D warnings
```

castord's queue tests need a Postgres with the migrations applied; set
`TEST_DATABASE_URL` (without it they pass by skipping, except in CI).

Rules for every crate: no `unsafe` (forbidden workspace-wide), no
`unwrap`/`expect`/`panic` outside tests, key material in `zeroize` types
and never `Debug`-printable.

## castord

The worker, with the control plane's `.env`:

```bash
cargo build --release -p castord
set -a; . ./.env; set +a
rust/target/release/castord        # from the repository, or with CASTOR_ROOT
```

It takes every job kind and runs every recurring task. `CASTORD_KINDS` and
`CASTORD_TASKS` limit one castord to some (a comma list, or `none`), for
example to run the backups on a second one; a job kind no castord takes
raises the monitor's `platform:queue`. When it starts it applies the
self-registration flow in Authentik and records the Docker host it runs on
as the node `local` (taking tenants unless `LOCAL_NODE=off`).

| Kind | Notes |
|---|---|
| `SUSPEND` | stops custom stacks and the tenant stack, locks both datasets, blocks the LLM key |
| `DELETE` | removes custom stacks, the tenant stack, its data and files, its Authentik apps and groups, LLM key and secrets; deleting the volume key crypto-shreds every copy |
| `REVOKE_ACCESS` | ends sessions and keys in the booked apps, Authentik sessions for "sign out everywhere"; waits while the apps are stopped |
| `PROVISION`, `UPDATE`, `RESUME` | places the tenant, prepares its node once per process, sets up Authentik, opens and sizes the storage, renders the files, starts the stack and waits for it; runs pending revocations; `UPDATE`/`RESUME` queue `STACKS` when the tenant has custom stacks |
| `STACKS` | places the tenant's stacks node and prepares it (egress image, edge proxy, its allowlist); removes, stops or deploys each stack (images pinned by digest, data directories owned by the mapped container user, Docker checked for userns-remap and gVisor right before), its sign-in, node route and the control plane's route |
| `RESTORE` | stops the stack and locks its volume, sets the current dataset aside, pipes the backup box's raw stream into the node's `zfs receive` (a failure puts the old dataset back), then applies as `UPDATE`; a retry once the data is back only restarts the stack |

Each recurring task runs once per slot across every worker (the `Schedule`
table); the backups run in a loop of their own, so a long first send never
holds up the node watcher:

| Task | Notes |
|---|---|
| `watch-nodes` | every 5 min: probes remote nodes (pool, memory, CPUs; a stacks node by its Docker engine, checked for userns-remap and gVisor), records the facts or the error, and reopens active tenants whose storage or stacks came back locked |
| `measure-storage` | hourly: every active tenant's storage use, for the portal |
| `sweep-deletions` | hourly: queues `DELETE` for tenants whose grace period has ended |
| `backup-tenants` | every `BACKUP_INTERVAL_HOURS`: snapshot each active tenant on a ZFS node and pipe it to the box (incremental while the chain is intact, else a full send into a new generation), record the outcome, then the retention policy (`src/retention.rs`) and forgetting deleted tenants |
| `backup-database` | 02:30: `pg_dumpall` of the control plane, gzipped and sealed to `BACKUP_DB_PUBLIC_KEY` (fetched with `castor-admin backup db-get`, opened offline with `db-open`) |
| `verify-backups` | Sundays 04:00: the box reads every tenant's newest backup back |

Settings (`src/main.rs` lists them all): `AUTHENTIK_BOOTSTRAP_TOKEN`,
`BASE_DOMAIN`, `PUBLIC_SCHEME`, `BRAND_NAME`, `TZ`, `AUTH_HOST_IP`, the encryption
keys, `LITELLM_MASTER_KEY`, `AI_USD_PER_CHF`, `CA_DIR`; in development also
`CASTORD_EXTRA_CA_CERTS=.dev-certs/ca.crt`.

castord prepares a node (templates, edge proxy; on a STACKS node the egress
image and the stacks edge) before it starts anything there.

Custom stacks: castord checks a stack definition with the same rules as
`@pc/stacks` (`src/stacks/spec.rs`, held to `tests/fixtures/stacks.json`,
which CI regenerates from `@pc/stacks`), but it reads only the parsed
document the portal stores as JSON, never the customer's YAML: two YAML
parsers could read one file two ways, and the definition is where the
security checks are. Definitions saved as YAML before that are converted
once with `npm run stacks:json -w @pc/portal`. Settings:
`STACKS_DOMAIN`, `STACKS_CERT_RESOLVER`.

Restores need `BACKUP_TARGET` (the backup box's user@host). The stream goes from one ssh to the other through
castord, still encrypted with the tenant's key, which castord never sends.

## castor-monitor

The monitor of docs/monitoring.md in Rust: the same checks (platform, nodes,
services, jobs, backups, certificates, Authentik, audit log), the same rules
(`monitor/rules.rs`, tested against the cases of the TypeScript monitor), the
same `Alert` rows and the same settings. Run one (each sends its own
digest):

```bash
cargo build --release -p castord
set -a; . ./.env; set +a
rust/target/release/castor-monitor test     # then: run, once, status
```

E-mail goes only over TLS (smtps://, or smtp:// with STARTTLS required); the
certificate check reads a bad certificate too, but reports it untrusted, and
sends nothing over the connection. In development set
`CASTORD_EXTRA_CA_CERTS=.dev-certs/ca.crt`.

## castor-telegram

The assistant's Telegram bot in Rust: pairing codes from the portal, one
conversation per linked member, `/new`, `/disconnect`, the same texts in the
four languages. Settings: `TELEGRAM_BOT_TOKEN`, `DATABASE_URL`, the
encryption keys, `BASE_DOMAIN`, `PUBLIC_SCHEME`, `LLM_GATEWAY_URL`, `TZ`; run
one, since Telegram hands each update to one poller. Answers go back as plain text, and the bot token never
appears in a log line.

## castor-admin

The operator commands in one binary, with the control plane's `.env`:

```bash
castor-admin node add <name> <user@host> --pool tank --edge <host:port> --auth-host-ip <ip>
castor-admin node add <name> <user@host> --role stacks --pool tank --edge <host> --control-plane-ip <ip>
castor-admin node list | drain <name> | enable <name>
castor-admin backup status | list <slug> | run <slug> | verify | restore <slug> [gen@snap] [--node <name>] | discard <slug> <tag>
castor-admin backup db-keygen | db-dump | db-list | db-get <name> <out-file>
castor-admin backup db-open <file> <secret-key-file> <out.sql.gz>
castor-admin staff list | add <email> | remove <email>
castor-admin delete-tenant <slug>
castor-admin e2e [--delete]
```

It replaced the TypeScript `npm run node`, `backup`, `staff`,
`delete-tenant` and `e2e`, with the same arguments. `node add` probes the node and refuses it unless
its pool is ONLINE (a stacks node also needs Docker with userns-remap and
gVisor, or `--no-gvisor`), then prepares it. Files with secrets
(`db-get`, `db-open`) are written with mode 0600.

## castor-node on a node

One binary replaces the three node scripts; it picks the tool by the name it
is installed under. CI builds it for TrueNAS SCALE in the `Rust core` job
on every push to main (artifact `castor-node-x86_64-unknown-linux-musl`,
with a SHA-256 file, kept 30 days). Mac minis build it themselves with
`cargo build --release -p castor-node`. To switch
a node:

1. Download the artifact from the CI run of the commit you deploy (on a
   Mac, build it from that commit), check it with `shasum -a 256 -c castor-node.sha256`.
2. Put it at `infra/node/bin/castor-node` in the copy of `infra/node` on the node.
3. Re-run the node's setup script (`truenas/setup.sh` or
   `macos/bootstrap.sh`). It installs the binary as `castor-ssh`,
   `castor-zfs` and `castor-snap`, so `authorized_keys` and sudoers stay
   as they are. Without the binary it installs the shell scripts, as before.

Differences from the scripts, all stricter: `castor.conf` is parsed as data
and refused unless root-owned and not writable by group or others; dataset
keys must be 64 hex characters; `castor-put` takes no extra arguments and
never follows a symlink at its temporary path; `castor-untar` refuses links,
devices and paths outside the target and keeps the old files when an
archive is bad; `usage` skips symlinks and stays on the dataset (`du -x`);
root tools never search Homebrew's directories for `zfs`.

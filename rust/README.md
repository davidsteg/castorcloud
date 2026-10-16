# Rust core

Plan and reasons: [docs/rust-core.md](../docs/rust-core.md).

The control-plane core, moving from TypeScript (`packages/core`,
`apps/provisioner`) to Rust one module at a time. Each crate stays
compatible with the TypeScript it replaces, so both run side by side until
the TypeScript part is removed.

| Crate | Replaces | State |
|---|---|---|
| `crates/crypto` | `packages/core/src/crypto.ts` | done; format-compatible, tested in both directions |
| `crates/node` | `infra/node/bin/castor-ssh`, `castor-zfs`, `castor-snap` | done; same commands and output, the scripts stay until every node runs it |
| `crates/castord` | `apps/provisioner/src/worker.ts` | queue loop done (same SQL as `jobs.ts`); every job kind ported |

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

Runs next to the TypeScript worker on the same queue, with the control
plane's `.env` (`set -a; . ./.env; set +a; castord` from the repository, or
`CASTOR_ROOT` pointing at it). `CASTORD_KINDS` lists the job kinds it takes;
the TypeScript worker reads the same setting and leaves those kinds alone, so
one line in `.env` moves a kind, and emptying it moves it back. castord refuses
a kind it has no handler for, and the monitor raises `platform:queue` when a
kind has no live worker.

| Kind | In castord | Notes |
|---|---|---|
| `SUSPEND` | yes | stops custom stacks and the tenant stack, locks both datasets, blocks the LLM key |
| `DELETE` | yes | removes custom stacks, the tenant stack, its data and files, its Authentik apps and groups, LLM key and secrets; deleting the volume key crypto-shreds every copy |
| `REVOKE_ACCESS` | yes | ends sessions and keys in the booked apps (access.ts commands), Authentik sessions for "sign out everywhere"; waits while the apps are stopped |
| `PROVISION`, `UPDATE`, `RESUME` | yes | places the tenant, prepares its node once per process, sets up Authentik, opens and sizes the storage, renders the files, starts the stack and waits for it; runs pending revocations; `UPDATE`/`RESUME` queue `STACKS` when the tenant has custom stacks |
| `STACKS` | yes | places the tenant's stacks node and prepares it (egress image, edge proxy, its allowlist); removes, stops or deploys each stack (images pinned by digest, data directories owned by the mapped container user, Docker checked for userns-remap and gVisor right before), its sign-in, node route and the control plane's route |
| `RESTORE` | yes | stops the stack and locks its volume, sets the current dataset aside, pipes the backup box's raw stream into the node's `zfs receive` (a failure puts the old dataset back), then applies as `UPDATE`; a retry once the data is back only restarts the stack |

Recurring tasks move the same way, with `CASTORD_TASKS`. Each runs once per
slot across every worker (the `Schedule` table, as `schedule.ts`), so either
worker can hold it:

| Task | In castord | Notes |
|---|---|---|
| `watch-nodes` | yes | every 5 min: probes remote nodes (pool, memory, CPUs; a stacks node by its Docker engine, checked for userns-remap and gVisor), records the facts or the error, and reopens active tenants whose storage or stacks came back locked |
| `measure-storage` | yes | hourly: every active tenant's storage use, for the portal |
| `sweep-deletions` | yes | hourly: queues `DELETE` for tenants whose grace period has ended |
| `backup-tenants` | yes | every `BACKUP_INTERVAL_HOURS`: snapshot each active tenant on a ZFS node and pipe it to the box (incremental while the chain is intact, else a full send into a new generation), record the outcome, then the retention policy of `retention.ts` and forgetting deleted tenants |
| `backup-database` | yes | 02:30: `pg_dumpall` of the control plane, gzipped and sealed to `BACKUP_DB_PUBLIC_KEY` (the format of `db-dump.ts`, opened by `npm run backup -- db-get` either way) |
| `verify-backups` | yes | Sundays 04:00: the box reads every tenant's newest backup back |

castord needs the TypeScript worker's settings for these (`AUTHENTIK_BOOTSTRAP_TOKEN`,
`BASE_DOMAIN`, `PUBLIC_SCHEME`, `BRAND_NAME`, `TZ`, `AUTH_HOST_IP`, the encryption
keys, `LITELLM_MASTER_KEY`, `AI_USD_PER_CHF`, `CA_DIR`); in development also
`CASTORD_EXTRA_CA_CERTS=.dev-certs/ca.crt`.

To switch a kind: build castord (`cargo build --release -p castord`), add the
kind to `CASTORD_KINDS`, restart the TypeScript worker and start castord.
castord prepares a node (templates, edge proxy; on a STACKS node the egress
image and the stacks edge) before it starts anything there, as the
TypeScript worker does.

Custom stacks: castord checks a stack definition with the same rules as
`@pc/stacks` (`src/stacks/spec.rs`, held to `tests/fixtures/stacks.json`,
which CI regenerates from the TypeScript), but it reads only the parsed
document the portal stores as JSON, never the customer's YAML: two YAML
parsers could read one file two ways, and the definition is where the
security checks are. Definitions saved before are converted by the
TypeScript worker when it starts (`storeDefinitionsAsJson`). Settings:
`STACKS_DOMAIN`, `STACKS_CERT_RESOLVER`.

Restores need `BACKUP_TARGET` (the backup box's user@host) as for the
TypeScript worker. The stream goes from one ssh to the other through
castord, still encrypted with the tenant's key, which castord never sends.

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

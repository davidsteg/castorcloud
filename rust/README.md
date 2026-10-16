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
| `crates/castord` | `apps/provisioner/src/worker.ts` | queue loop done (same SQL as `jobs.ts`); job kinds move one at a time |

```bash
cd rust
cargo test
cargo clippy --all-targets -- -D warnings
```

castord's queue tests need a Postgres with the migrations applied; set
`TEST_DATABASE_URL` (without it they pass by skipping, except in CI).

## castord

Runs next to the TypeScript worker on the same queue. `DATABASE_URL` as for
the control plane, `CASTORD_KINDS` the comma-separated job kinds it takes.
It refuses a kind it has no handler for. To move a kind: deploy castord
with the kind added to `CASTORD_KINDS` and, in the same deploy, remove it
from `KINDS` in `apps/provisioner/src/worker.ts`.

Rules for every crate: no `unsafe` (forbidden workspace-wide), no
`unwrap`/`expect`/`panic` outside tests, key material in `zeroize` types
and never `Debug`-printable.

## castor-node on a node

One binary replaces the three node scripts; it picks the tool by the name it
is installed under. CI builds it for each node platform (job
`castor-node`, artifacts `castor-node-x86_64-unknown-linux-musl` for
TrueNAS SCALE and `castor-node-aarch64-apple-darwin` for Mac minis, each
with a SHA-256 file). To switch a node:

1. Download the artifact for the node's platform from the CI run of the
   commit you deploy, check it with `shasum -a 256 -c castor-node.sha256`.
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

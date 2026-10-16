# Rust core

Plan and reasons: [docs/rust-core.md](../docs/rust-core.md).

The control-plane core, moving from TypeScript (`packages/core`,
`apps/provisioner`) to Rust one module at a time. Each crate stays
compatible with the TypeScript it replaces, so both run side by side until
the TypeScript part is removed.

| Crate | Replaces | State |
|---|---|---|
| `crates/crypto` | `packages/core/src/crypto.ts` | done; format-compatible, tested in both directions |

```bash
cd rust
cargo test
cargo clippy --all-targets -- -D warnings
```

Rules for every crate: no `unsafe` (forbidden workspace-wide), no
`unwrap`/`expect`/`panic` outside tests, key material in `zeroize` types
and never `Debug`-printable.

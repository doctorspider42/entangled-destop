# Vendored: linux-loader 0.14.0 (patched)

- Upstream: https://crates.io/crates/linux-loader, exact copy of the
  published 0.14.0 sources from the local cargo registry cache.
- License: Apache-2.0 AND BSD-3-Clause (files kept verbatim; both permissive).
- Wired in via `[patch.crates-io]` in the workspace root.

## The one change

`Cargo.toml` only: `default-features = false` on both `vm-memory` dependency
edges (main and dev). Same reason as the sibling `third_party/virtio-queue`
patch, and the same blocker one crate further along the graph (ADR-0002
amendments): vm-memory's default `rawfd` feature is unix-only — upstream
carries a literal `compile_error!` for Windows, and its `src/io.rs` calls
`libc::read`/`libc::write` with unix signatures — and cargo features are
additive, so linux-loader's default-features edge re-enabled `rawfd` for the
whole graph. That kept `linux-boot` (and therefore the direct-Linux boot path
the WHP backend needs) from building natively on Windows even after
virtio-queue was fixed.

linux-loader's own code only uses vm-memory's portable core traits
(`GuestMemory`, `Bytes`, `GuestAddress`) plus, in its own tests,
`backend-mmap`; nothing here needs `rawfd`.

No source (`src/`) changes. Drop this directory the moment upstream rust-vmm
ships an equivalent fix (an upstream PR is the intended endgame, and it is the
same PR virtio-queue wants).

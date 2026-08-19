# Vendored: virtio-queue 0.17.0 (patched)

- Upstream: https://crates.io/crates/virtio-queue, exact copy of the
  published 0.17.0 sources from the local cargo registry cache.
- License: Apache-2.0 OR BSD-3-Clause (files kept verbatim; both permissive).
- Wired in via `[patch.crates-io]` in the workspace root.

## The one change

`Cargo.toml` only: `default-features = false` on every `vm-memory`
dependency edge (main, dev, kani). Rationale (ADR-0002 amendments):
vm-memory's default `rawfd` feature is unix-only — upstream carries a
literal `compile_error!` for Windows — and cargo features are additive, so
virtio-queue's default-features edge re-enabled `rawfd` for the whole graph
and broke every native Windows build of the virtio crates. virtio-queue's
own code only uses the portable core traits; nothing here needs `rawfd`.

No source (`src/`) changes. Drop this directory the moment upstream
rust-vmm ships an equivalent fix (an upstream PR is the intended endgame).

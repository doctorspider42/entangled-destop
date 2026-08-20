#!/usr/bin/env bash
# Builds the bootstrap initramfs (backlog MVP-1102/1104): a single static
# /init that mounts the installed root and switch_roots into it.
set -euo pipefail
cd "$(dirname "$0")/.."

rustup target add x86_64-unknown-linux-musl >/dev/null

cargo build --quiet --release --target x86_64-unknown-linux-musl \
    --manifest-path guest/bootstrap-initramfs/init-rs/Cargo.toml

mkdir -p artifacts/bootstrap
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
# cargo honours CARGO_TARGET_DIR, so the binary is not always under the crate's
# own target/. Copying from the hard-coded path silently packed a STALE init
# whenever that variable was set — the "the guest ignored my new probe" trap
# that has cost several debugging sessions. Resolve the real path, then refuse
# to package a binary older than its sources.
target_root="${CARGO_TARGET_DIR:-$PWD/guest/bootstrap-initramfs/init-rs/target}"
init_bin="$target_root/x86_64-unknown-linux-musl/release/entangled-bootstrap-init"
if [ ! -f "$init_bin" ]; then
    echo "error: cargo built no $init_bin (CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-unset})" >&2
    exit 1
fi
stale=$(find guest/bootstrap-initramfs/init-rs/src guest/bootstrap-initramfs/init-rs/Cargo.toml -type f -newer "$init_bin" -print -quit)
if [ -n "$stale" ]; then
    echo "error: $init_bin is older than $stale — the build did not pick up that change" >&2
    exit 1
fi

cp "$init_bin" "$stage/init"
(cd "$stage" && echo init | cpio -o -H newc --quiet | gzip -9) \
    > artifacts/bootstrap/initrd.img
echo "wrote artifacts/bootstrap/initrd.img ($(stat -c%s artifacts/bootstrap/initrd.img) bytes)"

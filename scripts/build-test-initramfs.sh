#!/usr/bin/env bash
# Builds the minimal test initramfs (backlog MVP-207): a single static /init
# that prints VMHOST_GUEST_READY and powers off.
set -euo pipefail
cd "$(dirname "$0")/.."

rustup target add x86_64-unknown-linux-musl >/dev/null

cargo build --quiet --release --target x86_64-unknown-linux-musl \
    --manifest-path guest/test-rootfs/init-rs/Cargo.toml

mkdir -p artifacts/tests
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
# cargo honours CARGO_TARGET_DIR, so the binary is not always under the crate's
# own target/. Copying from the hard-coded path silently packed a STALE init
# whenever that variable was set — the "the guest ignored my new probe" trap
# that has cost several debugging sessions. Resolve the real path, then refuse
# to package a binary older than its sources.
target_root="${CARGO_TARGET_DIR:-$PWD/guest/test-rootfs/init-rs/target}"
init_bin="$target_root/x86_64-unknown-linux-musl/release/entangled-test-init"
if [ ! -f "$init_bin" ]; then
    echo "error: cargo built no $init_bin (CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-unset})" >&2
    exit 1
fi
stale=$(find guest/test-rootfs/init-rs/src guest/test-rootfs/init-rs/Cargo.toml -type f -newer "$init_bin" -print -quit)
if [ -n "$stale" ]; then
    echo "error: $init_bin is older than $stale — the build did not pick up that change" >&2
    exit 1
fi

cp "$init_bin" "$stage/init"
(cd "$stage" && echo init | cpio -o -H newc --quiet | gzip -9) \
    > artifacts/tests/test-initramfs.cpio.gz

echo "wrote artifacts/tests/test-initramfs.cpio.gz ($(stat -c%s artifacts/tests/test-initramfs.cpio.gz) bytes)"

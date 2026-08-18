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
cp guest/test-rootfs/init-rs/target/x86_64-unknown-linux-musl/release/entangled-test-init "$stage/init"
(cd "$stage" && echo init | cpio -o -H newc --quiet | gzip -9) \
    > artifacts/tests/test-initramfs.cpio.gz

echo "wrote artifacts/tests/test-initramfs.cpio.gz ($(stat -c%s artifacts/tests/test-initramfs.cpio.gz) bytes)"

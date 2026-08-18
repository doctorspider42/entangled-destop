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
cp guest/bootstrap-initramfs/init-rs/target/x86_64-unknown-linux-musl/release/vmhost-bootstrap-init "$stage/init"
(cd "$stage" && echo init | cpio -o -H newc --quiet | gzip -9) \
    > artifacts/bootstrap/initrd.img
echo "wrote artifacts/bootstrap/initrd.img ($(stat -c%s artifacts/bootstrap/initrd.img) bytes)"

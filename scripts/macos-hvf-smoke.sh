#!/bin/sh
# Run the Rust backend on Apple's real hypervisor, with explicit signing.
set -eu
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    echo "This acceptance requires macOS on Apple Silicon" >&2
    exit 1
fi
: "${CARGO_TARGET_DIR:?Set CARGO_TARGET_DIR outside the checkout for this task}"
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
example=hvf_smoke
case "$*" in
    "") ;;
    --shared-memory) example=hvf_shm ;;
    *) echo "usage: macos-hvf-smoke.sh [--shared-memory]" >&2; exit 1 ;;
esac
cargo build --manifest-path "$root/Cargo.toml" -p vmm-core \
    --example "$example" --target aarch64-apple-darwin --locked
binary="$CARGO_TARGET_DIR/aarch64-apple-darwin/debug/examples/$example"
codesign --force --sign - --entitlements "$root/tools/macos-probe/entitlements.plist" "$binary"
"$binary"

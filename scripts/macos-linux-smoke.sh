#!/bin/sh
# Real ARM64 kernel/rootfs acceptance, not a host-only graphics probe.
set -eu
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    echo "This acceptance requires macOS on Apple Silicon" >&2
    exit 1
fi
major=$(sw_vers -productVersion | cut -d. -f1)
if [ "$major" -lt 15 ]; then
    echo "The native GIC requires macOS 15 or newer" >&2
    exit 1
fi
if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    echo "usage: macos-linux-smoke.sh KERNEL INITRAMFS [UBUNTU_RAW_DISK]" >&2
    exit 1
fi
: "${CARGO_TARGET_DIR:?Set CARGO_TARGET_DIR outside the checkout for this task}"
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
case "$CARGO_TARGET_DIR" in
    "$root"|"$root"/*) echo "Build outputs must be outside the checkout" >&2; exit 1 ;;
    /*) ;;
    *) echo "CARGO_TARGET_DIR must be an absolute path" >&2; exit 1 ;;
esac
export MACOSX_DEPLOYMENT_TARGET=15.0
cargo build --manifest-path "$root/Cargo.toml" -p machine-arm64 \
    --example hvf_linux --target aarch64-apple-darwin --locked
binary="$CARGO_TARGET_DIR/aarch64-apple-darwin/debug/examples/hvf_linux"
codesign --force --sign - --entitlements "$root/tools/macos-probe/entitlements.plist" "$binary"
"$binary" "$@"

#!/bin/sh
# Native CLI suspend/refusal/two-resume acceptance, with an isolated signed engine.
set -eu
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    echo "This acceptance requires macOS on Apple Silicon" >&2
    exit 1
fi
: "${CARGO_TARGET_DIR:?Set CARGO_TARGET_DIR outside the checkout for this task}"
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cargo build --manifest-path "$root/Cargo.toml" -p entangled --locked
cargo test --manifest-path "$root/Cargo.toml" -p entangled --test macos_suspend --locked --no-run
scratch=$(mktemp -d "${TMPDIR:-/tmp}/entangled-suspend.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
cp "$CARGO_TARGET_DIR/debug/entangled" "$scratch/entangled"
codesign --force --sign - --entitlements "$root/tools/macos-probe/entitlements.plist" "$scratch/entangled"
ENTANGLED_NATIVE_ACCEPTANCE_ENGINE="$scratch/entangled" \
    cargo test --manifest-path "$root/Cargo.toml" -p entangled --test macos_suspend --locked -- --ignored --nocapture

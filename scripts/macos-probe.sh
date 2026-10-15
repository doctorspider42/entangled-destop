#!/bin/sh
# Keep generated binaries outside the checkout. Only Apple SDKs are required.
set -eu
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    echo "This probe requires macOS on Apple Silicon" >&2
    exit 1
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
probe_dir=$(mktemp -d "${TMPDIR:-/tmp}/entangled-macos-probe.XXXXXX")
trap 'rm -rf "$probe_dir"' EXIT HUP INT TERM
xcrun clang -fobjc-arc -fblocks -Wall -Wextra -Werror \
    "$root/tools/macos-probe/main.m" -framework Foundation -framework Metal \
    -framework Hypervisor -o "$probe_dir/probe"
codesign --force --sign - --entitlements "$root/tools/macos-probe/entitlements.plist" "$probe_dir/probe"
"$probe_dir/probe"

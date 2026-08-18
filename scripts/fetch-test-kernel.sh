#!/usr/bin/env bash
# Downloads the Debian stable netboot kernel (text variant) used by the boot
# integration tests. The proper downloader with signature verification is
# EPIC 6; this script is test tooling only.
set -euo pipefail
cd "$(dirname "$0")/.."

url="https://deb.debian.org/debian/dists/stable/main/installer-amd64/current/images/netboot/debian-installer/amd64/linux"
mkdir -p artifacts/tests
if [ -s artifacts/tests/vmlinuz ]; then
    echo "artifacts/tests/vmlinuz already present, skipping download"
    exit 0
fi
curl -fL --proto '=https' -o artifacts/tests/vmlinuz "$url"
echo "wrote artifacts/tests/vmlinuz ($(stat -c%s artifacts/tests/vmlinuz) bytes)"

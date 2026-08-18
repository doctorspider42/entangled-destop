#!/usr/bin/env bash
# Reproducible EDK2 CloudHvX64 firmware build (backlog UEFI-1802, ADR-0003).
#
# Builds the Tianocore OvmfPkg/CloudHv target — the fw_cfg-free, PVH-entry
# OVMF variant — and copies the resulting flash image to
# artifacts/firmware/CLOUDHV.fd. Run inside WSL/Linux:
#   bash guest/firmware/build-cloudhv.sh
#
# The EDK2 tree lives in the native Linux filesystem (~/.cache/entangled-edk2)
# because BaseTools on /mnt/* (9p) is an order of magnitude slower, and the
# checkout with submodules is ~2 GiB. Firmware binaries are cached artifacts:
# never commit them (artifacts/ is gitignored), and never vendor a prebuilt
# .fd into the repo.
#
# EDK2 is BSD-2-Clause-Patent — compatible with the no-copyleft rule. The
# firmware is a *guest-side* artifact anyway: it is never linked into the host
# binary, only loaded into guest memory at runtime.
set -euo pipefail

# Pinned upstream tag. Bump deliberately: the flash layout, the PVH entry
# contract and the platform's hardware expectations are all version-visible
# (see docs/adr/0003-uefi-firmware.md).
EDK2_TAG="${EDK2_TAG:-edk2-stable202602}"
EDK2_REPO="${EDK2_REPO:-https://github.com/tianocore/edk2.git}"
# DEBUG gives us the chatty SEC/PEI/DXE progress log that UEFI-1802 needs;
# RELEASE is silent. NOOPT is for source-level debugging only.
EDK2_TARGET="${EDK2_TARGET:-DEBUG}"
TOOLCHAIN="${TOOLCHAIN:-GCC5}"

repo="$(cd "$(dirname "$0")/../.." && pwd)"
workdir="${ENTANGLED_EDK2_WORKDIR:-$HOME/.cache/entangled-edk2}"
src="$workdir/edk2"

need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "ERROR: missing build tool '$1'." >&2
        echo "  sudo apt-get install build-essential uuid-dev iasl nasm python3 git" >&2
        exit 1
    }
}
for tool in git make gcc nasm iasl python3; do need "$tool"; done

mkdir -p "$workdir"
if [ ! -d "$src/.git" ]; then
    echo ">> cloning $EDK2_TAG (shallow, with submodules; ~2 GiB)" >&2
    git clone --depth 1 --branch "$EDK2_TAG" \
        --recurse-submodules --shallow-submodules "$EDK2_REPO" "$src"
fi

cd "$src"
# Guard against a stale tree from a previous pin.
if ! git describe --tags --exact-match HEAD 2>/dev/null | grep -qx "$EDK2_TAG"; then
    echo ">> checking out $EDK2_TAG" >&2
    git fetch --depth 1 origin "refs/tags/$EDK2_TAG:refs/tags/$EDK2_TAG"
    git checkout -q "refs/tags/$EDK2_TAG"
    git submodule update --init --depth 1 --recursive
fi

echo ">> building BaseTools" >&2
make -C BaseTools -j"$(nproc)" >/dev/null

# edksetup.sh is not -u clean (it dereferences unset EDK_TOOLS_PATH etc.).
set +u
export WORKSPACE="$src"
export EDK_TOOLS_PATH="$src/BaseTools"
# shellcheck disable=SC1091
source ./edksetup.sh BaseTools >/dev/null
set -u

# -D DEBUG_ON_SERIAL_PORT routes the DebugLib to the 16550 at 0x3f8 instead of
# the Bochs/QEMU debug I/O port 0x402 (PlatformDebugLibIoPort). Our machine
# emulates the UART, not that port, so this is what makes the firmware log
# visible on ttyS0 at all.
echo ">> building CloudHvX64 ($EDK2_TARGET/$TOOLCHAIN, debug on serial)" >&2
build -a X64 -t "$TOOLCHAIN" -p OvmfPkg/CloudHv/CloudHvX64.dsc \
    -b "$EDK2_TARGET" -n "$(nproc)" -D DEBUG_ON_SERIAL_PORT

image="$src/Build/CloudHvX64/${EDK2_TARGET}_${TOOLCHAIN}/FV/CLOUDHV.fd"
[ -f "$image" ] || { echo "ERROR: expected firmware at $image" >&2; exit 1; }

out="$repo/artifacts/firmware"
mkdir -p "$out"
cp "$image" "$out/CLOUDHV.fd"
{
    echo "source:     $EDK2_REPO"
    echo "tag:        $EDK2_TAG"
    echo "commit:     $(git rev-parse HEAD)"
    echo "package:    OvmfPkg/CloudHv/CloudHvX64.dsc"
    echo "target:     $EDK2_TARGET"
    echo "toolchain:  $TOOLCHAIN"
    echo "defines:    DEBUG_ON_SERIAL_PORT"
    echo "size:       $(stat -c%s "$out/CLOUDHV.fd") bytes"
    echo "sha256:     $(sha256sum "$out/CLOUDHV.fd" | cut -d' ' -f1)"
    echo "built:      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$out/CLOUDHV.fd.provenance"

echo "wrote artifacts/firmware/CLOUDHV.fd ($(stat -c%s "$out/CLOUDHV.fd") bytes)"

# Sanity: the image must be the PVH ELF variant (edk2-stable202205+), not a
# data-only blob — our loader dispatches on exactly this.
if [ "$(head -c4 "$out/CLOUDHV.fd" | od -An -tx1 | tr -d ' \n')" != "7f454c46" ]; then
    echo "ERROR: CLOUDHV.fd is not an ELF image; the PVH entry point is missing" >&2
    exit 1
fi
echo "image sanity: PVH ELF binary"

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

# ---------------------------------------------------------------------------
# Persistent UEFI variables (UEFI-1804): move the flash window somewhere a VMM
# can actually put a device.
#
# CloudHvX64 as shipped cannot have a non-volatile variable store. Its FDF
# declares no varstore region at all — offset 0 of [FD.CLOUDHV] is the PVH ELF
# header — and it points the flash PCDs at FW_BASE_ADDRESS = 0x004FFFD0, which
# is neither the address the image is loaded at (the ELF program header says
# 0x00100000) nor page aligned, and which *is* the PVH entry point: the first
# instruction the firmware executes lives there. So `QemuFlashDetected()`
# probes guest RAM the firmware is running from, reports "FD behaves as RAM",
# `FvbServicesRuntimeDxe` unloads itself with EFI_WRITE_PROTECTED, and
# `EmuVariableFvbRuntimeDxe` serves variables out of ordinary RAM — lost on
# every stop, which is exactly the Boot#### entry an installed Ubuntu needs.
#
# Emulating flash at 0x004FFFD0 is not an option: an MMIO region cannot be the
# instruction-fetch target of the entry point, and the 4 MiB from there
# overlaps both the loaded image and the PEI/DXE working memory at 0x800000.
#
# So the two PCDs that say *where the flash is* are overridden here to
# 0xFFC00000 — the same address OvmfPkgX64 uses for its 4 MiB flash device,
# chosen so the window ends exactly at 4 GiB and stays clear of the CloudHv
# MMIO hole (0xC0000000 + 0x38000000), the IOAPIC (0xFEC00000) and the LAPIC
# (0xFEE00000). Everything else follows: the event-log, FTW-working and
# FTW-spare bases are computed *from* the variable base in the FDF, giving the
# standard layout
#
#     +0x00000  0x40000  variable store
#     +0x40000  0x01000  event log
#     +0x41000  0x01000  FTW working block
#     +0x42000  0x42000  FTW spare blocks
#     = 0x84000 (0x84 blocks of 0x1000)
#
# which `machine_x86::pflash` backs with a per-VM NVRAM file, and
# `machine_x86::layout::PFLASH_BASE` has to agree with the number below.
#
# Deliberately *not* touched: FW_BASE_ADDRESS, FW_SIZE and the [FD.CLOUDHV]
# region layout, so the image keeps its shape and the hard-coded PVH ELF header
# (load 0x00100000, entry 0x004FFFD0) still describes it; PcdCfvBase/PcdBfvBase,
# which are confidential-computing measurement inputs pointing into the image,
# not the variable store; and PcdOvmfFirmwareFdSize (0x400000), which is what
# bounds QemuFlashWrite and the range FvbInitialize adds to the GCD as runtime
# MMIO. In this tree those two PCDs have exactly one consumer,
# OvmfPkg/QemuFlashFvbServicesRuntimeDxe — the driver we want to succeed.
#
# The two values are edited into CloudHvDefines.fdf.inc rather than passed as
# `build --pcd` overrides: measured, `--pcd` is silently ignored for these (the
# FDF's own `SET` statements win, and the build does not even re-run AutoGen).
# The edit asserts its pre-image, so bumping EDK2_TAG onto a tree where these
# lines have changed fails loudly instead of quietly producing a firmware with
# RAM-only variables — and the built PCD value is verified after the build.
#
# ENTANGLED_FW_PFLASH=0 builds the stock firmware instead (RAM-only variables),
# which is what every UEFI-1801..1803 measurement was taken on.
PFLASH_BASE="${ENTANGLED_FW_PFLASH_BASE:-0xFFC00000}"
PFLASH_ENABLED="${ENTANGLED_FW_PFLASH:-1}"

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

# Always start from the pristine include, so a previous run's edit never
# compounds and ENTANGLED_FW_PFLASH=0 really does build stock firmware.
defines_inc="OvmfPkg/CloudHv/CloudHvDefines.fdf.inc"
git checkout -- "$defines_inc"
if [ "$PFLASH_ENABLED" != "0" ]; then
    echo ">> pointing the flash PCDs at $PFLASH_BASE (persistent UEFI variables)" >&2
    PFLASH_BASE="$PFLASH_BASE" python3 - "$src/$defines_inc" <<'PY'
import os
import sys

path = sys.argv[1]
base = os.environ["PFLASH_BASE"]
# Binary mode: the file is CRLF, and rewriting it as LF would turn a two-line
# change into a whole-file diff.
data = open(path, "rb").read()
edits = [
    (b"SET gUefiOvmfPkgTokenSpaceGuid.PcdOvmfFdBaseAddress     = $(FW_BASE_ADDRESS)",
     b"SET gUefiOvmfPkgTokenSpaceGuid.PcdOvmfFdBaseAddress     = " + base.encode()),
    (b"SET gUefiOvmfPkgTokenSpaceGuid.PcdOvmfFlashNvStorageVariableBase = $(FW_BASE_ADDRESS)",
     b"SET gUefiOvmfPkgTokenSpaceGuid.PcdOvmfFlashNvStorageVariableBase = " + base.encode()),
]
for old, new in edits:
    if data.count(old) != 1:
        sys.exit(
            "ERROR: %s does not contain exactly one\n  %s\n"
            "The pinned EDK2 tree changed shape; re-read the flash layout before\n"
            "trusting this build (docs/adr/0003-uefi-firmware.md)." % (path, old.decode())
        )
    data = data.replace(old, new)
open(path, "wb").write(data)
PY
fi

# -D DEBUG_ON_SERIAL_PORT routes the DebugLib to the 16550 at 0x3f8 instead of
# the Bochs/QEMU debug I/O port 0x402 (PlatformDebugLibIoPort). Our machine
# emulates the UART, not that port, so this is what makes the firmware log
# visible on ttyS0 at all.
echo ">> building CloudHvX64 ($EDK2_TARGET/$TOOLCHAIN, debug on serial)" >&2
build -a X64 -t "$TOOLCHAIN" -p OvmfPkg/CloudHv/CloudHvX64.dsc \
    -b "$EDK2_TARGET" -n "$(nproc)" -D DEBUG_ON_SERIAL_PORT

# What the firmware was actually compiled with, read back out of the generated
# header rather than assumed. `machine_x86::layout::PFLASH_BASE` must equal this
# or the firmware probes an address nothing decodes and falls back to RAM
# variables without saying so.
autogen="$src/Build/CloudHvX64/${EDK2_TARGET}_${TOOLCHAIN}/X64/OvmfPkg/QemuFlashFvbServicesRuntimeDxe/FvbServicesRuntimeDxe/DEBUG/AutoGen.h"
flash_base="$(sed -n 's/^#define _PCD_VALUE_PcdOvmfFdBaseAddress *\(0x[0-9A-Fa-f]*\)U\?$/\1/p' "$autogen" | head -1)"
if [ "$PFLASH_ENABLED" != "0" ]; then
    want="$(printf '0x%08X' "$PFLASH_BASE")"
    got="$(printf '0x%08X' "$flash_base")"
    [ "$want" = "$got" ] || {
        echo "ERROR: firmware built with flash base $got, expected $want" >&2
        echo "  (the FDF edit did not take effect; the build would have RAM-only variables)" >&2
        exit 1
    }
    echo "flash base:   $got (persistent UEFI variables, machine_x86::pflash)" >&2
else
    echo "flash base:   $flash_base (stock; UEFI variables live in RAM only)" >&2
fi

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
    echo "flash base: $flash_base ($([ "$PFLASH_ENABLED" != "0" ] && echo "pflash/NVRAM, UEFI-1804" || echo "stock, RAM-only variables"))"
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

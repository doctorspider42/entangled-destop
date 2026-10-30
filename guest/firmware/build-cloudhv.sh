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

# ---------------------------------------------------------------------------
# A GOP with a real framebuffer (ADR-0008 §1).
#
# CloudHv ships no display driver at all, so the firmware and every boot
# loader are visible on ttyS0 only. Linux does not care; Windows draws into
# the linear framebuffer GOP reports from the first instruction of
# bootmgfw.efi and keeps drawing into it after ExitBootServices. QemuVideoDxe
# (EDK2's own driver, BSD-2-Clause-Patent; the name is historical) binds the
# Bochs-DISPI display `machine_x86::framebuffer` emulates and produces exactly
# that GOP. Its mode comes from the device's EDID through three dynamic PCDs,
# which CloudHv does not declare dynamic, so they are added too.
#
# A screen is only half a console. CloudHv has no keyboard driver either, so
# its ConIn is the serial terminal alone, and a key pressed in the window
# reaches the virtio keyboard and stops there. Windows' boot media asks for
# exactly such a key ("Press any key to boot from CD or DVD"), so
# VirtioKeyboardDxe (EDK2's, same licence) comes in the same switch.
#
# ENTANGLED_FW_VIDEO=0 builds the stock component list.
VIDEO_ENABLED="${ENTANGLED_FW_VIDEO:-1}"
platform_dsc="OvmfPkg/CloudHv/CloudHvX64.dsc"
platform_fdf="OvmfPkg/CloudHv/CloudHvX64.fdf"
git checkout -- "$platform_dsc" "$platform_fdf"
if [ "$VIDEO_ENABLED" != "0" ]; then
    echo ">> adding QemuVideoDxe and VirtioKeyboardDxe (a console on the window)" >&2
    python3 - "$src/$platform_dsc" "$src/$platform_fdf" <<'PY'
import sys

dsc, fdf = sys.argv[1], sys.argv[2]

def insert_after(path, anchor, lines):
    # Binary, and in the file's own line ending: the EDK2 tree is CRLF.
    data = open(path, "rb").read()
    eol = b"\r\n" if b"\r\n" in data else b"\n"
    needle = anchor + eol
    if data.count(needle) != 1:
        sys.exit(
            "ERROR: %s does not contain exactly one line\n  %s\n"
            "The pinned EDK2 tree changed shape; re-read ADR-0008 before trusting\n"
            "this build." % (path, anchor.decode())
        )
    data = data.replace(needle, needle + b"".join(l + eol for l in lines))
    open(path, "wb").write(data)

insert_after(dsc, b"  MdeModulePkg/Universal/Console/TerminalDxe/TerminalDxe.inf",
             [b"  OvmfPkg/QemuVideoDxe/QemuVideoDxe.inf",
              b"  OvmfPkg/VirtioKeyboardDxe/VirtioKeyboard.inf"])
insert_after(dsc, b"  gUefiOvmfPkgTokenSpaceGuid.PcdPciMmio64Size|0x800000000",
             [b"  gEfiMdeModulePkgTokenSpaceGuid.PcdVideoHorizontalResolution|1280",
              b"  gEfiMdeModulePkgTokenSpaceGuid.PcdVideoVerticalResolution|800",
              b"  gUefiOvmfPkgTokenSpaceGuid.PcdVideoResolutionSource|0"])
insert_after(fdf, b"INF  MdeModulePkg/Universal/Console/TerminalDxe/TerminalDxe.inf",
             [b"INF  OvmfPkg/QemuVideoDxe/QemuVideoDxe.inf",
              b"INF  OvmfPkg/VirtioKeyboardDxe/VirtioKeyboard.inf"])
PY
fi

# ---------------------------------------------------------------------------
# Install the FACS (ADR-0008 §3).
#
# InstallCloudHvTables() hands AcpiTableDxe every table in our XSDT and the
# DSDT the FADT names, and never the FACS the FADT also names. AcpiTableDxe
# then rewrites FIRMWARE_CTRL and X_FIRMWARE_CTRL to the FACS it was given,
# which is none, so the OS sees zeroes. Cloud Hypervisor's own machine is
# hardware-reduced and needs no FACS; ours is not (PM block, SCI, global lock),
# and Windows stops on it with ACPI_BIOS_ERROR (0xA5, 0x11, 3: "cannot load
# the RSDT"). Linux only warns. The patch installs the FACS the FADT points at,
# after which AcpiTableDxe patches the FADT itself.
platform_acpi="OvmfPkg/AcpiPlatformDxe/CloudHvAcpi.c"
git checkout -- "$platform_acpi"
echo ">> installing the FADT's FACS in CloudHvAcpi.c" >&2
python3 - "$src/$platform_acpi" <<'PY'
import sys

path = sys.argv[1]
data = open(path, "rb").read()
eol = b"\r\n" if b"\r\n" in data else b"\n"
edits = [
    (b"  EFI_ACPI_DESCRIPTION_HEADER                   *DsdtTable;",
     [b"  EFI_ACPI_DESCRIPTION_HEADER                   *FacsTable = NULL;"],
     "after"),
    (b"        DsdtTable = (EFI_ACPI_DESCRIPTION_HEADER *)(UINTN)Fadt2Table->XDsdt;",
     [b"        //",
      b"        // Install the FACS the FADT points at (Entangled, ADR-0008):",
      b"        // without it AcpiTableDxe zeroes FIRMWARE_CTRL. Installed after",
      b"        // the FADT, so AcpiTableDxe patches the FADT to its copy.",
      b"        //",
      b"        FacsTable = (EFI_ACPI_DESCRIPTION_HEADER *)(UINTN)",
      b"                    (Fadt2Table->XFirmwareCtrl != 0 ?",
      b"                     Fadt2Table->XFirmwareCtrl : Fadt2Table->FirmwareCtrl);",
      b"        if (FacsTable != NULL) {",
      b"          Status = AcpiProtocol->InstallAcpiTable (",
      b"                                   AcpiProtocol,",
      b"                                   FacsTable,",
      b"                                   FacsTable->Length,",
      b"                                   &TableHandle",
      b"                                   );",
      b"          if (EFI_ERROR (Status)) {",
      b"            ASSERT_EFI_ERROR (Status);",
      b"            return Status;",
      b"          }",
      b"        }"],
     "after"),
]
for anchor, lines, where in edits:
    needle = anchor + eol
    if data.count(needle) != 1:
        sys.exit(
            "ERROR: %s does not contain exactly one line\n  %s\n"
            "The pinned EDK2 tree changed shape; re-read ADR-0008 before trusting\n"
            "this build." % (path, anchor.decode())
        )
    block = b"".join(l + eol for l in lines)
    data = data.replace(needle, needle + block if where == "after" else block + needle)
open(path, "wb").write(data)
PY

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
    echo "video:      $([ "$VIDEO_ENABLED" != "0" ] && echo "QemuVideoDxe + VirtioKeyboardDxe (ADR-0008)" || echo "none (stock)")"
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

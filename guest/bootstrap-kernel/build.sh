#!/usr/bin/env bash
# Reproducible bootstrap-kernel build (backlog MVP-1101/1102).
#
# Builds a pinned Linux kernel with all Entangled Desktop virtio drivers built in and
# copies the bzImage to artifacts/bootstrap/vmlinuz. Run inside WSL/Linux:
#   bash guest/bootstrap-kernel/build.sh
#
# The build tree lives in the native Linux filesystem (~/.cache/entangled-kernel)
# because compiling on /mnt/* (9p) is an order of magnitude slower.
set -euo pipefail

KERNEL_VERSION="${KERNEL_VERSION:-6.12.9}"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz"

repo="$(cd "$(dirname "$0")/../.." && pwd)"
fragment="$repo/guest/bootstrap-kernel/entangled.config"
workdir="${ENTANGLED_KERNEL_WORKDIR:-$HOME/.cache/entangled-kernel}"
src="$workdir/linux-$KERNEL_VERSION"

mkdir -p "$workdir"
if [ ! -d "$src" ]; then
    echo ">> downloading linux-$KERNEL_VERSION" >&2
    curl -fL --proto '=https' -o "$workdir/linux-$KERNEL_VERSION.tar.xz" "$KERNEL_URL"
    tar -C "$workdir" -xf "$workdir/linux-$KERNEL_VERSION.tar.xz"
fi

cd "$src"
make -s defconfig kvm_guest.config
# Apply the Entangled Desktop fragment and resolve dependencies.
while read -r line; do
    case "$line" in
        CONFIG_*=y) ./scripts/config --enable  "${line%%=*}" ;;
        CONFIG_*=n) ./scripts/config --disable "${line%%=*}" ;;
    esac
done < "$fragment"
make -s olddefconfig

make -s -j"$(nproc)" bzImage

mkdir -p "$repo/artifacts/bootstrap"
cp arch/x86/boot/bzImage "$repo/artifacts/bootstrap/vmlinuz"
echo "wrote artifacts/bootstrap/vmlinuz (kernel $KERNEL_VERSION, $(stat -c%s "$repo/artifacts/bootstrap/vmlinuz") bytes)"

# Sanity: confirm the fragment stuck. Both transports are checked — a kernel
# without VIRTIO_PCI boots fine and then finds no devices at all on a
# `transport = "pci"` VM, which is a confusing way to discover a missing option.
for opt in VIRTIO_MMIO PCI VIRTIO_PCI VIRTIO_BLK VIRTIO_NET VIRTIO_INPUT DRM_VIRTIO_GPU; do
    grep -q "^CONFIG_${opt}=y" .config || { echo "ERROR: CONFIG_${opt} not builtin" >&2; exit 1; }
done
echo "config sanity: both virtio transports and all drivers builtin"

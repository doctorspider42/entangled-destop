#!/usr/bin/env bash
# Reproducible bootstrap-kernel build (backlog MVP-1101/1102).
#
# Builds a pinned Linux kernel with all VMHost virtio drivers built in and
# copies the bzImage to artifacts/bootstrap/vmlinuz. Run inside WSL/Linux:
#   bash guest/bootstrap-kernel/build.sh
#
# The build tree lives in the native Linux filesystem (~/.cache/vmhost-kernel)
# because compiling on /mnt/* (9p) is an order of magnitude slower.
set -euo pipefail

KERNEL_VERSION="${KERNEL_VERSION:-6.12.9}"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz"

repo="$(cd "$(dirname "$0")/../.." && pwd)"
fragment="$repo/guest/bootstrap-kernel/vmhost.config"
workdir="${VMHOST_KERNEL_WORKDIR:-$HOME/.cache/vmhost-kernel}"
src="$workdir/linux-$KERNEL_VERSION"

mkdir -p "$workdir"
if [ ! -d "$src" ]; then
    echo ">> downloading linux-$KERNEL_VERSION" >&2
    curl -fL --proto '=https' -o "$workdir/linux-$KERNEL_VERSION.tar.xz" "$KERNEL_URL"
    tar -C "$workdir" -xf "$workdir/linux-$KERNEL_VERSION.tar.xz"
fi

cd "$src"
make -s defconfig kvm_guest.config
# Apply the VMHost fragment and resolve dependencies.
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

# Sanity: confirm the fragment stuck.
for opt in VIRTIO_MMIO VIRTIO_BLK VIRTIO_NET VIRTIO_INPUT DRM_VIRTIO_GPU; do
    grep -q "^CONFIG_${opt}=y" .config || { echo "ERROR: CONFIG_${opt} not builtin" >&2; exit 1; }
done
echo "config sanity: all virtio drivers builtin"

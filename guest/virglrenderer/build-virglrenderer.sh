#!/usr/bin/env bash
# Reproducible virglrenderer build with Venus enabled (backlog VEN-2003,
# ADR-0004).
#
# Builds the pinned upstream virglrenderer with `-Dvenus=true` and installs it
# into a cache directory outside the repository. Run inside WSL/Linux:
#
#   bash guest/virglrenderer/build-virglrenderer.sh
#
# and then point the VMM at it with the path the script prints:
#
#   export ENTANGLED_VIRGL_LIB=$HOME/.cache/entangled-virglrenderer/<tag>/lib/libvirglrenderer.so.1
#
# WHY THIS EXISTS
#   Ubuntu jammy ships libvirglrenderer 0.9.1, which predates blob resources
#   and Venus entirely: the four Venus-era entry points do not resolve and
#   `virgl_renderer_get_cap_set(VIRTIO_GPU_CAPSET_VENUS)` reports zero bytes.
#   Everything on our side of the seam has been ready since EPIC 20 phase 1;
#   the wall was the library. This script takes the wall down reproducibly
#   rather than as a thing somebody once did by hand.
#
# WHAT IS *NOT* HAPPENING HERE
#   Nothing links. `crates/virtio-gpu/src/virgl.rs` `dlopen`s the result at
#   runtime (ADR-0004 §2), so `cargo build --workspace` stays header-free on
#   both hosts and `cargo deny` still governs only the Cargo graph. Building
#   this is a *host environment* step, like installing /dev/kvm access.
#
# LICENCE
#   virglrenderer is MIT. Its runtime dependencies — libepoxy (MIT), libdrm
#   (MIT), libgbm/Mesa (MIT), the Vulkan loader (Apache-2.0) and libc — are all
#   permissive, so the built .so is redistributable beside the engine. The
#   Vulkan *driver* it finds at runtime is the host's own, exactly as the GL
#   driver already is.
set -euo pipefail

# --------------------------------------------------------------------- pins
#
# Bump deliberately. The Venus protocol version, the capset size and the
# `virgl_renderer_resource_map` contract are all version-visible, and the
# renderer probes for them at runtime rather than at build time — so a newer
# library changes what a guest negotiates, not whether the VMM compiles.
VIRGL_TAG="${VIRGL_TAG:-virglrenderer-1.1.0}"
VIRGL_COMMIT="${VIRGL_COMMIT:-1aeaf5e10a9c89096e96d09599aa419d5c50712f}"
VIRGL_REPO="${VIRGL_REPO:-https://gitlab.freedesktop.org/virgl/virglrenderer.git}"

# jammy's libvulkan-dev is 1.3.204, and virglrenderer 1.1.0's bundled
# venus-protocol headers are generated against VK_HEADER_VERSION 269: the
# build fails on `StdVideoH264LevelIdc` and friends, which only exist in the
# newer `vk_video/` headers. Supplying Vulkan-Headers at exactly the version
# venus-protocol was generated for is the fix, and it is headers only — the
# loader stays the distribution's.
VULKAN_HEADERS_TAG="${VULKAN_HEADERS_TAG:-v1.3.269}"
VULKAN_HEADERS_COMMIT="${VULKAN_HEADERS_COMMIT:-374f9fd97520f6dd1b80745de09208d878ab4a52}"
VULKAN_HEADERS_REPO="${VULKAN_HEADERS_REPO:-https://github.com/KhronosGroup/Vulkan-Headers.git}"

# The cache lives in the native Linux filesystem, never on /mnt/* — drvfs is
# slow and cannot do sparse files, and this is shared machine state that a
# second worktree should reuse rather than rebuild (dev-environment skill).
CACHE="${ENTANGLED_VIRGL_CACHE:-$HOME/.cache/entangled-virglrenderer}"
PREFIX="$CACHE/$VIRGL_TAG"
SRC="$CACHE/src/virglrenderer"
HEADERS="$CACHE/src/Vulkan-Headers"

# --------------------------------------------------------- build dependencies
need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing build tool: $1" >&2
        MISSING=1
    }
}
MISSING=0
need git
need meson
need ninja
need pkg-config
need cc
for pc in epoxy libdrm gbm vulkan; do
    pkg-config --exists "$pc" || {
        echo "missing development package for: $pc" >&2
        MISSING=1
    }
done
if [ "$MISSING" != 0 ]; then
    cat >&2 <<'EOF'

On Debian/Ubuntu:
  sudo apt install meson ninja-build pkg-config libepoxy-dev libdrm-dev \
                   libgbm-dev libegl1-mesa-dev libgles2-mesa-dev libvulkan-dev
EOF
    exit 1
fi

# ------------------------------------------------------------------- sources
clone_at() {
    local repo="$1" dir="$2" tag="$3" commit="$4"
    if [ ! -d "$dir/.git" ]; then
        mkdir -p "$(dirname "$dir")"
        git clone --depth 1 --branch "$tag" --recurse-submodules "$repo" "$dir"
    else
        git -C "$dir" fetch --depth 1 origin "refs/tags/$tag:refs/tags/$tag" || true
        git -C "$dir" checkout --force "$tag"
        git -C "$dir" submodule update --init --recursive --depth 1
    fi
    local have
    have="$(git -C "$dir" rev-parse HEAD)"
    if [ "$have" != "$commit" ]; then
        echo "pin mismatch in $dir: $tag is $have, expected $commit" >&2
        exit 1
    fi
}

clone_at "$VIRGL_REPO" "$SRC" "$VIRGL_TAG" "$VIRGL_COMMIT"
clone_at "$VULKAN_HEADERS_REPO" "$HEADERS" "$VULKAN_HEADERS_TAG" "$VULKAN_HEADERS_COMMIT"

# --------------------------------------------------------------------- build
#
# -Dvenus=true is the whole point. -Dplatforms=egl keeps the GL half exactly as
# ADR-0004 phase 1 established it (surfaceless EGL, no window system), so the
# same library serves the classic virgl path and the Venus path — which is what
# lets a guest run GNOME on virgl and Vulkan on Venus at the same time.
# -Dtests/-Dfuzzer stay off: they pull in check/GTest and build nothing we load.
rm -rf "$SRC/build"
meson setup "$SRC/build" "$SRC" \
    --prefix "$PREFIX" \
    --buildtype release \
    -Dvenus=true \
    -Dplatforms=egl \
    -Dc_args="-I$HEADERS/include"
ninja -C "$SRC/build"
ninja -C "$SRC/build" install

LIB="$(find "$PREFIX" -name 'libvirglrenderer.so.1' -print -quit)"
if [ -z "$LIB" ]; then
    echo "build produced no libvirglrenderer.so.1 under $PREFIX" >&2
    exit 1
fi

# ------------------------------------------------------------- what we built
#
# Printed, not asserted: a host whose Mesa is older or newer will report
# different capset sizes, and the renderer probes at runtime anyway. What the
# script *does* insist on is that the four Venus entry points are exported,
# because a library without them is one this work cannot use and the failure
# would otherwise show up as a guest that silently gets classic virgl.
echo
echo "virglrenderer $VIRGL_TAG installed:"
echo "  $LIB"
echo "  soname deps: $(readelf -d "$LIB" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p' | tr '\n' ' ')"
missing=0
for sym in virgl_renderer_context_create_with_flags \
    virgl_renderer_resource_create_blob \
    virgl_renderer_resource_map \
    virgl_renderer_resource_unmap; do
    if nm -D --defined-only "$LIB" | grep -q " $sym\$"; then
        echo "  symbol $sym: yes"
    else
        echo "  symbol $sym: MISSING" >&2
        missing=1
    fi
done
[ "$missing" = 0 ] || exit 1

cat <<EOF

Use it with:
  export ENTANGLED_VIRGL_LIB=$LIB

Everything else — whether the Venus capset is non-empty, whether a host Vulkan
ICD exists and what it is — the renderer probes at load time and logs. Check it
with:
  ENTANGLED_VIRGL_LIB=$LIB cargo test -p virtio-gpu --test virgl_host -- --nocapture
EOF

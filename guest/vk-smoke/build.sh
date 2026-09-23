#!/usr/bin/env bash
# Builds the guest binary of vk-smoke in WSL Ubuntu (x86_64 glibc; a binary
# built against 22.04's glibc 2.35 runs on any newer guest, Ubuntu 26.04
# included). See README.md.
#
#   From Windows:  wsl -d Ubuntu -e bash /mnt/f/<checkout>/guest/vk-smoke/build.sh [DEST_DIR]
#   Inside WSL:    bash guest/vk-smoke/build.sh [DEST_DIR]
#
# DEST_DIR (optional) receives a copy of the binary, e.g.
# /mnt/f/VMs/Entangled/probes/vk-smoke — the directory the host serves to the
# guest from.
#
# Environment:
#   CARGO_TARGET_DIR   default /mnt/f/cargo-targets/vk-smoke — never in the
#                      repo (D: is small) and never in the WSL VHDX (it only
#                      grows). Delete it when you are done.
#   JOBS               cargo -j, default 6.
#   NO_WAIT=1          fail instead of waiting when another cargo is running.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/mnt/f/cargo-targets/vk-smoke}"
jobs="${JOBS:-6}"
dest="${1:-}"

# This machine has crashed under concurrent build load: never start a build
# while another cargo/rustc runs in WSL. Wait for it instead.
while busy="$( (pgrep -a -x cargo; pgrep -a -x rustc) || true)"; [ -n "$busy" ]; do
    if [ "${NO_WAIT:-0}" = 1 ]; then
        echo "build.sh: another cargo/rustc is running, NO_WAIT=1 set:" >&2
        echo "$busy" | cut -c1-160 >&2
        exit 75
    fi
    echo "build.sh: waiting for another cargo/rustc to finish:"
    echo "$busy" | cut -c1-160
    sleep 20
done

command -v cargo >/dev/null || { echo "build.sh: cargo not found (looked in \$HOME/.cargo/bin and PATH)" >&2; exit 1; }
cd "$here"
cargo build --release --locked -j "$jobs"

bin="$CARGO_TARGET_DIR/release/vk-smoke"
echo "built: $bin"
ls -l "$bin"
sha256sum "$bin"
if command -v objdump >/dev/null; then
    echo "needs glibc: $(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
fi
if [ -n "$dest" ]; then
    mkdir -p "$dest"
    cp "$bin" "$dest/vk-smoke"
    chmod +x "$dest/vk-smoke"
    echo "copied to: $dest/vk-smoke"
fi

#!/usr/bin/env bash
# Builds the Windows XP guest drivers (ADR-0013): 32-bit, freestanding C,
# compiled with clang and linked with lld-link.
#
# No Windows XP DDK is involved. The DDI comes from the Windows SDK's own
# headers (winddi.h, video.h still describe the XP driver model), and the
# import libraries for videoprt.sys and win32k.sys are generated here from
# the .def files beside the sources: an import library is only a list of
# names.
#
#   guest/winxp/build.sh [OUT_DIR]        # default: guest/winxp/out
#
# Needs clang, lld-link and llvm-dlltool on PATH (any recent LLVM), and a
# Windows SDK with the kernel-mode headers (the WDK's `km` directory).
# WINDOWS_KITS_INCLUDE names the versioned include directory when it is not
# found under "Program Files (x86)/Windows Kits/10/Include".
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:-$here/out}"
mkdir -p "$out"

kits="${WINDOWS_KITS_INCLUDE:-}"
if [ -z "$kits" ]; then
    for candidate in "/c/Program Files (x86)/Windows Kits/10/Include"/*/; do
        if [ -f "$candidate/km/video.h" ] && [ -f "$candidate/um/winddi.h" ]; then
            kits="${candidate%/}"
        fi
    done
fi
if [ -z "$kits" ]; then
    echo "no Windows SDK with km/video.h and um/winddi.h found; set WINDOWS_KITS_INCLUDE" >&2
    exit 1
fi
echo "headers: $kits"

# -mrtd: the kernel's default calling convention is stdcall, and the DDI
# headers declare their callbacks without naming one.
cflags=(--target=i686-pc-windows-msvc -c -O2 -mrtd -ffreestanding -fno-stack-protector
        -fno-builtin -Wall -Wno-unknown-pragmas -Wno-pragma-pack -Wno-microsoft-anon-tag
        -Wno-ignored-attributes -Wno-unused-function -Wno-missing-prototype-for-cc
        -Wno-nonportable-include-path -Wno-ignored-pragma-intrinsic
        -Wno-deprecated-declarations
        -D_X86_=1 -Di386=1 -D_WIN32_WINNT=0x0501 -DWINVER=0x0501 -DNTDDI_VERSION=0x05010000
        -I"$kits/km" -I"$kits/shared" -I"$kits/um" -I"$kits/km/crt")

# Image headers XP's loader accepts: subsystem version 5.01, a checksum
# (the kernel loader verifies it), relocations kept.
lflags=(-machine:x86 -nodefaultlib -release -subsystem:native,5.01 -osversion:5.1)

lib() {
    llvm-dlltool -m i386 -k -d "$here/display/$1.def" -l "$out/$1.lib"
}

echo "entxp.sys (video miniport)"
lib videoprt
clang "${cflags[@]}" "$here/display/miniport.c" -o "$out/miniport.obj"
lld-link "${lflags[@]}" -driver -entry:DriverEntry@8 -out:"$out/entxp.sys" \
    "$out/miniport.obj" "$out/videoprt.lib"

echo "entxp.dll (display driver)"
lib win32k
clang "${cflags[@]}" "$here/display/display.c" -o "$out/display.obj"
lld-link "${lflags[@]}" -dll -entry:DrvEnableDriver@12 -export:DrvEnableDriver=_DrvEnableDriver@12 \
    -out:"$out/entxp.dll" "$out/display.obj" "$out/win32k.lib"

echo "entxp-setup.exe (driver installer)"
libs="${kits/Include/Lib}/um/x86"
setup_cflags=(--target=i686-pc-windows-msvc -c -O2 -ffreestanding -fno-stack-protector
              -fno-builtin -Wall -Wno-pragma-pack -Wno-nonportable-include-path
              -Wno-ignored-attributes -Wno-ignored-pragma-intrinsic -Wno-microsoft-anon-tag
              -D_WIN32_WINNT=0x0501 -DWINVER=0x0501 -I"$kits/um" -I"$kits/shared")
clang "${setup_cflags[@]}" "$here/setup/setup.c" -o "$out/setup.obj"
lld-link -machine:x86 -nodefaultlib -release -subsystem:console,5.01 -osversion:5.1 \
    -entry:entry -out:"$out/entxp-setup.exe" "$out/setup.obj" \
    "$libs/kernel32.lib" "$libs/newdev.lib"

cp "$here/display/entxp.inf" "$out/entxp.inf"
rm -f "$out"/*.obj "$out"/*.lib "$out"/*.exp
ls -l "$out"

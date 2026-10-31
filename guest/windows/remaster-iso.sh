#!/usr/bin/env bash
# Remasters a Windows installation ISO so Windows Setup can install onto an
# Entangled VM's virtio disk unattended (ADR-0008 §4).
#
#   bash guest/windows/remaster-iso.sh WINDOWS.iso VIRTIO-WIN.iso OUT.iso|OUT.img [INDEX]
#
# OUT.img writes a FAT32 installation disk instead of an ISO; that is the form
# Setup accepts on a virtio-blk device (see the end of this script).
#
# Windows PE has no virtio drivers, so it sees no virtio-blk device at all:
# not the target disk, not the virtio-win ISO, not an answer-file volume, and
# not even the installation ISO it booted from (the firmware read that, not
# Windows). The one place Setup can find anything before a driver loads is
# its own boot.wim. So this script puts the virtio-win drivers into image 2
# of boot.wim as X:\drivers, and autounattend.xml as X:\sources\
# Autounattend.xml, which Setup's implicit answer-file search reads in the
# windowsPE pass. The answer file loads the drivers from there.
#
# It also boots through efisys_noprompt.bin, so there is no "Press any key to
# boot from CD or DVD" prompt to answer.
#
# INDEX is the edition in install.esd/install.wim (6 = Pro on a Windows 10
# consumer ISO; `wimlib-imagex info` lists them). Needs wimtools, p7zip-full
# and genisoimage (an ISO) or dosfstools, mtools and fdisk (a disk). Development tooling: nothing here is linked into, or
# shipped with, the host binaries.
set -euo pipefail

windows_iso="$1"
virtio_iso="$2"
out_iso="$3"
index="${4:-6}"
# The Windows time zone name, e.g. "Central European Standard Time".
timezone="${ENTANGLED_WIN_TIMEZONE:-UTC}"
here="$(cd "$(dirname "$0")" && pwd)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo ">> extracting $windows_iso" >&2
7z x -y -o"$work/iso" "$windows_iso" >/dev/null

echo ">> extracting the Windows 10 x64 drivers from $virtio_iso" >&2
mkdir -p "$work/drivers"
for driver in viostor vioscsi NetKVM vioinput Balloon; do
    7z x -y -o"$work/vw" "$virtio_iso" "$driver/w10/amd64" >/dev/null
    cp -r "$work/vw/$driver/w10/amd64" "$work/drivers/$driver"
done
# Debug symbols are a third of the size and nothing Setup reads.
find "$work/drivers" -name '*.pdb' -delete

case "$out_iso" in
    *.img) disk=1 ;;
    *) disk=0 ;;
esac

if [ "$disk" = 1 ]; then
    # FAT32 holds no file of 4 GiB or more, and install.esd is larger. Keep
    # only the chosen edition, as a split WIM Setup reads natively
    # (sources\install.swm, install2.swm, ...). It is image 1 from now on.
    echo ">> exporting image $index of install.esd as a split WIM" >&2
    wimlib-imagex export "$work/iso/sources/install.esd" "$index" \
        "$work/install.wim" --compress=LZX >/dev/null
    rm "$work/iso/sources/install.esd"
    wimlib-imagex split "$work/install.wim" "$work/iso/sources/install.swm" 3800 >/dev/null
    rm "$work/install.wim"
    index=1
fi

sed -e "s/@INDEX@/$index/" -e "s/@TIMEZONE@/$timezone/" "$here/autounattend.xml" > "$work/Autounattend.xml"

echo ">> adding drivers and answer file to boot.wim image 2" >&2
wimupdate "$work/iso/sources/boot.wim" 2 <<EOF
add $work/drivers /drivers
add $work/Autounattend.xml /sources/Autounattend.xml
EOF
cp "$work/Autounattend.xml" "$work/iso/Autounattend.xml"

if [ "$disk" = 1 ]; then
    # A disk Windows treats as installation media, like a USB stick: an MBR
    # with one FAT32 partition holding the whole tree. Setup does not take a
    # UDF image on a virtio-blk *disk* as its media ("A media driver your
    # computer needs is missing"), even with viostor loaded. The firmware
    # boots it through \EFI\BOOT\BOOTX64.EFI like any removable FAT volume.
    echo ">> writing the FAT32 installation disk $out_iso" >&2
    content_mib=$(( $(du -sm "$work/iso" | cut -f1) ))
    size_mib=$(( content_mib + content_mib / 10 + 64 ))
    rm -f "$out_iso"
    truncate -s "${size_mib}M" "$out_iso"
    echo 'start=2048, type=c, bootable' | sfdisk -q "$out_iso"
    part_sectors=$(( size_mib * 2048 - 2048 ))
    mkfs.vfat -F 32 -n WINSETUP --offset 2048 "$out_iso" $(( part_sectors / 2 )) >/dev/null
    MTOOLS_SKIP_CHECK=1 mcopy -s -i "$out_iso@@1M" "$work/iso/"* ::/
    echo "wrote $out_iso ($(stat -c%s "$out_iso") bytes)"
    exit 0
fi

echo ">> writing $out_iso" >&2
# UDF, because install.esd is larger than ISO 9660's 4 GiB file limit and
# Windows mounts UDF on a disk device (our CD-ROM is a virtio-blk disk).
# EDK2's El Torito reader takes the no-emulation entry as the EFI system
# partition; efisys_noprompt.bin is a 1.44 MB FAT image, 2880 sectors.
genisoimage -quiet -udf -allow-limited-size -iso-level 3 -J -joliet-long -D -N \
    -relaxed-filenames -V "ENTANGLED_WIN" \
    -b efi/microsoft/boot/efisys_noprompt.bin -no-emul-boot -boot-load-size 2880 \
    -o "$out_iso" "$work/iso"
echo "wrote $out_iso ($(stat -c%s "$out_iso") bytes)"

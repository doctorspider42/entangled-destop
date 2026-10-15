#!/bin/sh
# Run inside an Ubuntu ARM64 guest, never on the macOS host.
# Current macOS Venus presentation uses software WSI, without dma-buf export.
set -eu

if [ "$(id -u)" != 0 ] || [ "$(uname -m)" != aarch64 ]; then
    echo "Run as root inside the Ubuntu ARM64 guest." >&2
    exit 1
fi
if ! grep -q '^ID=ubuntu$' /etc/os-release; then
    echo "This configuration is for Ubuntu." >&2
    exit 1
fi
if [ ! -f /usr/share/vulkan/icd.d/virtio_icd.json ]; then
    echo "Install Ubuntu mesa-vulkan-drivers before configuring Venus." >&2
    exit 1
fi

# Preserve other environment settings, and replace only our previous block.
# GDM reads /etc/environment for the X11 desktop session and its applications.
task_environment=$(mktemp /etc/environment.entangled.XXXXXX)
trap 'rm -f "$task_environment"' EXIT HUP INT TERM
if [ -f /etc/environment ]; then
    awk '
        /^# BEGIN ENTANGLED VENUS$/ { managed = 1; next }
        /^# END ENTANGLED VENUS$/ { managed = 0; next }
        !managed { print }
    ' /etc/environment > "$task_environment"
fi
cat >> "$task_environment" <<'ENV'
# BEGIN ENTANGLED VENUS
GALLIUM_DRIVER=zink
MESA_LOADER_DRIVER_OVERRIDE=zink
LIBGL_KOPPER_DRI2=true
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json
# END ENTANGLED VENUS
ENV
chmod 644 "$task_environment"
mv "$task_environment" /etc/environment

# Xorg Glamor needs a shareable scanout buffer and fails ScreenInit on the
# current driver. Keep the modesetting display, while GNOME and GL clients
# render through Zink/Venus using its software WSI presentation path.
mkdir -p /etc/X11/xorg.conf.d
cat > /etc/X11/xorg.conf.d/20-entangled-modesetting.conf <<'XORG'
Section "Device"
    Identifier "Entangled virtio GPU"
    Driver "modesetting"
    Option "AccelMethod" "none"
EndSection
XORG
echo "Venus desktop configured. Start a new X11 login session to apply it."

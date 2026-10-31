#!/bin/bash
# Run in the GNOME terminal of an exclusively owned test guest, with Mesa
# utilities, glmark2 and vulkan-tools installed. Copy locally and unmount the
# 9p staging share before capture. The probe user needs write access to ttyAMA0.
# start: continuous workload + serial identity/heartbeat; verify PHASE: new GL/VK work.
set -eu
if [ "$(uname -m)" != aarch64 ] || [ -z "${DISPLAY:-}" ] \
    || ! grep -aq 'Entangled ARM64' /sys/firmware/devicetree/base/model; then
    echo "Run inside an Entangled ARM64 GNOME test guest." >&2
    exit 2
fi
for tool in glxinfo glmark2 vulkaninfo flock; do command -v "$tool" >/dev/null; done
[ -w /dev/ttyAMA0 ]
work=/tmp/entangled-gpu-probe
mkdir -p "$work"
emit() { printf '%s\n' "$*" | tee -a "$work/probe.log" > /dev/ttyAMA0; }
identity() {
    emit "GPU_ID phase=$1 boot=$(cat /proc/sys/kernel/random/boot_id) shell=$(pgrep -u "$(id -u)" -x gnome-shell | head -1) display=$DISPLAY"
    if timeout 30 glxinfo -B > "$work/glx-$1.log" 2>&1; then
        while IFS= read -r line; do emit "GPU_GLX phase=$1 $line"; done < "$work/glx-$1.log"
    else emit "GPU_GLX_FAILED phase=$1"; return 1; fi
    if command -v vulkaninfo >/dev/null; then
        if timeout 30 vulkaninfo --summary > "$work/vulkan-$1.log" 2>&1; then
            while IFS= read -r line; do emit "GPU_VK phase=$1 $line"; done < "$work/vulkan-$1.log"
        else emit "GPU_VK_FAILED phase=$1"; return 1; fi
    fi
}
if [ "${1:-start}" = verify ]; then
    identity "$2"
    if timeout 30 glmark2 --off-screen --size 256x256 -b build:duration=2 > "$work/fresh-$2.log" 2>&1; then
        while IFS= read -r line; do emit "GPU_FRESH phase=$2 $line"; done < "$work/fresh-$2.log"
        emit "GPU_FRESH_OK phase=$2"
    else emit "GPU_FRESH_FAILED phase=$2"; exit 1; fi
    exit
fi
# A host may retry starting the probe while GNOME is still becoming ready.
# Keep one original workload; duplicate starts must not invalidate its witness.
exec 9> "$work/run.lock"
if ! flock -n 9; then emit "GPU_PROBE_ALREADY_RUNNING"; exit 0; fi
# Long acceptance runs must remain controllable through the guest terminal.
# This script's machine/model guard limits these settings to the owned test VM.
gsettings set org.gnome.desktop.session idle-delay 0
gsettings set org.gnome.desktop.screensaver lock-enabled false
identity before
boot=$(cat /proc/sys/kernel/random/boot_id)
compositor=$(pgrep -u "$(id -u)" -x gnome-shell | head -1)
nonce="witness-$$-$(date +%s)"
stdbuf -oL -eL glmark2 --run-forever --size 640x480 -b build:duration=1 -b texture:duration=1 -b shading:shading=phong:duration=1 > "$work/glmark.log" 2>&1 &
gpu=$!
start=$(awk '{print $22}' /proc/$gpu/stat)
emit "GPU_SESSION nonce=$nonce boot=$boot compositor=$compositor glmark=$gpu start=$start"
n=0
while kill -0 "$gpu" 2>/dev/null; do
    n=$((n+1))
    frames=$(grep -c 'FPS:' "$work/glmark.log" || true)
    ticks=$(awk '{print $14+$15}' /proc/$gpu/stat)
    emit "GPU_WITNESS n=$n nonce=$nonce boot=$boot compositor=$compositor glmark=$gpu start=$start fps_lines=$frames cpu_ticks=$ticks"
    if [ "$frames" -gt 0 ]; then emit "GPU_LAST $(grep 'FPS:' "$work/glmark.log" | tail -1)"; fi
    sleep 2
done
emit "GPU_WORKLOAD_EXIT glmark=$gpu"
wait "$gpu"

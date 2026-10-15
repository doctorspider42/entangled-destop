#!/bin/sh
# Opt-in test-only init for an exclusively owned Ubuntu image. Mount this
# directory read-only over 9p; never use this init on someone's working VM.
set -eu
expected=$1
case "$expected" in ''|*[!0-9]*) exit 2;; esac
[ "$expected" -ge 1 ] && [ "$expected" -le 64 ]
trap 'sync; mount -o remount,ro / || true; /sbin/poweroff -f' EXIT
echo "SMP_ACCEPTANCE_BEGIN expected=$expected"
actual=$(getconf _NPROCESSORS_ONLN)
echo "SMP_ONLINE actual=$actual affinity=$(nproc)"
[ "$actual" -eq "$expected" ]
cat /sys/devices/system/cpu/online
cat /proc/interrupts
if [ "$expected" -gt 1 ]; then
    for round in 1 2; do
        cpu=1
        while [ "$cpu" -lt "$expected" ]; do
            echo 0 > "/sys/devices/system/cpu/cpu$cpu/online"
            [ "$(getconf _NPROCESSORS_ONLN)" -eq "$((expected - 1))" ]
            echo 1 > "/sys/devices/system/cpu/cpu$cpu/online"
            [ "$(getconf _NPROCESSORS_ONLN)" -eq "$expected" ]
            echo "SMP_HOTPLUG_OK cpu=$cpu round=$round"
            cpu=$((cpu + 1))
        done
    done
    if [ -w /sys/devices/system/cpu/cpu0/online ]; then
        echo 0 > /sys/devices/system/cpu/cpu0/online
        [ "$(getconf _NPROCESSORS_ONLN)" -eq "$((expected - 1))" ]
        echo 1 > /sys/devices/system/cpu/cpu0/online
        [ "$(getconf _NPROCESSORS_ONLN)" -eq "$expected" ]
        echo SMP_BOOT_CPU_HOTPLUG_OK
    fi
fi
openssl version
echo "SMP_BENCH_BEGIN workers=$expected"
openssl speed -seconds 3 -elapsed -multi "$expected" -evp SHA256 -bytes 16384
echo SMP_BENCH_END
cat /proc/stat
echo "SMP_ACCEPTANCE_OK cpus=$expected"

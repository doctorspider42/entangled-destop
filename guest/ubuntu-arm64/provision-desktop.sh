#!/bin/sh
# Runs as init inside the new, verified Ubuntu rootfs. Arguments are supplied
# by the native installer, never sourced from a host user's shell config.
set -eu
epoch=$1
marker=$2
hostname=$3
guest_ip=$4
gateway=$5
trap 'sync; mount -o remount,ro / || true; /sbin/poweroff -f' EXIT
date -s "@$epoch"
resize2fs /dev/vda
mkdir -p /run/systemd/resolve
printf 'nameserver %s\n' "$gateway" > /run/systemd/resolve/stub-resolv.conf
ip link set eth0 up
ip addr replace "$guest_ip/24" dev eth0
ip route replace default via "$gateway"
export DEBIAN_FRONTEND=noninteractive
apt-get -o Acquire::Retries=2 -o Acquire::http::Timeout=30 update
apt-get -o Acquire::Retries=2 -o Acquire::http::Timeout=30 install -y --no-install-recommends \
    ubuntu-desktop-minimal gnome-terminal mesa-vulkan-drivers mesa-utils glmark2 xserver-xorg-video-fbdev
id entangled >/dev/null 2>&1 || useradd -m -s /bin/bash -G sudo,video,render,input entangled
echo 'entangled:entangled' | chpasswd
printf '%s\n' "$hostname" > /etc/hostname
printf '127.0.0.1 localhost\n127.0.1.1 %s\n' "$hostname" > /etc/hosts
mkdir -p /etc/systemd/network
cat > /etc/systemd/network/10-entangled.network <<'NET'
[Match]
Name=eth0
[Network]
DHCP=yes
NET
touch /etc/cloud/cloud-init.disabled
mkdir -p /etc/gdm3
cat > /etc/gdm3/custom.conf <<'GDM'
[daemon]
AutomaticLoginEnable=true
AutomaticLogin=entangled
WaylandEnable=false
GDM
systemctl enable systemd-networkd systemd-resolved gdm3
systemctl set-default graphical.target
/bin/sh /run/entangled/configure-venus-desktop.sh
printf '%s\n' "$marker"

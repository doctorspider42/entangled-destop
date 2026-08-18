#!/usr/bin/env bash
# Host network setup for Entangled Desktop's virtio-net device (backlog MVP-506/507).
#
# Creates a persistent TAP interface owned by an unprivileged user, gives it a
# host-side address and connects the VM either to a NAT gateway (default) or to
# an existing bridge. `--down` reverses everything it did.
#
# WHY THIS SCRIPT EXISTS (privileges)
# ===================================
# Attaching to a TAP interface with TUNSETIFF needs CAP_NET_ADMIN *only when the
# interface does not exist yet*. Creating it here once, owned by the user who
# will run `entangled`, means the VMM itself runs unprivileged for the whole life of
# the VM — the MVP's security posture: no capabilities on the process that talks
# to an untrusted guest.
#
# This script therefore needs root (or CAP_NET_ADMIN, and CAP_NET_RAW-equivalent
# privileges for nftables/iptables). `entangled run` does not.
#
# USAGE
# =====
#   sudo scripts/setup-tap.sh                        # entangled0, NAT, current user
#   sudo scripts/setup-tap.sh --iface entangled1 --user alice
#   sudo scripts/setup-tap.sh --bridge br0           # bridge instead of NAT
#   sudo scripts/setup-tap.sh --down                 # tear it all down
#   scripts/setup-tap.sh --dnsmasq                   # print a DHCP snippet
#
# GUEST SIDE
# ==========
# The MVP does not ship a DHCP server: with NAT the guest can be configured
# statically (address 192.168.73.2/24, gateway 192.168.73.1) or served by
# dnsmasq — `--dnsmasq` prints a ready configuration snippet, which is
# documentation, not a requirement. A boot-time `ip=dhcp` kernel cmdline (used by
# the boot integration tests) needs such a server on the host side.
set -euo pipefail

IFACE=entangled0
# TAP owner: SUDO_USER when invoked via sudo; overridable with ENTANGLED_TAP_OWNER
# for environments without sudo context (e.g. `wsl -u root -- …`, where
# defaulting to root would leave the unprivileged VMM unable to attach).
OWNER=${ENTANGLED_TAP_OWNER:-${SUDO_USER:-$(id -un)}}
HOST_IP=192.168.73.1/24
SUBNET=
UPLINK=
BRIDGE=
ACTION=up

usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

die() {
    echo "setup-tap: $*" >&2
    exit 1
}

need_root() {
    [ "$(id -u)" -eq 0 ] || die "must run as root (needs CAP_NET_ADMIN); try sudo"
}

have() {
    command -v "$1" >/dev/null 2>&1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --iface) IFACE=${2:?--iface needs a value}; shift 2 ;;
        --user) OWNER=${2:?--user needs a value}; shift 2 ;;
        --host-ip) HOST_IP=${2:?--host-ip needs a value}; shift 2 ;;
        --subnet) SUBNET=${2:?--subnet needs a value}; shift 2 ;;
        --uplink) UPLINK=${2:?--uplink needs a value}; shift 2 ;;
        --bridge) BRIDGE=${2:?--bridge needs a value}; shift 2 ;;
        --down) ACTION=down; shift ;;
        --dnsmasq) ACTION=dnsmasq; shift ;;
        -h|--help) usage 0 ;;
        *) echo "setup-tap: unknown argument $1" >&2; usage 1 ;;
    esac
done

# IFNAMSIZ is 16 including the NUL terminator, and the name reaches the kernel
# through an ioctl, so validate it here rather than truncating silently.
case "$IFACE" in
    ''|*[!A-Za-z0-9._-]*) die "invalid interface name '$IFACE'" ;;
esac
[ "${#IFACE}" -le 15 ] || die "interface name '$IFACE' is longer than 15 characters"

ADDR=${HOST_IP%/*}
PREFIX=${HOST_IP##*/}
if [ -z "$SUBNET" ]; then
    if [ "$PREFIX" != 24 ]; then
        die "--host-ip with a /$PREFIX prefix needs an explicit --subnet"
    fi
    SUBNET="${ADDR%.*}.0/24"
fi

default_uplink() {
    ip -4 route show default 2>/dev/null | awk '{ for (i = 1; i < NF; i++) if ($i == "dev") { print $(i + 1); exit } }'
}

# ---------------------------------------------------------------- dnsmasq doc

if [ "$ACTION" = dnsmasq ]; then
    cat <<EOF
# Optional: serve DHCP + DNS to the VM on $IFACE (install the 'dnsmasq' package).
# Write this to /etc/dnsmasq.d/entangled-$IFACE.conf and restart dnsmasq, or run it
# in the foreground as shown at the bottom. Not required by \`entangled run\`: a
# statically configured guest works just as well.
interface=$IFACE
bind-interfaces
except-interface=lo
dhcp-range=${ADDR%.*}.50,${ADDR%.*}.150,12h
dhcp-option=option:router,$ADDR
dhcp-option=option:dns-server,$ADDR
# No DNS forwarding surprises: answer only for the VM subnet.
domain=entangled.invalid
local=/entangled.invalid/

# Foreground equivalent, handy for a one-off boot test with ip=dhcp:
#   sudo dnsmasq --no-daemon --interface=$IFACE --bind-interfaces \\
#        --dhcp-range=${ADDR%.*}.50,${ADDR%.*}.150,12h \\
#        --dhcp-option=option:router,$ADDR
EOF
    exit 0
fi

# ------------------------------------------------------------------ teardown

if [ "$ACTION" = down ]; then
    need_root
    if have nft && nft list table inet entangled >/dev/null 2>&1; then
        nft delete table inet entangled
        echo "removed nftables table inet entangled"
    fi
    if have iptables; then
        UPLINK=${UPLINK:-$(default_uplink || true)}
        iptables -t nat -D POSTROUTING -s "$SUBNET" ! -o "$IFACE" -j MASQUERADE 2>/dev/null || true
        if [ -n "$UPLINK" ]; then
            iptables -D FORWARD -i "$IFACE" -o "$UPLINK" -j ACCEPT 2>/dev/null || true
            iptables -D FORWARD -i "$UPLINK" -o "$IFACE" \
                -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT 2>/dev/null || true
        fi
    fi
    if ip link show "$IFACE" >/dev/null 2>&1; then
        # A TAP still open by a running VM is removed as soon as it is closed;
        # deleting the persistent interface here is what makes teardown final.
        ip link set "$IFACE" down || true
        ip tuntap del dev "$IFACE" mode tap
        echo "removed TAP interface $IFACE"
    else
        echo "TAP interface $IFACE was not present"
    fi
    echo "note: net.ipv4.ip_forward was left as it is (it may be shared with other tools)"
    exit 0
fi

# ---------------------------------------------------------------------- setup

need_root
have ip || die "the 'ip' command (iproute2) is required"
id -u "$OWNER" >/dev/null 2>&1 || die "user '$OWNER' does not exist"

if ip link show "$IFACE" >/dev/null 2>&1; then
    echo "TAP interface $IFACE already exists, reusing it"
else
    ip tuntap add dev "$IFACE" mode tap user "$OWNER"
    echo "created TAP interface $IFACE owned by $OWNER"
fi
ip link set "$IFACE" up

if [ -n "$BRIDGE" ]; then
    # Bridged mode: the VM appears on the physical LAN and gets its address from
    # whatever serves DHCP there. No NAT, no forwarding rules, no host address.
    ip link show "$BRIDGE" >/dev/null 2>&1 || die "bridge '$BRIDGE' does not exist"
    ip link set "$IFACE" master "$BRIDGE"
    echo "attached $IFACE to bridge $BRIDGE"
    echo
    echo "Run the VM as $OWNER with:  entangled run --net-tap $IFACE"
    exit 0
fi

# NAT mode.
if ip -4 addr show dev "$IFACE" | grep -q "inet $ADDR/"; then
    echo "host address $HOST_IP already on $IFACE"
else
    ip addr add "$HOST_IP" dev "$IFACE"
    echo "added host address $HOST_IP to $IFACE"
fi

UPLINK=${UPLINK:-$(default_uplink || true)}
[ -n "$UPLINK" ] || die "cannot determine the uplink interface; pass --uplink"
echo "using uplink $UPLINK"

sysctl -q -w net.ipv4.ip_forward=1
echo "enabled net.ipv4.ip_forward"

if have nft; then
    # A dedicated table so teardown is a single `nft delete table` and nothing
    # this script did can disturb another tool's ruleset.
    nft list table inet entangled >/dev/null 2>&1 && nft delete table inet entangled
    nft -f - <<EOF
table inet entangled {
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        ip saddr $SUBNET oifname != "$IFACE" masquerade
    }
    chain forward {
        type filter hook forward priority filter; policy accept;
        # MSS clamp to path MTU: the guest sees MTU 1500 on the TAP, but the
        # uplink can be smaller (WSL2 eth0 is 1472); without clamping, bulk
        # TCP from the internet stalls mid-transfer (observed: d-i dying on
        # "Downloading Release files" and blaming the mirror).
        tcp flags syn tcp option maxseg size set rt mtu
        iifname "$IFACE" oifname "$UPLINK" accept
        iifname "$UPLINK" oifname "$IFACE" ct state established,related accept
    }
}
EOF
    echo "installed nftables table inet entangled (NAT $SUBNET -> $UPLINK)"
elif have iptables; then
    iptables -t nat -C POSTROUTING -s "$SUBNET" ! -o "$IFACE" -j MASQUERADE 2>/dev/null ||
        iptables -t nat -A POSTROUTING -s "$SUBNET" ! -o "$IFACE" -j MASQUERADE
    iptables -C FORWARD -i "$IFACE" -o "$UPLINK" -j ACCEPT 2>/dev/null ||
        iptables -A FORWARD -i "$IFACE" -o "$UPLINK" -j ACCEPT
    iptables -C FORWARD -i "$UPLINK" -o "$IFACE" \
        -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT 2>/dev/null ||
        iptables -A FORWARD -i "$UPLINK" -o "$IFACE" \
            -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
    echo "installed iptables NAT rules ($SUBNET -> $UPLINK)"
else
    die "neither nft nor iptables is available; install one or use --bridge"
fi

cat <<EOF

TAP $IFACE is ready.
  host address : $HOST_IP
  guest subnet : $SUBNET   (gateway $ADDR)
  owner        : $OWNER    (may attach without CAP_NET_ADMIN)

Run the VM as $OWNER with:  entangled run --net-tap $IFACE
Guest static configuration:  address ${ADDR%.*}.2/$PREFIX, gateway $ADDR
For DHCP instead:            scripts/setup-tap.sh --iface $IFACE --dnsmasq
Tear everything down with:   sudo scripts/setup-tap.sh --iface $IFACE --down

Caveat: another firewall (Docker, ufw, firewalld) may still drop forwarded
traffic through its own base chains; this script only adds its own accepts.
EOF

# Added by entangled install (EPIC 13): start Weston on the autologin console.
if [ -z "$WAYLAND_DISPLAY" ] && [ "$(tty)" = "/dev/tty1" ]; then
    # Minimal-footprint session: without libpam-systemd there is no logind
    # session and no XDG_RUNTIME_DIR, which Weston hard-requires.
    if [ -z "$XDG_RUNTIME_DIR" ]; then
        XDG_RUNTIME_DIR="/tmp/xdg-runtime-$(id -u)"
        export XDG_RUNTIME_DIR
        mkdir -p "$XDG_RUNTIME_DIR"
        chmod 0700 "$XDG_RUNTIME_DIR"
    fi
    exec weston
fi

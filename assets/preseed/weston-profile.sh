# Added by entangled install (EPIC 13): start Weston on the autologin console.
if [ -z "$WAYLAND_DISPLAY" ] && [ "$(tty)" = "/dev/tty1" ]; then
    exec weston
fi

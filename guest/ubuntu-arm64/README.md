# Ubuntu ARM64 desktop on macOS

The native backend uses an Ubuntu ARM64 direct-Linux profile, virtio-MMIO,
Venus and a host Vulkan driver over Metal. See ADR-0007 for the verified
runtime and remaining product work. This directory contains guest scripts;
do not run them on the host.

The CLI now fetches the authenticated base rootfs:

```sh
entangled fetch ubuntu-cloud --arch arm64
entangled fetch ubuntu-cloud --arch arm64 --offline
```

The current base is the immutable official Noble 20260926 ARM64 cloud
archive. Its SHA256SUMS signature is verified with Canonical's separately
pinned cloud-image key before the archive digest is used. Interrupted
transfers can resume; cache hits recheck the signature and archive digest
without network access. This provides installer media; extraction,
desktop provisioning and connecting the manager's creation flow are still
being integrated.

After installing the Ubuntu GNOME desktop, `mesa-vulkan-drivers`,
`mesa-utils` and `glmark2`, run as root inside the guest:

```sh
sh configure-venus-desktop.sh
```

Use an X11 session (the development image disables GDM Wayland). The script
preserves unrelated `/etc/environment` entries and replaces its own marked
block when repeated. It selects Zink over the virtio Vulkan ICD, allows
Kopper without DRM modifiers, and disables Xorg Glamor. Without the last
setting, Xorg repeatedly fails `AddScreen/ScreenInit` with the current
software-WSI Venus path. GNOME's compositor and GL applications still use
Zink/Venus; disabling Glamor does not disable their 3D renderer.

Verify the actual session after login with `glxinfo -B`, `glmark2` and the
GNOME Shell journal. The macOS hardware run displayed the GNOME desktop;
`gnome-shell` inherited the Zink settings, loaded `libvulkan_virtio.so`, and
its own journal identified the Venus device. `glxinfo` reported Apple M3 Pro
and `Accelerated: yes`.

Current KosmicKrisp capabilities limit Zink to OpenGL 2.1 / GLES 2.0 and
produce warnings for missing `fillModeNonSolid` and
`VK_EXT_custom_border_color`. This is experimental hardware acceleration,
not graphics conformance or a guarantee for every application. The native
presentation currently copies pixels; native image sharing and broader
correctness/performance checks remain work in progress.

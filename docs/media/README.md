# README artwork and screenshots

These assets are committed so the repository's README does not depend on an
external image host. Screenshots are actual application output, not UI mockups.

| Asset | Origin | What it shows |
|---|---|---|
| `banner.svg` | Hand-authored vector artwork using the manager's Deep Field palette and linked-ring motif (`apps/manager/src/theme.rs`, `logo.rs`) | Project branding; not an application screenshot |
| `architecture.svg` | Hand-authored diagram based on the workspace and ADRs | Simplified component relationships, with backend limitations called out |
| `manager.png` | Installed native macOS manager, version 0.2.86, captured 2026-10-21 | Actual All machines UI using `--mock`; states, machines and metrics are fixtures |
| `create-machine.png` | Same binary and capture date | Actual macOS creation wizard using `--mock`; no installation was launched |
| `ubuntu-venus.png` | Unmodified copy of `artifacts/macos/acceptance/smp/ubuntu-glmark2.png`, from the 2026-10-15 SMP acceptance run | Real Ubuntu ARM64 GNOME guest on Apple M3 Pro, Zink/Venus renderer output after a selected glmark2 scene |

The Ubuntu capture is a guest framebuffer, not a photograph of a host window.
It documents one real graphics workload. It is not evidence of general game
compatibility, graphics conformance, sustained performance or a long SMP soak.
See [ADR-0007](../adr/0007-macos-arm64.md) for implementation and acceptance
context. The original local acceptance report identifies a 12-core M3 Pro,
macOS 27, a 12-vCPU GNOME run and a selected ten-second glmark2 build scene.

## Recreate manager screenshots

Use the installed `entangled-manager` or a build of the manager. Run from the
repository root (the output directory must already exist):

```bash
entangled-manager --mock --screenshot "$PWD/docs/media/manager.png" --screenshot-view main
entangled-manager --mock --screenshot "$PWD/docs/media/create-machine.png" --screenshot-view wizard
```

The screenshot mode renders the actual application and exits. `--mock` avoids
loading personal settings, scanning VM directories or launching VM processes.
Keep the visible **MOCK DATA** badge and the README captions. In particular,
the screenshot's simulated pause/suspend states do not imply macOS support for
those lifecycle operations.

The Ubuntu screenshot requires a real guest; it cannot be recreated with the
manager's demo data. Follow the native ARM64 setup and acceptance notes in
[ADR-0007](../adr/0007-macos-arm64.md).

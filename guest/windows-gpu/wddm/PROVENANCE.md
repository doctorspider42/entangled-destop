# entangled-wddm: where it comes from

`entangled-wddm.sys` is the WDDM driver for the display function
([ADR-0010](../../../docs/adr/0010-wddm-desktop-on-the-gpu.md)): the
**viogpu3d** driver proposed to virtio-win as PR 943, with our patches.
Guest-side code: it runs in a Windows guest and is never linked into a host
binary, so `cargo deny` does not apply to it.

| | |
|---|---|
| Upstream | <https://github.com/virtio-win/kvm-guest-drivers-windows/pull/943> (open, not merged) |
| Fetched from | the PR's source branch, `max8rr8/kvm-guest-drivers-windows`, `viogpu3d` |
| Directories used | `viogpu/` (`viogpu3d` + `common` + `shared`) and `VirtIO/` |
| Pinned commit | `9ed3aab11fb46e55dc835ff008008623b290a6cf` (2025-09-05) — in `prepare.ps1` |
| Licence | BSD-3-Clause, Red Hat, Inc. and contributors (`viogpu/LICENSE`); shipped beside the binary as `LICENSE-viogpu3d.txt` |
| Our changes | the `0*.patch` files here, applied in order, under the same licence |

Nothing of upstream is copied into this repository. `prepare.ps1` fetches the
pinned commit (the two directories only) and applies the patches; `build.cmd`
compiles the result with `cl`/`link` against the WDK; `../driver/sign.ps1`
test-signs the output directory.

```
powershell -File prepare.ps1 F:\entangled-targets\wddm\src
build.cmd F:\entangled-targets\wddm\src F:\entangled-targets\wddm\out
powershell -File ..\driver\sign.ps1 -Out F:\entangled-targets\wddm\out
```

## The patches

- `0001-builds-without-wpp.patch` — upstream's forced debug mode
  (`#define DBG 1`) is off; `ENTANGLED_NO_WPP` builds without the WDK's trace
  preprocessor, which needs its MSBuild integration; and a diagnostic build,
  `set ENTANGLED_SERIAL_TRACE=4` before `build.cmd` (2 errors … 5 everything),
  sends the driver's messages to COM1, which the VM's log carries. A package
  anyone installs is built without it.

- `0002-a-primary-without-virgl.patch` — on a host with no virgl capset the
  driver makes no context and no 3D resource: allocations are 2D resources
  over the guest's pages, and a present is copied by the driver and sent with
  `TRANSFER_TO_HOST_2D` and `RESOURCE_FLUSH`. A virgl host is served as before.

## Differences in the INF

`entangled-wddm.inf` is ours, not upstream's `viogpu3d.inx`: MSI is on
(upstream turns it off, and this machine cannot route a line interrupt), and
`UserModeDriverName` names a DLL that does not exist until phase W3 — without
the entry Windows stops the adapter right after it starts (code 43).

## Status

Builds, installs, is accepted by Windows and shows the desktop through 2D
resources — for about twenty seconds, until `dwm.exe` crashes for want of a
Direct3D user-mode driver and Windows removes the adapter (ADR-0010, W0 and
W0′). Not in any package, not in CI.

The branch behind an open pull request can be rewritten or deleted. If the
pinned commit stops being fetchable, the driver has to be vendored from a
checkout that still has it.

- `0004-presents-on-the-overlay-plane.patch` - escape `0x901`: the user-mode
  driver's lease saying the host shows its presents on the overlay plane
  (ADR-0010). Under it a flip refreshes the primary once a second instead of
  every frame.

## The user-mode driver

`entangled_d3d10.dll` is built from Mesa (MIT), not from virtio-win:
`../mesa/prepare-d3d10.ps1` clones Mesa 26.2.6 and applies
`../mesa/0001-venus-builds-with-clang-cl.patch` and
`../mesa/0002-d3d10umd-on-wddm-over-zink.patch` - the WDDM integration of
Mesa MR 24223 (<https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/24223>,
MIT) forward-ported, a cap on Zink's buffer cache, and `zink_entangled_frame` -
then copies in `../mesa/d3d10_wddm.c` and `../mesa/wddm_sw_winsys.*`, which
are ours. `../mesa/build-d3d10.cmd` builds it.

The start-once gate in `0001` (a `StartOnce` registry value read after the
hardware is up) is a development aid the INF no longer sets; the INF deletes
a value an earlier build left.

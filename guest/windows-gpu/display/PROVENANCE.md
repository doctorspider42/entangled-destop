# entangled-display: where it comes from

`entangled-display.sys` is virtio-win's display-only driver **viogpudo** with
one patch of ours. Guest-side code: it runs in a Windows guest and is never
linked into a host binary, so `cargo deny` does not apply to it.

| | |
|---|---|
| Upstream | <https://github.com/virtio-win/kvm-guest-drivers-windows> |
| Directories used | `viogpu/` (the driver, `viogpudo` + `common` + `shared`) and `VirtIO/` (the virtio library it is built with) |
| Pinned commit | `defa22ad4ba72b9a35662da40e29b0df40e9fab8` (master, 2026-11-09) — in `prepare.ps1` |
| Licence | BSD-3-Clause, Red Hat, Inc. and contributors (`viogpu/LICENSE`, `VirtIO/LICENSE`); shipped beside the binary as `LICENSE-viogpudo.txt` |
| Our change | `0001-refresh-rate-and-vsync.patch`, under the same licence |

Nothing of upstream is copied into this repository. `prepare.ps1` fetches the
pinned commit (the two directories only) and applies the patch; `build.cmd`
compiles the result with `cl`/`link` against the WDK, as
`../driver/build.cmd` does for `entangled-gpu.sys`; `../driver/sign.ps1`
test-signs every driver in the output directory under one certificate.

```
powershell -File prepare.ps1 F:\entangled-targets\viogpudo\src
build.cmd F:\entangled-targets\viogpudo\src F:\entangled-targets\viogpudo\out
powershell -File ..\driver\sign.ps1 -Out F:\entangled-targets\viogpudo\out
```

## What the patch changes

- **A refresh rate.** `BuildVideoSignalInfo` reported
  `D3DKMDT_FREQUENCY_NOTSPECIFIED`; it now reports the rate of the EDID's
  preferred timing (the host's `[display] refresh_hz`), in millihertz, for
  every mode.
- **A vsync.** `DxgkDdiControlInterrupt` and `DxgkDdiGetScanLine` are
  supplied, and while Windows has the vsync enabled a high-resolution timer
  (`ExAllocateTimer`, `EX_TIMER_HIGH_RESOLUTION`), re-armed against a grid one
  refresh period apart, raises `DXGK_INTERRUPT_DISPLAYONLY_VSYNC`
  (`DxgkCbNotifyInterrupt` under `DxgkCbSynchronizeExecution`, then the DPC).
- **No WPP.** `ENTANGLED_NO_WPP` builds without the WDK's trace preprocessor,
  which needs its MSBuild integration; the driver's traces compile to nothing.

Why, and what it measured: `docs/adr/0008-windows-guests.md`, the vsync
amendment.

## Moving the pin

Change the commit in `prepare.ps1`, run it on a fresh directory, and if the
patch no longer applies, redo the edits in that tree and regenerate it with
`git diff > 0001-refresh-rate-and-vsync.patch` (the clone has
`core.autocrlf=false`, so the patch stays LF). Then build, install in a guest
and repeat the measurement in the ADR before pinning.

# Native D3D12 for the Windows x64 guest

Initial investigation based on `main` at `0fdc490`. The target is an Entangled
WDDM adapter that applications can pass to the system `D3D12CreateDevice`, with
rendering on the physical GPU through Venus. The first functional milestone is
an offscreen buffer copy followed by a fence and verified readback. Presentation
follows as a separate milestone. The existing graphics architecture is recorded
in [ADR-0009](../../../docs/adr/0009-own-windows-gpu-driver.md),
[ADR-0010](../../../docs/adr/0010-wddm-desktop-on-the-gpu.md) and
[ADR-0011](../../../docs/adr/0011-direct3d-9-driver.md).

## Kernel driver blocks native device creation

The display miniport pins viogpu3d at `9ed3aab11fb46e55dc835ff008008623b290a6cf`.
Its `viogpu/viogpu3d/driver.cpp` sets `InitialData.Version` to
`DXGKDDI_INTERFACE_VERSION_WDDM1_3` and installs the legacy `Render`, `Patch` and
`SubmitCommand` callbacks. `viogpu/viogpu3d/viogpu_adapter.cpp` reports
`DXGKDDI_WDDMv1_3` in `DXGKQAITYPE_DRIVERCAPS`. The default miniport build keeps
these values. Microsoft's hardware D3D12 path requires WDDM 2.0 or newer. Putting an
UMD in the fourth INF slot can change the reported DLL path while this kernel
contract is still missing. See the [pinned miniport](https://github.com/max8rr8/kvm-guest-drivers-windows/tree/9ed3aab11fb46e55dc835ff008008623b290a6cf/viogpu/viogpu3d)
and [Microsoft's D3D12 requirements](https://learn.microsoft.com/en-us/windows/win32/direct3d12/directx-12-getting-started).

The next kernel milestone is a WDDM 2.x adapter that starts, reports its model
through `KMTQAITYPE_DRIVERVERSION`, and completes a monitored fence in a real
context on a disposable guest. A WDDM 2.x version bump requires implementing the
matching kernel contract. Validate the GPUVA and residency model, context and
paging queue creation, virtual submission, and fence ordering before exposing
D3D12 caps. Windows can queue monitored fence signals when the engine cannot
write the fence directly. In this design the signal must follow actual Venus
completion. [GPU virtual memory](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-virtual-memory-in-wddm-2-0),
[WDDM residency](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/driver-residency-in-wddm-2-0),
[monitored fence callback](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dumddi/nc-d3dumddi-pfnd3dddi_signalsynchronizationobjectfromgpucb).

## Adapter ABI and first GPU workload

- `entangled_d3d12.dll` exports `OpenAdapter12` using the WDK structures. That
  export is present on this host's NVIDIA and Microsoft WARP UMDs. Microsoft's
  early DDI spec also describes the `OpenAdapter10_2` entry for D3D12. Verify
  the entry and callback order against the target guest runtime. This probe
  reports zero supported DDI versions and refuses device creation. It emits
  `OutputDebugString` messages at adapter open and version enumeration.
  [Microsoft's DDI overview](https://microsoft.github.io/DirectX-Specs/d3d/CPUEfficiency.html#initialization).
- The first workload requests feature level 11_0 and uses a DIRECT queue,
  allocator, command list, upload and readback buffers, `CopyBufferRegion`,
  and an event backed by a queue fence. `d3d12-smoke.exe` checks the adapter
  LUID, rejects software adapters, and requires the Entangled UMD in guest
  mode. A passing copy validates those operations. Public feature-level support
  also requires the matching caps and function tables.
- `_0040` is a WDK DDI revision worth testing once the KMD is ready. Helios
  measured it with a triangle on its Windows build. Its measurements narrow
  the ABI search, while the target Entangled Windows 10 build still has to
  negotiate the revision. [Helios DDI measurements](https://github.com/winboat-org/helios/blob/master/DX12.md#42--p1-status--contract-capture-complete-2026-08-05).

## Transport and presentation

| Area | What the current stack provides |
| --- | --- |
| Adapter and transport | `entangled-wddm.sys` owns the display PCI function. The Windows Venus ICD opens `\\.\EntangledGpu` on the separate KMDF render node. Reuse that ICD and render node for execution. The host policy filters CPU Vulkan devices and caps the advertised Vulkan API at 1.3. |
| Queues | The host policy forwards physical queue families. The executor records `vkCmdCopyBuffer` and implements `vkQueueSubmit` and `vkQueueSubmit2`. Start with one graphics/transfer queue and validate the advertised queues inside the guest before exposing more. The RTX 2070 host reports graphics/compute/transfer, dedicated transfer and dedicated compute families. |
| Memory | The executor admits a device with importable coherent host memory and keeps guest-visible and host memory type mappings. Use host-visible upload and readback plus a device-local path for render targets. WDDM allocation ownership, GPUVA and reported residency remain separate from Vulkan object allocation. |
| Sync | The renderer supports Vulkan timeline semaphore operations and per-queue virtio fence retirement. The Windows ICD presently waits for retirement on fenced IOCTL submits and has no external sync import/export. Validate a single WDDM monitored fence against a host completion before implementing cross-queue waits. Measure the guest/host round trip. |
| Present | DWM and fullscreen D3D9 already use a GPU copy to host-visible memory followed by the Venus overlay. A windowed frame still needs a handoff to DWM. Validate a real DXGI flip-model swap chain and windowed composition before using the overlay for a fullscreen comparison. |

Sources in the tree include the [Windows Venus winsys](../mesa/vn_renderer_entangled.c),
[Venus adapter policy](../../../crates/virtio-gpu/src/venus/executor/policy.rs),
[queue submits](../../../crates/virtio-gpu/src/venus/executor/submit.rs) and
[DWM integration](../mesa/d3d10_wddm.c).

vkd3d-proton is a useful reference for the API-to-Vulkan translation. Its
LGPL-2.1 distribution terms need review before reusing code. Upstream's Windows
DLLs implement the application API and rely on DXVK's DXGI. Using it as a guest
UMD backend would still require a DDI bridge and WDDM integration. Its published
requirements include Vulkan 1.3, high descriptor indexing limits, robustness2
and `VK_KHR_push_descriptor`. The RTX host advertises push descriptors, while
the Entangled Venus policy excludes that extension. Query the other limits
and features in the guest before considering that backend.
[Upstream requirements and integration](https://github.com/HansKristian-Work/vkd3d-proton/blob/v3.0.1/README.md),
[upstream license](https://github.com/HansKristian-Work/vkd3d-proton/blob/v3.0.1/LICENSE).

## Build and verification

From the repository root on Windows with MSVC Build Tools and SDK 10.0.26100.0:

```powershell
$probeOut = 'F:\entangled-targets\d3d12'
& .\guest\windows-gpu\d3d12\build.cmd $probeOut
& "$probeOut\ddi-test.exe"
& "$probeOut\d3d12-smoke.exe" --hardware
& "$probeOut\d3d12-smoke.exe" --hardware --fence
```

The ABI test loads the built DLL and calls its WDK adapter entry point. The
hardware control uses `System32` D3D12/DXGI. On 2026-11-23 the MSVC build
passed with `/W4 /WX`, the DLL exported `OpenAdapter12`, and the control on the
RTX 2070 (`10DE:1F02`, WDDM `2700`) verified all 65,536 bytes. The final run
reported a 4.466 ms submit-to-fence interval for a single copy on the host.
Guest mode on the host correctly failed with `0x887A0002` because the explicit
Entangled adapter was absent. An invalid argument returned exit code 2.

`--fence` tests the WDDM kernel path through D3DKMT. It creates a virtual
synchronization-only context, queues a wait followed by a completion signal,
then releases the wait from the CPU. The completion event must stay quiet
before that release and arrive afterward. The RTX 2070 at WDDM `2700`
passed. Run `d3d12-smoke.exe --fence` on the Entangled adapter to identify
the first unsupported operation in the guest's miniport.

To prepare a package, pass a base WDDM directory containing the miniport, INF
and four x64/x86 UMDs. Staging changes the fourth x64 slot, adds the file to
the INF and advances the INF revision. Rebuild and test-sign the catalog using
the project's lab signing procedure before installing on a disposable Windows
x64 guest. `Inf2Cat /os:10_X64 /uselocaltime` passed on the staged package, with
zero warnings and errors. The input package was left in place.

```powershell
& .\guest\windows-gpu\d3d12\stage-lab.ps1 -WddmPackage $baseWddmPackage `
    -Umd "$probeOut\entangled_d3d12.dll" -Out "$probeOut\wddm-lab"
```

In the isolated guest, enable Venus and the render node, verify that the
catalog and display driver are selected, then run `d3d12-smoke.exe` with no
arguments. Check the printed WDDM version, the registered UMD and debug output
independently. While the miniport advertises WDDM 1.3 and the UMD offers zero
DDI versions, native D3D12 device creation remains incomplete.

The isolated Windows 10 baseline booted headless with Venus and the render
node enabled on 2026-11-23. `dxgkrnl` reported WDDM `1300` for the Entangled
adapter (`1AF4:1050`) and `entangled_d3d10.dll` in the D3D12 slot. The
virtual context was created, followed by `STATUS_INVALID_PARAMETER`
(`0xC000000D`) at monitored fence creation. `D3D12CreateDevice` for feature
level 11_0 returned `DXGI_ERROR_UNSUPPORTED` (`0x887A0004`). This measured the
installed baseline package. The experimental UMD's runtime callback order
still needs to be captured in the guest.

The first miniport change gives DXGK distinct device and context objects.
Present and render use the context's owning device. Queued legacy commands
carry the virtio context ID by value. After a cold boot the lab guest ran
version `21.20.10.383` from `oem58.inf`. The active DriverStore image matched
the signed build by SHA-256. On that adapter `d3d11-test` presented 300 frames
through the copying swap chain and verified the readback. The fence probe
still reached monitored fence creation and got `0xC000000D` on WDDM `1300`.
Both PCI functions reported no PnP problems. The render node also completed
the Venus ring and reply-blob exchange in `gpu-test.exe venus`.

## Experimental WDDM 2.0 miniport

Set `ENTANGLED_WDDM2_LAB=1` for `guest/windows-gpu/wddm/build.cmd` to build
the experimental miniport for the disposable guest. The GPUVA description
uses four 4 KiB tables per address walk, with 256 entries per table and a
44-bit virtual address. VidMm allocates the tables in system memory. The
miniport stores the root on each DXGK context and applies CPU-virtual page
table updates, including repeated entries and bounds checks. On the single
execution node it uses the existing command queue to retire empty virtual
packets. Nonempty virtual DMA still needs a page-table walk and command
execution path. The lab returns `STATUS_INVALID_PARAMETER` for those packets
and tracks their fence in order without reporting a successful execution
interrupt. [GpuMmu model](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpummu-model),
[virtual DMA callback contract](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_submitcommandvirtual).

The signed lab builds reached `DxgkInitialize` and `DxgkDdiStartDevice` with
`STATUS_SUCCESS` on Windows 10. The first diagnostic build identified two
queries to add for display capabilities and history buffer precision. After
adding those answers, every observed miniport capability query succeeded.
The DxgKrnl trace shows VidMm creating its system GPUVA allocator and an
internal fence, then destroying both immediately before releasing the
miniport. Device Manager reported Code 43. Registering the process and
root-size callbacks still produced the same post-start removal and the
trace did not reach the process callback. The user-mode fence probe could
not find the failed adapter. The lab reverted to the verified `oem58.inf`
WDDM 1.3 package for further tests.

For GPU proof, correlate the guest workload with a new Venus context and a
host GPU submission and retirement, then verify the bytes after the guest
fence. For presentation, separately measure GPU work, `Present` and the time
until the host displays the image. Wintanyl F241 measured median scroll
latency of 5.16 ms on the RTX and 312.73 ms on WARP. Those are visible-response
measurements, and the headless buffer-copy interval covers a different path.

# Native D3D12 for the Windows x64 guest

Initial investigation based on `main` at `0fdc490`. The target is an Entangled
WDDM adapter that applications can pass to the system `D3D12CreateDevice`, with
rendering on the physical GPU through Venus. The first functional milestone is
an offscreen buffer copy followed by a fence and verified readback. Presentation
follows as a separate milestone. The existing graphics architecture is recorded
in [ADR-0009](../../../docs/adr/0009-own-windows-gpu-driver.md),
[ADR-0010](../../../docs/adr/0010-wddm-desktop-on-the-gpu.md) and
[ADR-0011](../../../docs/adr/0011-direct3d-9-driver.md).

## Kernel contract for native device creation

The display miniport pins viogpu3d at `9ed3aab11fb46e55dc835ff008008623b290a6cf`.
Its `viogpu/viogpu3d/driver.cpp` sets `InitialData.Version` to
`DXGKDDI_INTERFACE_VERSION_WDDM1_3` and installs the legacy `Render`, `Patch` and
`SubmitCommand` callbacks. `viogpu/viogpu3d/viogpu_adapter.cpp` reports
`DXGKDDI_WDDMv1_3` in `DXGKQAITYPE_DRIVERCAPS`. The default miniport build keeps
these values. Microsoft's hardware D3D12 path requires WDDM 2.0 or newer. Putting an
UMD in the fourth INF slot can change the reported DLL path while this kernel
contract is still missing. See the [pinned miniport](https://github.com/max8rr8/kvm-guest-drivers-windows/tree/9ed3aab11fb46e55dc835ff008008623b290a6cf/viogpu/viogpu3d)
and [Microsoft's D3D12 requirements](https://learn.microsoft.com/en-us/windows/win32/direct3d12/directx-12-getting-started).

The lab miniport starts as WDDM 2.0. A virtual render context on node 1
passed the monitored-fence ordering test after the CPU released the gate.
Windows can queue monitored fence signals when the engine cannot write the
fence directly. For submitted GPU work the signal must follow actual Venus
completion. Residency and execution of nonempty WDDM DMA packets remain open.
[GPU virtual memory](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-virtual-memory-in-wddm-2-0),
[WDDM residency](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/driver-residency-in-wddm-2-0),
[monitored fence callback](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dumddi/nc-d3dumddi-pfnd3dddi_signalsynchronizationobjectfromgpucb).

## Adapter ABI and first GPU workload

- `entangled_d3d12.dll` exports `OpenAdapter12` using the WDK structures. The
  Windows 10 system runtime loaded it from the DriverStore and called that
  entry, then queried `GetSupportedVersions` twice. Each open owns a Vulkan
  instance. It selects a non-CPU Mesa Venus physical device with a graphics
  queue through the system Vulkan loader and releases the instance on close.
  The runtime trace selected the host RTX 2070 through Venus, queue family `0`.
  The default build reports zero supported DDI versions. The disposable
  runtime probe negotiates `_0040` and creates a logical Vulkan device and
  graphics queue in the D3D12 driver's `CreateDevice` callback.
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

The ABI test loads the built DLL and calls its WDK adapter entry point. Pass
`--venus` in the guest to require success and check two independent adapter
lifetimes and logical device creation and destruction. On the host without
Venus it verifies that a failed open leaves the
driver handle and callback table empty. The hardware control uses `System32`
D3D12/DXGI. On 2026-11-23 the MSVC build passed with `/W4 /WX`, the DLL exported
`OpenAdapter12`, and the control on the
RTX 2070 (`10DE:1F02`, WDDM `2700`) verified all 65,536 bytes. The final run
reported a 4.466 ms submit-to-fence interval for a single copy on the host.
Guest mode on the host correctly failed with `0x887A0002` because the explicit
Entangled adapter was absent. An invalid argument returned exit code 2.

`--fence` tests the WDDM kernel path through D3DKMT. It creates a virtual
synchronization-only context, queues a wait followed by a completion signal,
then releases the wait from the CPU. The completion event must stay quiet
before that release and arrive afterward. The RTX 2070 at WDDM `2700`
passed with both CPU-visible fence values at `1` after release. The probe
reports the two values even if the event times out. That distinguishes a
stalled signal from a signal that advanced the fence without waking its
waiter. Run `d3d12-smoke.exe --fence` on the Entangled adapter.
Use `--fence-render-context` in the lab to exercise a GPUVA rendering context
with the same fence sequence and Entangled's empty context-private data.

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
installed baseline package.

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
table updates, including repeated entries and bounds checks. The lab exposes a
physical-address node for paging and GDI and a GPUVA node for application
rendering. The nodes share the backend command queue and track completion
fences separately. Nonempty virtual DMA still needs a page-table walk and command
execution path. The lab returns `STATUS_INVALID_PARAMETER` for those packets
and tracks their fence in order without reporting a successful execution
interrupt. [GpuMmu model](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpummu-model),
[virtual DMA callback contract](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_submitcommandvirtual).

The first lab builds reached `DxgkInitialize` and `DxgkDdiStartDevice` with
`STATUS_SUCCESS` on Windows 10. The diagnostic trace identified two missing
answers for display capabilities and history buffer precision. With both in
place, every observed capability query succeeded. DxgKrnl then created its
system GPUVA allocator and an internal fence, destroyed both and removed the
adapter with Code 43. Setting `PageTableAlignmentInBytes` to 4096 and then
moving the paging buffer from the aperture to system memory produced the same
result. No process, device or context creation was seen in those trials.

On build 19041, the Watchdog error log recorded diagnostic `0x1DD6` from
`DXGADAPTER::Initialize`, followed by `STATUS_INVALID_PARAMETER` in the start
path. The failing check requires `DxgkDdiCalibrateGpuClock` and
`DxgkDdiSetStablePowerState` for this adapter. The lab registers both. Clock
calibration returns `STATUS_NOT_SUPPORTED` because the miniport has no host GPU
timestamp source. The stable-power callback leaves physical clocks under host
control. [WDDM debugging tips](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/debugging-tips-for-wddm-drivers),
[clock calibration contract](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_calibrategpuclock).

The signed build `22.19.8.198` started the adapter as `oem11.inf`. DxgKrnl
called `CreateProcess` for its system process, and the D3DKMT test reported
WDDM `2000` on the Entangled DXGI adapter. Monitored fence creation and the
CPU signal passed. The queued completion stayed quiet before gate release and
still had no event after 10 seconds (`WAIT_TIMEOUT`, `0x102`). The stock UMD
was registered in the fourth INF slot for this test. `D3D12CreateDevice`
returned `0x887A0004` at feature level 11_0. The default WDDM 1.3 and the
WDDM 2.0 lab miniports both built with WDK 10.0.26100.0.

On reboot, the WDDM 2.0 miniport again reached system process creation. The
desktop scanout remained black while the serial log showed DWM failing to
resolve allocation handles for its escapes. A framebuffer-only boot let us
remove the experimental packages and the lab kernel debug flag.
After restoring `oem58.inf` and booting with Venus again, the Entangled
adapter reported WDDM `1300`. The D3D11 regression presented 300 frames and
verified the readback via the copying swap chain. The diagnostic fence probe
reproduced `0xC000000D` at monitored fence creation on that restored baseline.

With the experimental UMD installed, the synchronization-only probe read
`gate=1 completion=0`. DxgKrnl ETW recorded that it unblocked the wait packet
on the CPU signal. There was no corresponding completion of the queued signal.
The probe did not finish tearing down its DXGK objects, and a D3D11 regression
launched in that session also timed out. The separate rendering-context probe
initially returned `0xC000000D` before reaching the KMD. The kernel checks
GPUVA support for the requested node, in addition to the adapter-wide caps.

Advertising GPUVA on the only node caused Windows to create virtual system
and GDI contexts. During the install, the guest bugchecked with `0x3B` and
`0xC0000005`. The minidump matches a null dereference in
`VIDMM_DMA_POOL::AddDmaBufferToPool` on its way to committing GPU VA. The lab
now keeps the system paging and GDI node on the legacy path and reserves a
separate node for GPUVA rendering.

The two-node build started after it initialized the full per-node history
precision array. Signed build `22.57.31.732` created a virtual render context
on node `1`, then completed the wait and signal in order. The probe read
`gate=1 completion=1` and exited. DxgKrnl ETW recorded the wait and signal
packets completing on that context. A follow-up `D3DKMTSubmitCommand` with no
command bytes succeeded at the thunk and generated no DMA packet in ETW for
that context. The D3D11 regression on the same build presented 300 frames
through the copying swap chain and verified the readback. The lab restored
`oem58.inf` afterward.

## Runtime device initialization

Build with `ENTANGLED_D3D12_PROBE_DDI=1` for the disposable guest. This
enables the provisional `_0040` DDI handshake and reports feature level
11_0 and tier 1 heap and resource binding values to advance the runtime's
initialization. The probe rejects function table requests while the command
and resource DDIs are incomplete. Keep this build out of the regular driver
package.

On Windows 10 build 19041, `D3D12Core.dll` called `CalcPrivateDeviceSize`
and `CreateDevice` with interface `0x000C0028`. `CreateDevice` opened a
logical device and graphics queue through Venus on the host RTX 2070 and
returned `S_OK`. The runtime queried the 52-byte `_0033` options structure,
then requested the 768-byte `_0040` device-core table. That request returned
`DXGI_ERROR_UNSUPPORTED`, after which the runtime called `DestroyDevice`.
The public `D3D12CreateDevice` also returned `0x887A0004`. The log establishes
the point at which the resource, command queue and command list DDIs are
needed for the offscreen copy to become reachable.

Add `ENTANGLED_D3D12_PROBE_COPY=1` to the lab build to exercise the Venus
transfer inside `CreateDevice`. The probe uses independent coherent upload
and readback allocations, records one buffer copy and a host-read barrier,
submits to the graphics queue and waits on a Vulkan fence. It verifies all
64 KiB and both guard regions. Keep this diagnostic off during normal device
creation and latency measurements.

Signed build `0.25.50.851` passed that probe on Windows 10 build 19041.
The host log connects two 69,632-byte mapped allocations and a ring fence to
Venus context `1240065` on the physical RTX 2070. The readback in the guest
passed after the Vulkan wait. The guest then reached the same core-table
refusal and restored `oem58.inf`. The guest debugger and host logs are in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-venus-copy/`. The default build
passed the adapter-decline test on the Windows host.

Build `0.31.11.202` repeated the copy using the WDK `_0030` heap and
resource callbacks. The probe exercised separate runtime-private heap and
resource objects, mapped both heaps, submitted the transfer and destroyed
the objects after verification. The callbacks currently accept unprotected
committed system-memory buffers with write-combine or write-back page
properties. The system runtime still receives the core-table refusal while
the rest of the device DDI is under construction. The guest and host logs
are in `F:/VMs/Entangled/d3d12-lab/share/ddi40-ddi-buffer/`.

Two debugger-only builds used typed stop callbacks for the unfinished
table slots and advertised the revision only in the smoke test process.
Windows 10 requested the 768-byte core, two 464-byte command-list tables
at indices 0 and 1, the 56-byte queue table and the 32-byte extended-feature
table. Build `0.35.9.973` refused the last table and device creation returned
`0x887A0004`. Build `0.36.49.221` provided `_0021` extended-feature
enumeration with zero features. The runtime accepted that response and
requested the 40-byte `D3D12DDICAPS_TYPE_SHADER` payload. The current
driver source returns the extended-feature table. The traces are in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-tables/` and
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-extended/`. The stop
callbacks live in the separate scratch builds.

Windows 10 rejects a zero `_0015` shader payload even when wave operations
are disabled. The Direct3D 12 ETW event in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-shader-diag/d3d12.xml`
names the wave and total lane counts. The lab uses the Vulkan subgroup
size as a conservative floor. The RTX 2070 reports 32. This allows the
runtime to query architecture information and then the format callback
for `R8G8_UINT` and `R8_UINT`. Reporting no optional format operations
in the debugger-only build allowed the two-phase shader model query.
The lab advertises shader model 5.1 for the initialization test. Shader
translation remains to be implemented.

Build `0.46.16.21` reached the UMD queue priority query. After the test
script restored `oem58.inf`, the installed host engine ended with
`0xC0000005`. The preceding several driver swaps left the host log
counting 51 Venus contexts. The faulting module and cause are unverified.
The probe trace, rollback log and host exit log are in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-model/`.

On a fresh boot, build `0.47.24.485` passed the queue priority query with
no global real-time queues and reached `pfnCheckMultisampleQualityLevels`.
The next scratch build answered the format and sample-count queries with
single-sample diagnostic values and reached `pfnQueryNodeMap`. Build
`0.52.20.501` returned one physical adapter at index zero. That index
is separate from WDDM engine ordinal one used for the render context.
The runtime then requested the 20-byte `_0026` texture-layout payload.
ETW recorded this request as the next initialization failure. The source
now provides the provisional layout payload without optional swizzles or
row-major textures. The trace and the subsequent rollback stall are in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-node-map/`.

The recovery boot used the framebuffer profile of the same isolated lab
disk, removed diagnostic package `oem12.inf`, and verified that
`oem58.inf` was available. It also removed the twelve temporary lab
certificates from the guest trust stores. The recovery guest shut down
cleanly.

Build `0.58.49.775` used the typed command-list
`pfnCopyBufferRegion` and `pfnCloseCommandList` callbacks in the
`CreateDevice` diagnostic. They recorded the Vulkan copy and its
host-read dependency. The queue fence and byte verification passed on the
RTX 2070. The scratch runtime initialization accepted the texture-layout
payload and next validated `D3D12DDI_MEMORY_ARCHITECTURE_CAPS`. ETW reports
that the lab's discrete adapter requires `IOCoherent`. The trace and
clean rollback are in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-layout/`.

Build `1.6.46.113` answered the coherent discrete memory query and all three
row-major layout queries using Direct3D's 512-byte placement and 256-byte
row-pitch alignments. Build `1.8.8.973` also answered the CPU page-table
false-positive query with no affected queues. Windows then created its own
root signature during `D3D12CreateDevice`, before the application's copy.
Build `1.11.32.396` identified version 1.1, node mask 1, and zero root
parameters and static samplers. Logs are in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-root-sig/`.

Build `1.17.4.597` created the runtime's empty root signature as a Vulkan
pipeline layout. The UMD owns copies of nonempty root-signature descriptions
and their nested tables for later pipeline construction. This build also
used the allocation-size DDI to choose a 64 KiB alignment and a 128 KiB
heap for the guarded transfer test. The 64 KiB GPU copy and byte check
passed. Windows next passed a vertex shader with a raw Shader Model 4.0
token stream of 15 DWORDs. The scratch build stopped in the vertex shader
creation callback. The trace is in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-root-create/`.

## Shader translation

The stock vkd3d-proton Windows DLL exports the Direct3D runtime entry point.
A private engine integration would need its own device factory and a Vulkan
feature profile the Venus renderer can serve. Upstream requires Vulkan 1.3,
descriptor indexing, robustness2 and push descriptors. Entangled currently
withholds `VK_KHR_push_descriptor`. vkd3d-proton uses LGPL-2.1-or-later.
[Driver requirements](https://github.com/HansKristian-Work/vkd3d-proton#drivers),
[Windows exports](https://github.com/HansKristian-Work/vkd3d-proton/blob/master/libs/d3d12core/d3d12core.def),
[license](https://github.com/HansKristian-Work/vkd3d-proton/blob/master/COPYING).

The smaller shader-only candidate is `dxil-spirv` with its
`dxbc-spirv` submodule, both MIT-licensed. The pinned scratch build
(`53aac418` and `d82d9eaa`) compiled as a Windows x64 DLL with MSVC.
A local probe translated a vertex shader compiled with FXC `vs_5_1`
and with DXC `vs_6_0` through the public C API. It verified the vertex
stage and SPIR-V magic for both outputs. The converter and evidence are
in `F:/entangled-targets/d3d12/upstream/dxil-spirv-msvc/`. This compiler
test ran as a Windows host process.
[dxil-spirv license](https://github.com/HansKristian-Work/dxil-spirv/blob/53aac41815f8203dff4e081969438ed5063dbff3/LICENSE.MIT),
[dxbc-spirv license](https://github.com/doitsujin/dxbc-spirv/blob/d82d9eaa6cab5c7277a6839f328e1b4d2f146894/LICENSE).

The runtime supplies raw tokens and separate input/output signature entries.
The first vertex shader has no inputs and one `SV_Position` output. The
diagnostic reconstructs the three DXBC chunks from these inputs. In the
local FXC fixture, retaining both signatures produced byte-identical SPIR-V
to the original container. The runtime shader also translated locally.
See `F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-shader-io/` and
`F:/entangled-targets/d3d12/upstream/dxil-spirv-msvc/ddi-vertex.log`.

For the shader probe, point `ENTANGLED_D3D12_SHADER_INCLUDE` at the pinned
`dxil-spirv` include directory and enable both existing DDI and copy flags.
Pass the Windows x64 `dxil-spirv-c-shared.dll` to `stage-lab.ps1` with
`-ShaderCompiler`. Use the converter built with a static CRT
(`CMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded`). The UMD loads it from its own
Driver Store package. Build `1.26.39.682` with the dynamic CRT reached the
loader and reported `0x887A0004`. Build `1.32.22.507` with the static CRT
created a Venus `VkShaderModule` for the runtime vertex shader, then
initialized the blend, depth-stencil and rasterizer descriptions.

Build `1.38.16.411` negotiated `VK_KHR_maintenance5` and created the
runtime's vertex-only point pipeline without attachments. Maintenance5
provides a Vulkan point size of 1 when the shader omits it. Windows then
called `pfnMakeResident` for its pipeline initialization. The GPU copy,
shader-module and graphics-pipeline creation results are recorded in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-vk-pso/`.
[Vulkan maintenance5](https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_maintenance5.html).

Windows passed an empty make-resident batch for the graphics pipeline and
asked for debug allocation records. The experimental UMD has no KMT
allocations or WDDM GPU virtual addresses to report for these host-backed
objects. With both callbacks in place, the runtime requested its second
shader, an eight-DWORD Shader Model 5.0 compute program. The trace is in
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-debug-alloc/`.

Build `1.47.32.724` translated the compute program and created its Vulkan
shader module and compute pipeline with the empty root layout. The
runtime finished both pipeline initialization paths and reached
`pfnGetDescriptorSizeInBytes`. The guest still observed the scratch
driver's stop callback at that slot. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-compute-pso/`.

Build `1.53.20.398` returned a 64-byte RTV descriptor stride from the
driver's private descriptor storage. The runtime then requested its first
command pool through `pfnCalcPrivateCommandPoolSize`. Descriptor heap storage
and handles are defined for all four heap types. Descriptor writes and shader
binding are still open. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-descriptors/`.

Build `1.57.41.86` created the runtime's command pool as a Vulkan object and
returned an `ID3D12Device` to the application through `D3D12CreateDevice` at
feature level 11.0. The Direct3D12 trace records the device start event for
the Entangled KMT adapter. The first public `CreateCommittedResource`
returned `DXGI_ERROR_DEVICE_REMOVED` before calling the allocation DDI.
The same trace records a rejected GPUVA capability query (`0x887A0004`)
and device removal for a bad UMD error. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-pool/`.

Build `1.59.51.843` confirmed that Windows rejects a GPUVA capability of
zero during device initialization. Build `2.5.50.688` reported the lab
miniport's 44-bit address width and reached `CreateHeapAndResource` for the
first application buffer. The runtime requested shader-resource and
unordered-access capabilities even for this upload heap. The address-width
query does not provide address translation or reserved resource support.
See `F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-gpuva-caps/` and
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-gpuva44/`.

Build `2.8.38.235` allocated the application's first 64 KiB upload buffer
through Venus on the host RTX 2070. Build `2.10.50.158` also returned its
64 KiB allocation layout through `pfnCheckExistingResourceAllocationInfo`.
Windows then called `pfnCheckResourceVirtualAddress`. These are public API
allocations after `D3D12CreateDevice` returned. The internal copy diagnostic
still supplies the executed workload in these builds. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-commands/` and
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-existing-buffer/`.

Build `2.13.18.604` enabled Vulkan buffer device addresses for the transfer
buffers. The upload buffer returned a nonzero Venus address, and ETW recorded
its resource creation. That address identifies the Vulkan buffer for this
device lifetime. It has no WDDM page-table mapping or KMT allocation handle.
The runtime removed the device before creating the second committed buffer.
An output-event breakpoint and matching Microsoft public symbols identified
`CHeap::LUCCompleteLayerConstruction` calling `CDevice::MakeResident_Worker`.
The prototype accepted pipelines and declined heap objects, which triggered
the removal. The lab now accepts a heap if its Vulkan backing remains
allocated and CPU-mapped. See `F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-bda/`
and `F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-out-stack/`.

Build `2.25.9.237` completed creation and residency of both application
buffers with backing on the physical GPU. The upload used write-combine
properties and the readback used write-back properties. The runtime then
queried the buffer's subresource layout before mapping it. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-resident-buffers/`.

Build `2.27.22.273` mapped the upload buffer, created a virtual WDDM render
context for the application's DIRECT queue and allocated a Vulkan command
buffer from its allocator. The runtime selected list table index 0 and
requested its initial pipeline state after reset. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-subresource/`.

Build `2.30.51.496` bound the runtime's initial graphics pipeline to the
Vulkan command buffer and advanced to input-assembler state initialization.
See `F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-bind-pso/`.

Build `2.34.59.392` completed the first public API copy with scratch callbacks
logging the runtime's empty graphics bindings. Build `2.37.26.896` repeated
it with all exercised command-state callbacks implemented in this directory.
The system D3D12 runtime recorded and closed the application list, the UMD
submitted its Vulkan copy and waited for GPU retirement, and the application
verified 65,536 bytes after its WDDM fence reached 1. In the second run, host
Venus context `1992705` exposed the RTX 2070 and allocated both 65,536-byte
buffers. The individual submit-to-fence times were 2.710 ms and 45.126 ms.
They include guest, Venus, and WDDM synchronization. The smoke test has no
rendering or presentation timing. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-empty-bindings/` and
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-public-copy/`.

Build `2.44.53.466` repeated the public copy with unmodified source and
tables built by `build.cmd`. It verified all bytes at 1.781 ms submit-to-fence.
Build `2.41.17.228` had already confirmed that the same process is declined
when the run flag is absent. The lab uses `ENTANGLED_D3D12_PROBE_DDI=1`,
`ENTANGLED_D3D12_PROBE_COPY=1`, `ENTANGLED_D3D12_PROBE_RUNTIME=1` and the pinned
`ENTANGLED_D3D12_SHADER_INCLUDE` described above. Stage the signed UMD and
converter with the lab WDDM 2.0 miniport, and set `ENTANGLED_RUNTIME_TABLES=1`
in the test process's environment. The only admitted executable name is
`d3d12-smoke.exe`. This build exposes the tested transfer contract, with shared
heap opens and tiled copies explicitly declined. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-in-tree-tables/`.

Build `2.54.20.259` passed `d3d12-smoke.exe --clear` through the system
runtime. The application cleared an RGBA8 render target, transitioned it
to copy source, copied it into a readback buffer with a 512-byte row pitch,
and verified all 73 × 41 pixels after its WDDM fence. Vulkan used a
single-sample linear image in device-local memory and executed the clear
and copy on the RTX 2070. Host Venus context `1082369` allocated the
page-rounded 24,576-byte readback. The debugger run reported 102.057 ms
from submit to fence for clear and readback, and 2.243 ms for the separate
65,536-byte copy regression. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-color-clear/`.

Build `3.6.27.946` passed `d3d12-smoke.exe --draw`. The application compiled
Shader Model 5.1 vertex and pixel programs with FXC and passed them through
the system D3D12 runtime. The UMD converted the program and signature tokens
to SPIR-V, built a dynamic Vulkan graphics pipeline and executed a fullscreen
triangle on the RTX 2070. The application verified 73 × 41 green pixels after
its WDDM fence. The draw uses a lazily allocated optimal Vulkan color
attachment, and readback copies directly from that image. The same driver
passed the clear and buffer-copy regressions. Debugger submit-to-fence
times were 1.273 ms for draw and readback, 148.274 ms for clear and readback,
and 1.329 ms for buffer copy. These are individual trace runs. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-color-draw/`.

The live driver rollback stalled after the three draw-suite processes
exited. The lab stopped accepting ACPI shutdown. After pausing only that VM
and confirming the disk had stopped writing, its framebuffer recovery boot
removed the exact `3.6.27.946` package and test certificate and verified the
baseline `oem58.inf` in the guest store. The recovery log is alongside the
draw results. Subsequent installs run one probe process per boot.

The experimental `_0040` queue and list callbacks now hold a WDDM context
and Vulkan command pool, serialize recording on the shared pool and select
the matching runtime table. The transfer submission callback waits for
Vulkan completion before returning. The release build continues to decline
the provisional DDI.

## Windowed DXGI presentation

For the presentation probe, also set `ENTANGLED_D3D12_WDDM_INCLUDE` to the
pinned miniport's `viogpu/shared` directory. The UMD uses that header to
allocate a WDDM surface for each BGRA8 backbuffer. The `--present` probe
creates a 320 × 240 flip-discard HWND swap chain. The `--composition` mode
uses a flip-sequential, premultiplied swap chain attached to a DirectComposition
visual. Rendering and the copy into a coherent staging buffer run through
Venus. After GPU retirement, the UMD passes the packed pixels to the existing
WDDM shared-surface handoff and returns the allocation and queue context to
DXGI. This lab path has a synchronous staging copy.

Build `3.21.55.661` created the swap chain and returned two KMT allocation
handles. The draw initially stopped at the PRESENT-to-RENDER_TARGET barrier.
Build `3.25.26.722` executed the draw and copy on the RTX 2070 and reported
successful WDDM Lock2 and DXGI Present. This trace measured 2.020 ms from
submit to fence, 10.806 ms in Present, and 516.285 ms in DwmFlush, which
returned `DWM_E_COMPOSITIONDISABLED` (`0x80263001`). All ten host scanout
captures taken while the probe ran were black. The lab restored
`oem58.inf` and shut down cleanly. See
`F:/VMs/Entangled/d3d12-lab/share/ddi40-runtime-present-copy/`.

The serial-trace miniport reproduced unresolved Windows allocation handles
when the Mesa UMD wrote and read shared surfaces during the desktop
composition path. The WDDM 2.0 lab now uses Dxgk's reference-counted
allocation lookup around these escape operations. The default WDDM 1.3
miniport retains its previous lookup path. In the acquire/release run the
invalid-handle reports disappeared, and the guest's Application Error log
identified a crash in the older `entangled_d3d10.dll` in the lab package.
The D3D12 draw and WDDM Present still succeeded, and DWM and scanout failed.
See `ddi40-runtime-present-dwm/` and `ddi40-runtime-present-dwm-acquire/`
under the lab share.

DWM later composed successfully and reached the 320 × 240 app surface.
Its read returned `STATUS_DEVICE_NOT_READY` because the first probe copied
only into the Lock2 mapping. Using the existing present-source write
escape let DWM read the staged pixels from its opened allocation. The
HWND run on `3.40.57.447` passed the DXGI and DWM calls with 2.102 ms
submit-to-fence, 8.998 ms in Present and 24.372 ms in DwmFlush. The
installed engine did not recognize the full-size capture command used
in that run, so it did not produce a host image for visual verification.
See `ddi40-runtime-present-current-mesa/` and
`ddi40-runtime-present-dwm-pixels/` under the lab share.

Build `3.43.42.592` also passed the flip-sequential DirectComposition
probe. It took 2.459 ms submit-to-fence, 7.504 ms in Present and 10.694 ms
in DwmFlush. The host logged Venus scanout flips, but its headless captures
showed the firmware framebuffer. A package audit also found that the local
signing helper had restored the old Mesa UMD from cache. The traces above
therefore do not qualify the newer companion UMD. See
`ddi40-runtime-present-composition/`.

Build `3.47.25.747` staged all driver DLLs and verified the newer Mesa
companion's SHA-256 in the guest. The DirectComposition probe passed in
1.242 ms submit-to-fence, 5.120 ms in Present and 21.092 ms in DwmFlush.
A guest desktop capture verified all 76,800 pixels in the app's frame.
Host captures with `ENTANGLED_SCANOUT_PATH=copy` stayed black. The
installed engine tag `v0.2.152` predates `6db240a`, which added the
headless GPU presenter for the DWM overlay. See
`ddi40-runtime-present-copy-scanout/guest-desktop.png` and its
`guest-pixel-verification.log` under the lab share.

The same signed driver and DirectComposition probe ran with a fresh
headless engine built from this branch, including `6db240a`. Two
1280 × 720 host scanout captures matched all 76,800 expected pixels
inside the window. The trace took 2.292 ms submit-to-fence,
6.915 ms in Present and 57.675 ms in DwmFlush. Host captures verify the
displayed image. Their sampling interval does not measure time to
scanout. See `ddi40-runtime-present-headless-current/scanout-2.png`
and its `host-pixel-verification.log` under the lab share. The installed
engine was preserved.

## End-to-end test criteria

For GPU proof, correlate the guest workload with a new Venus context and a
host GPU submission and retirement, then verify the bytes after the guest
fence. For presentation, separately measure GPU work, `Present` and the time
until the host displays the image. Wintanyl F241 measured median scroll
latency of 5.16 ms on the RTX and 312.73 ms on WARP. Those are visible-response
measurements, and the headless buffer-copy interval covers a different path.

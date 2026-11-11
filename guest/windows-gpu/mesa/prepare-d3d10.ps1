# Makes the Mesa tree build-d3d10.cmd expects (ADR-0010): Mesa 26.2.6 from
# freedesktop.org with both our patches applied - 0001 (Venus builds with
# clang-cl; the tree is the one prepare.ps1 makes) and 0002 (the Direct3D 10
# user-mode driver on WDDM allocations, over Zink) - and our own sources
# copied in beside the rest: the WDDM software winsys, the screen factory, the
# Venus renderer backend and the driver's IOCTL header.
# Usage: powershell -File prepare-d3d10.ps1 <directory for the clone>
param([Parameter(Mandatory)][string]$Dir)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$tag = 'mesa-26.2.6'
if (-not (Test-Path (Join-Path $Dir '.git'))) {
    git clone --depth 1 --branch $tag https://gitlab.freedesktop.org/mesa/mesa.git $Dir
    if ($LASTEXITCODE -ne 0) { throw "cannot clone $tag" }
}
foreach ($patch in '0001-venus-builds-with-clang-cl.patch', '0002-d3d10umd-on-wddm-over-zink.patch') {
    git -C $Dir apply --whitespace=nowarn (Join-Path $here $patch)
    if ($LASTEXITCODE -ne 0) { throw "$patch does not apply" }
}
$venus = Join-Path $Dir 'src\virtio\vulkan'
Copy-Item (Join-Path $here 'vn_renderer_entangled.c') $venus
Copy-Item (Join-Path $here '..\driver\public.h') (Join-Path $venus 'entangled_gpu_public.h')
$winsys = Join-Path $Dir 'src\gallium\winsys\sw\wddm'
Copy-Item (Join-Path $here 'wddm_sw_winsys.c'), (Join-Path $here 'wddm_sw_winsys.h') $winsys
Copy-Item (Join-Path $here 'd3d10_wddm.c') (Join-Path $Dir 'src\gallium\targets\d3d10umd')
Write-Output "prepared $Dir"

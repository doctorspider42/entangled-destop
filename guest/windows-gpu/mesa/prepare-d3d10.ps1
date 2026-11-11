# Makes the Mesa tree build-d3d10.cmd and build-d3d9.cmd expect (ADR-0010,
# ADR-0011): Mesa 26.2.6 from freedesktop.org with our patches applied - 0001
# (Venus builds with clang-cl; the tree is the one prepare.ps1 makes), 0002
# (the Direct3D 10 user-mode driver on WDDM allocations, over Zink) and 0003
# (Nine carried forward, below) - and our own sources copied in beside the
# rest: the WDDM software winsys, the screen factory, the Venus renderer
# backend, the driver's IOCTL header and the Direct3D 9 driver's target.
#
# Nine, Mesa's Direct3D 9 state tracker, left the tree in Mesa 25.2. Its last
# sources (the tag below, MIT like the rest) are checked out into this tree,
# and 0003 is what it takes to build them against the Gallium of 26.2.6 and
# on Windows.
# Usage: powershell -File prepare-d3d10.ps1 <directory for the clone>
param([Parameter(Mandatory)][string]$Dir)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$tag = 'mesa-26.2.6'
$nineTag = 'mesa-25.1.9'
if (-not (Test-Path (Join-Path $Dir '.git'))) {
    git clone --depth 1 --branch $tag https://gitlab.freedesktop.org/mesa/mesa.git $Dir
    if ($LASTEXITCODE -ne 0) { throw "cannot clone $tag" }
}
foreach ($patch in '0001-venus-builds-with-clang-cl.patch', '0002-d3d10umd-on-wddm-over-zink.patch') {
    git -C $Dir apply --whitespace=nowarn (Join-Path $here $patch)
    if ($LASTEXITCODE -ne 0) { throw "$patch does not apply" }
}
git -C $Dir fetch --depth 1 origin tag $nineTag
if ($LASTEXITCODE -ne 0) { throw "cannot fetch $nineTag" }
git -C $Dir checkout $nineTag -- src/gallium/frontends/nine include/D3D9 include/d3dadapter
if ($LASTEXITCODE -ne 0) { throw "cannot take Nine from $nineTag" }
git -C $Dir apply --whitespace=nowarn (Join-Path $here '0003-nine-on-mesa-26.2.patch')
if ($LASTEXITCODE -ne 0) { throw '0003-nine-on-mesa-26.2.patch does not apply' }
$venus = Join-Path $Dir 'src\virtio\vulkan'
Copy-Item (Join-Path $here 'vn_renderer_entangled.c') $venus
Copy-Item (Join-Path $here '..\driver\public.h') (Join-Path $venus 'entangled_gpu_public.h')
$winsys = Join-Path $Dir 'src\gallium\winsys\sw\wddm'
Copy-Item (Join-Path $here 'wddm_sw_winsys.c'), (Join-Path $here 'wddm_sw_winsys.h') $winsys
Copy-Item (Join-Path $here 'd3d10_wddm.c') (Join-Path $Dir 'src\gallium\targets\d3d10umd')
# The Direct3D 9 driver's target: the DDI front, the Nine side and their build.
$d3d9 = Join-Path $Dir 'src\gallium\targets\d3d9umd'
New-Item -ItemType Directory -Force $d3d9 | Out-Null
foreach ($file in 'entangled_d3d9.c', 'entangled_d3d9.def', 'ddi.h', 'ddi_pp.c', 'd3d9_nine.c',
                  'd3d9_nine.h', 'gen-stubs.py', 'meson.build') {
    Copy-Item (Join-Path $here "..\d3d9\$file") $d3d9
}
Write-Output "prepared $Dir"

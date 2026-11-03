# Makes the Mesa tree build.cmd and build-zink.cmd expect (ADR-0009): Mesa
# 26.2.6 from freedesktop.org, our patch applied, and our renderer backend
# plus the driver's IOCTL header copied in beside the rest of Venus.
# Usage: powershell -File prepare.ps1 <directory for the clone>
param([Parameter(Mandatory)][string]$Dir)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$tag = 'mesa-26.2.6'
if (-not (Test-Path (Join-Path $Dir '.git'))) {
    git clone --depth 1 --branch $tag https://gitlab.freedesktop.org/mesa/mesa.git $Dir
    if ($LASTEXITCODE -ne 0) { throw "cannot clone $tag" }
}
git -C $Dir apply --whitespace=nowarn (Join-Path $here '0001-venus-builds-with-clang-cl.patch')
if ($LASTEXITCODE -ne 0) { throw 'the patch does not apply' }
$venus = Join-Path $Dir 'src\virtio\vulkan'
Copy-Item (Join-Path $here 'vn_renderer_entangled.c') $venus
Copy-Item (Join-Path $here '..\driver\public.h') (Join-Path $venus 'entangled_gpu_public.h')
Write-Output "prepared $Dir"

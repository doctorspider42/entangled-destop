# Makes the source tree build.cmd expects (ADR-0008, the vsync amendment):
# virtio-win's kvm-guest-drivers-windows at the pinned commit (only the two
# directories the display-only driver is built from), with our patch applied.
# Running it again on the same directory starts over from upstream.
# Usage: powershell -File prepare.ps1 <directory for the clone>
param([Parameter(Mandatory)][string]$Dir)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$origin = 'https://github.com/virtio-win/kvm-guest-drivers-windows.git'
# PROVENANCE.md says what this commit is and how to move it.
$commit = 'defa22ad4ba72b9a35662da40e29b0df40e9fab8'
function Invoke-Git {
    & git -C $Dir @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed ($LASTEXITCODE)" }
}
if (-not (Test-Path (Join-Path $Dir '.git'))) {
    & git init --quiet $Dir
    if ($LASTEXITCODE -ne 0) { throw "cannot create $Dir" }
    # The patch is LF, as the repository stores the sources: no conversion,
    # whatever this machine's git is configured to do.
    Invoke-Git config core.autocrlf false
    Invoke-Git remote add origin $origin
    Invoke-Git sparse-checkout set viogpu VirtIO
}
$head = & git -C $Dir rev-parse --quiet --verify HEAD 2>$null
if ($head -ne $commit) {
    Invoke-Git fetch --quiet --depth 1 --filter=blob:none origin $commit
    Invoke-Git checkout --quiet --force FETCH_HEAD
}
Invoke-Git checkout --quiet --force -- viogpu VirtIO
Invoke-Git apply --whitespace=nowarn (Join-Path $here '0001-refresh-rate-and-vsync.patch')
Write-Output "prepared $Dir"

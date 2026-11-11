# Makes the source tree build.cmd expects (ADR-0010): the viogpu3d WDDM driver
# of virtio-win PR 943 at the pinned commit (only the two directories it is
# built from), with our patches applied in order. Running it again on the same
# directory starts over from upstream.
# Usage: powershell -File prepare.ps1 <directory for the clone>
param([Parameter(Mandatory)][string]$Dir)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
# The PR's source branch; PROVENANCE.md says what this commit is.
$origin = 'https://github.com/max8rr8/kvm-guest-drivers-windows.git'
$commit = '9ed3aab11fb46e55dc835ff008008623b290a6cf'
function Invoke-Git {
    & git -C $Dir @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed ($LASTEXITCODE)" }
}
if (-not (Test-Path (Join-Path $Dir '.git'))) {
    & git init --quiet $Dir
    if ($LASTEXITCODE -ne 0) { throw "cannot create $Dir" }
    # The patches are LF, as the repository stores the sources: no conversion,
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
foreach ($patch in Get-ChildItem -Path $here -Filter '0*.patch' | Sort-Object Name) {
    Invoke-Git apply --whitespace=nowarn $patch.FullName
}
Write-Output "prepared $Dir"

# Builds entangled-gpu's attestation-signing submission (ADR-0009 phase F):
# the driver built fresh, its INF checked against the Hardware Dev Center's
# rules, and a CAB of it - signed with the publisher's EV code-signing
# certificate when one is given - ready to upload as a new driver submission
# in Partner Center (Hardware > Submit new hardware > "Attestation signing").
# Microsoft returns the package signed by itself: the driver then installs
# without test-signing mode or our test certificate.
#
# Usage: powershell -File package-attestation.ps1 <output directory>
#            [-Version 1.2.3.4] [-Thumbprint <EV certificate SHA-1>]
#
# Without -Thumbprint the CAB is left unsigned (everything up to the step
# only the certificate's owner can do). The certificate is looked for in the
# user's and the machine's "My" stores, as signtool does with /sha1.
param(
    [Parameter(Mandatory)][string]$Out,
    [string]$Version,
    [string]$Thumbprint,
    [string]$Timestamp = 'http://timestamp.digicert.com'
)
$ErrorActionPreference = 'Stop'
$kit = 'C:\Program Files (x86)\Windows Kits\10'
$bin = Join-Path $kit 'bin\10.0.26100.0\x64'
$tools = Join-Path $kit 'Tools\10.0.26100.0\x64'
$build = Join-Path $Out 'build'
$stage = Join-Path $Out 'entangled-gpu'
New-Item -ItemType Directory -Force $build, $stage | Out-Null

# 1. A fresh build (it stamps a new DriverVer every time).
& cmd /c "`"$PSScriptRoot\build.cmd`" `"$build`""
if ($LASTEXITCODE -ne 0) { throw "build.cmd failed ($LASTEXITCODE)" }
$inf = Join-Path $build 'entangled-gpu.inf'
if ($Version) {
    & (Join-Path $bin 'stampinf.exe') -f $inf -d * -v $Version -a amd64 -k 1.15 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "stampinf failed ($LASTEXITCODE)" }
}

# 2. The INF against the Windows-driver rules and the signing service's own.
foreach ($mode in '/w', '/h') {
    & (Join-Path $tools 'infverif.exe') $mode $inf | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "infverif $mode refused $inf" }
}

# 3. What the submission carries: the INF, the driver and its symbols (the
# service makes the catalog; ours is a test one).
foreach ($name in 'entangled-gpu.inf', 'entangled-gpu.sys', 'entangled-gpu.pdb') {
    Copy-Item (Join-Path $build $name) $stage -Force
}
$cab = Join-Path $Out 'entangled-gpu.cab'
$ddf = Join-Path $Out 'entangled-gpu.ddf'
@(
    '.OPTION EXPLICIT'
    '.Set CabinetFileCountThreshold=0'
    '.Set FolderFileCountThreshold=0'
    '.Set FolderSizeThreshold=0'
    '.Set MaxCabinetSize=0'
    '.Set MaxDiskFileCount=0'
    '.Set MaxDiskSize=0'
    '.Set CompressionType=MSZIP'
    '.Set Cabinet=on'
    '.Set Compress=on'
    '.Set CabinetNameTemplate=entangled-gpu.cab'
    ".Set DiskDirectoryTemplate=$Out"
    '.Set DestinationDir=entangled-gpu'
    ".Set InfFileName=$(Join-Path $Out 'setup.inf')"
    ".Set RptFileName=$(Join-Path $Out 'setup.rpt')"
    (Join-Path $stage 'entangled-gpu.inf')
    (Join-Path $stage 'entangled-gpu.sys')
    (Join-Path $stage 'entangled-gpu.pdb')
) | Set-Content -Encoding ASCII $ddf
& makecab.exe /F $ddf | Out-Null
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $cab)) { throw "makecab failed ($LASTEXITCODE)" }
Remove-Item -Force (Join-Path $Out 'setup.inf'), (Join-Path $Out 'setup.rpt') -ErrorAction SilentlyContinue

# 4. Signed with the EV certificate, when its owner runs this.
if ($Thumbprint) {
    & (Join-Path $bin 'signtool.exe') sign /fd sha256 /sha1 $Thumbprint /tr $Timestamp /td sha256 $cab
    if ($LASTEXITCODE -ne 0) { throw "signtool failed ($LASTEXITCODE)" }
    Write-Output "signed $cab - upload it in Partner Center (Hardware > Submit new hardware),"
    Write-Output "requesting Windows 10 x64 and later; the signed package it returns replaces"
    Write-Output "entangled-gpu.inf/.sys/.cat in the --gpu3d folder, and the test *.cer goes."
} else {
    Write-Output "built $cab (unsigned): sign it with -Thumbprint <your EV certificate>, then"
    Write-Output "upload it in Partner Center (Hardware > Submit new hardware)."
}

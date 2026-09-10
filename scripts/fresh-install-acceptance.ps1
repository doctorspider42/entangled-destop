<#
.SYNOPSIS
    Installs a PUBLISHED Entangled Desktop release into a scratch directory,
    with an empty per-user profile, and asserts that a newcomer's path works.

.DESCRIPTION
    Every other test in this project runs inside a checkout: `artifacts/` is
    populated, the cargo cache is warm, WSL is configured, `GITHUB_TOKEN` is
    set and `~/entangled-vms` already exists. A stranger has none of that, and
    for a while nothing in 1500 tests behaved like one. In a single evening the
    project's own author, installing the published release on a second machine,
    hit two blockers the suite could not see:

      1. `no artifacts/firmware/CLOUDHV.fd under F:\Program Files\Entangled
         Desktop` — every UEFI guest was impossible, because the firmware was a
         gitignored artifact that only ever existed in a source checkout;
      2. `execvpe entangled failed 2` — the WSL backend ran a Linux binary the
         Windows installer never shipped.

    Both are fixed. This script exists for the *class*: it starts from the
    artifact a stranger downloads, not from the tree, and every assertion below
    is one a stranger would have made.

    What "stranger" means here, concretely:

      * the setup .exe comes off GitHub Releases over plain HTTPS — no `gh`, no
        token, no checkout;
      * it is installed with /VERYSILENT into a scratch {app} that is not
        Program Files;
      * every command afterwards runs with a throwaway USERPROFILE, APPDATA and
        LOCALAPPDATA, so there is no manager settings file, no verified media
        cache, no ~/entangled-vms and no `gh` config;
      * every ENTANGLED_*, CARGO_*, XDG_* and *TOKEN variable is stripped, and
        the directory holding `gh.exe` is taken off PATH;
      * the working directory is an empty scratch folder with no repository at
        or above it — asserted, not assumed.

    NOTHING HERE MAY DISTURB AN EXISTING INSTALLATION. The .iss keeps one
    AppId for the life of the product (it is the upgrade identity), so a second
    install rewrites that AppId's uninstall registration and the shared Start
    Menu shortcuts, and uninstalling would then delete them. So the `install`
    stage snapshots both first and the `cleanup` stage puts them back —
    byte-for-byte for the registry key, file-for-file for the shortcuts. On a
    machine with no existing installation (a CI runner) all of that is a no-op.
    The user's VM directory, media cache, manager settings and WSL engine are
    never written at all, because the scrubbed profile means the installed
    program cannot even see them.

.PARAMETER Tag
    Release tag to install, e.g. v0.2.44. Default "latest" resolves through the
    GitHub API.

.PARAMETER Root
    Scratch root. Everything — the download, {app}, the fake profile, the
    working directory and any VM — lives under it. Default is a per-tag
    directory under TEMP.

.PARAMETER Stages
    Which stages to run, in this order regardless of how they are listed:
    download, install, tree, doctor, fetch, engine, guest.
    Default is everything except `guest`, which needs a hypervisor, a 2.9 GiB
    ISO and up to an hour.

.PARAMETER Iso
    The Ubuntu Server ISO for the `guest` stage. On Windows a newcomer has no
    `bash scripts/fetch-ubuntu-iso.sh`, so passing --iso by hand is the
    documented path and this parameter is that path.

.PARAMETER AppDir
    Test an installation that is ALREADY on this machine instead of installing
    one — typically `C:\Program Files\Entangled Desktop`. Everything else is
    unchanged: the empty per-user profile, the scrubbed environment, the
    repository-free working directory, every assertion. The `install` stage
    is refused with it (there is nothing to install) and cleanup uninstalls
    nothing.

    Two reasons it exists. Installing needs administrator rights and a UAC
    consent that a person has to click; the stages after it do not, and a run
    that cannot get that click should still be able to check a real
    installation. And "the copy that is actually on this machine" is a fair
    subject in its own right — it is the one the user will run.

.PARAMETER KeepInstalled
    Skip the uninstall and the scratch cleanup, for poking at the result. The
    existing installation's registration and shortcuts are still restored.

.PARAMETER JsonReport
    Also write the check table to this file as JSON.

.EXAMPLE
    # The whole thing a CI runner can do (no hypervisor needed):
    pwsh -File scripts\fresh-install-acceptance.ps1

.EXAMPLE
    # Everything, including taking a machine from nothing to a login prompt:
    pwsh -File scripts\fresh-install-acceptance.ps1 -Tag v0.2.44 `
        -Stages download,install,tree,doctor,fetch,engine,guest `
        -Iso "$env:LOCALAPPDATA\entangled\ubuntu\26.04\ubuntu-26.04-live-server-amd64.iso"

.NOTES
    Needs administrator rights: the installer is PrivilegesRequired=admin.
    Exit code 0 when every check passed, 1 otherwise.
#>

[CmdletBinding()]
param(
    [string] $Tag = 'latest',
    [string] $Root,
    [ValidateSet('download', 'install', 'tree', 'doctor', 'fetch', 'engine', 'guest')]
    [string[]] $Stages = @('download', 'install', 'tree', 'doctor', 'fetch', 'engine'),
    [string] $Iso,
    [string] $AppDir,
    [switch] $KeepInstalled,
    [string] $JsonReport
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# `reg query` on a key that is not there is an answer, not an error, and
# PowerShell 7.4 turns a non-zero native exit code into a terminating one.
$PSNativeCommandUseErrorActionPreference = $false

# The repository the release comes from. Spelled out rather than derived from
# `git remote`, because this script must run where there is no checkout.
$Repo = 'doctorspider42/entangled-destop'

# The upgrade identity in installer\entangled.iss. It never changes, which is
# exactly why a scratch install collides with a real one.
$AppId = '{EF1CC6F3-68F0-43EE-BABF-12296005FFDE}_is1'
$UninstallKeys = @(
    "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$AppId",
    "HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\$AppId"
)

# ---------------------------------------------------------------------------
# The check table
# ---------------------------------------------------------------------------

$script:Checks = [System.Collections.Generic.List[object]]::new()

function Add-Check {
    param(
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [ValidateSet('PASS', 'FAIL', 'SKIP', 'INFO')] [string] $Status,
        [string] $Detail = ''
    )
    $script:Checks.Add([pscustomobject]@{ Name = $Name; Status = $Status; Detail = $Detail })
    $colour = switch ($Status) { 'PASS' { 'Green' } 'FAIL' { 'Red' } 'SKIP' { 'Yellow' } default { 'Gray' } }
    Write-Host ("  [{0}] {1}" -f $Status, $Name) -ForegroundColor $colour
    if ($Detail) { Write-Host ("         {0}" -f ($Detail -replace "`r?`n", "`n         ")) -ForegroundColor DarkGray }
}

function Assert-Check {
    param(
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [bool] $Condition,
        [string] $Detail = ''
    )
    Add-Check -Name $Name -Status ($Condition ? 'PASS' : 'FAIL') -Detail $Detail
    return $Condition
}

function Write-Stage {
    param([Parameter(Mandatory)][string] $Name)
    Write-Host ''
    Write-Host "== $Name " -ForegroundColor Cyan -NoNewline
    Write-Host ('=' * [Math]::Max(0, 60 - $Name.Length)) -ForegroundColor Cyan
}

# ---------------------------------------------------------------------------
# The stranger's environment
# ---------------------------------------------------------------------------

$script:SavedEnv = $null

<#
    Applies the throwaway profile to THIS process, so children inherit it.
    Saved and restored rather than set once, because the download and install
    stages need the real environment (network proxies, admin profile) and only
    the *installed program* must be run as a stranger.
#>
function Enter-StrangerEnv {
    param([Parameter(Mandatory)][hashtable] $ProfileVars)

    if ($null -ne $script:SavedEnv) { throw 'already inside a stranger environment' }
    $saved = @{}

    # Variables a developer has and a newcomer does not. ENTANGLED_* would
    # hand the program a firmware or a cache it should have had to find for
    # itself; the tokens would turn a public download into an authenticated
    # one and hide a 404 that a stranger would meet.
    $drop = @()
    foreach ($entry in [System.Environment]::GetEnvironmentVariables().Keys) {
        $key = [string]$entry
        if ($key -match '^(ENTANGLED_|CARGO|RUST|XDG_)' -or
            $key -match '(^|_)(GITHUB_TOKEN|GH_TOKEN|GH_CONFIG_DIR|GH_HOST)$' -or
            $key -eq 'HOME') {
            $drop += $key
        }
    }
    foreach ($key in $drop) {
        $saved[$key] = [System.Environment]::GetEnvironmentVariable($key)
        [System.Environment]::SetEnvironmentVariable($key, $null)
    }
    foreach ($key in $ProfileVars.Keys) {
        $saved[$key] = [System.Environment]::GetEnvironmentVariable($key)
        [System.Environment]::SetEnvironmentVariable($key, $ProfileVars[$key])
    }

    # A stranger has no `gh` on PATH. Scrubbing APPDATA already hides its
    # config file, but modern gh keeps tokens in Windows credential storage,
    # which no environment variable hides — so take the program itself away.
    $saved['PATH'] = $env:PATH
    $ghDirs = @(Get-Command gh -All -ErrorAction SilentlyContinue |
        ForEach-Object { Split-Path -Parent $_.Source } | Sort-Object -Unique)
    if ($ghDirs.Count -gt 0) {
        $kept = ($env:PATH -split ';' | Where-Object {
                $dir = $_.TrimEnd('\')
                $dir -and -not ($ghDirs | Where-Object { $_.TrimEnd('\') -ieq $dir })
            }) -join ';'
        [System.Environment]::SetEnvironmentVariable('PATH', $kept)
    }

    $script:SavedEnv = $saved
}

function Exit-StrangerEnv {
    if ($null -eq $script:SavedEnv) { return }
    foreach ($key in $script:SavedEnv.Keys) {
        [System.Environment]::SetEnvironmentVariable($key, $script:SavedEnv[$key])
    }
    $script:SavedEnv = $null
}

<#
    Runs an installed binary the way a newcomer's shell would, capturing
    everything it printed.

    Returns @{ ExitCode; Output; TimedOut; Log }. stdout and stderr go to one
    file, because `doctor`'s verdict is on stderr and its inventory is on
    stdout, and a check that reads only one of them reads half the answer.
#>
function Invoke-Stranger {
    param(
        [Parameter(Mandatory)] [string] $Exe,
        [string[]] $Arguments = @(),
        [Parameter(Mandatory)] [hashtable] $ProfileVars,
        [Parameter(Mandatory)] [string] $WorkDir,
        [int] $TimeoutSec = 120,
        [string] $Log,
        [string] $UntilMarker
    )

    if (-not $Log) {
        $Log = Join-Path $script:LogDir ('run-{0:D3}.log' -f (++$script:LogSeq))
    }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Log) | Out-Null
    $errLog = "$Log.err"

    Enter-StrangerEnv -ProfileVars $ProfileVars
    try {
        $proc = Start-Process -FilePath $Exe -ArgumentList $Arguments -WorkingDirectory $WorkDir `
            -NoNewWindow -PassThru -RedirectStandardOutput $Log -RedirectStandardError $errLog
        $deadline = (Get-Date).AddSeconds($TimeoutSec)
        $sawMarker = $false
        while (-not $proc.HasExited -and (Get-Date) -lt $deadline) {
            if ($UntilMarker) {
                $sofar = Get-Content -Raw -LiteralPath $Log -ErrorAction SilentlyContinue
                if ($sofar -and $sofar.Contains($UntilMarker)) { $sawMarker = $true; break }
            }
            Start-Sleep -Milliseconds 500
        }
        $timedOut = $false
        if (-not $proc.HasExited) {
            if (-not $sawMarker) { $timedOut = $true }
            # A headless VM has no window and no shutdown command on its
            # control channel, so this is how the boot check ends — the same
            # thing apps/entangled/tests/ubuntu_install.rs does.
            try { $proc.Kill($true) } catch { }
            $proc.WaitForExit(30000) | Out-Null
        }
        $out = (Get-Content -Raw -LiteralPath $Log -ErrorAction SilentlyContinue) ?? ''
        $err = (Get-Content -Raw -LiteralPath $errLog -ErrorAction SilentlyContinue) ?? ''
        if ($err) { $out = $out + $err; Add-Content -LiteralPath $Log -Value $err }
        Remove-Item -LiteralPath $errLog -ErrorAction SilentlyContinue
        return @{
            ExitCode   = ($proc.HasExited ? $proc.ExitCode : -1)
            Output     = $out
            TimedOut   = $timedOut
            SawMarker  = $sawMarker
            Log        = $Log
        }
    }
    finally {
        Exit-StrangerEnv
    }
}

# ---------------------------------------------------------------------------
# Layout
# ---------------------------------------------------------------------------

if (-not $Root) {
    $Root = Join-Path ([System.IO.Path]::GetTempPath()) ("entangled-fresh-" + ($Tag -replace '[^A-Za-z0-9._-]', '_'))
}
$Root = [System.IO.Path]::GetFullPath($Root)

# The {app} under test: the scratch one this run installs, or an existing
# installation named by -AppDir.
$ExistingApp = [bool] $AppDir
$AppDir = $ExistingApp ? [System.IO.Path]::GetFullPath($AppDir) : (Join-Path $Root 'app')
$DownloadDir  = Join-Path $Root 'download'
$ProfileDir   = Join-Path $Root 'profile'      # the throwaway USERPROFILE
$CwdDir       = Join-Path $Root 'cwd'          # a working directory with no repo
$VmDir        = Join-Path $Root 'vm'
$BackupDir    = Join-Path $Root 'preexisting'  # what we must put back
$script:LogDir = Join-Path $Root 'logs'
$script:LogSeq = 0

# The real installation must be somewhere else entirely.
foreach ($sacred in @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:USERPROFILE)) {
    if ($sacred -and $Root.StartsWith((Join-Path $sacred 'Entangled Desktop'), 'OrdinalIgnoreCase')) {
        throw "refusing to use $Root as the scratch root: that is a real installation directory"
    }
}

$StrangerProfile = @{
    USERPROFILE   = $ProfileDir
    APPDATA       = Join-Path $ProfileDir 'AppData\Roaming'
    LOCALAPPDATA  = Join-Path $ProfileDir 'AppData\Local'
    TEMP          = Join-Path $ProfileDir 'AppData\Local\Temp'
    TMP           = Join-Path $ProfileDir 'AppData\Local\Temp'
    HOMEDRIVE     = [System.IO.Path]::GetPathRoot($ProfileDir).TrimEnd('\')
    HOMEPATH      = $ProfileDir.Substring([System.IO.Path]::GetPathRoot($ProfileDir).Length - 1)
}

$Entangled = Join-Path $AppDir 'entangled.exe'
$Manager   = Join-Path $AppDir 'entangled-manager.exe'
$Firmware  = Join-Path $AppDir 'artifacts\firmware\CLOUDHV.fd'

$order = @('download', 'install', 'tree', 'doctor', 'fetch', 'engine', 'guest')
$run = [ordered]@{}
foreach ($stage in $order) { $run[$stage] = ($Stages -contains $stage) }
if ($ExistingApp) {
    if ($run['install']) {
        throw "-AppDir names an installation to test; it cannot also be installed. Drop 'install' from -Stages."
    }
    if (-not (Test-Path (Join-Path $AppDir 'entangled.exe'))) {
        throw "-AppDir $AppDir holds no entangled.exe"
    }
}

Write-Host ''
Write-Host 'Entangled Desktop — fresh-install acceptance' -ForegroundColor White
Write-Host "  repository : $Repo"
Write-Host "  tag        : $Tag"
Write-Host "  scratch    : $Root"
Write-Host "  installation: $AppDir$($ExistingApp ? ' (existing, -AppDir)' : ' (installed by this run)')"
Write-Host "  stages     : $(($order | Where-Object { $run[$_] }) -join ', ')"

$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "  elevated   : $isAdmin"
if ($run['install'] -and -not $isAdmin) {
    throw 'the installer is PrivilegesRequired=admin. Run this in an elevated shell, or drop the ' +
        'install stage and point -AppDir at an installation you already have'
}

foreach ($dir in @($Root, $DownloadDir, $ProfileDir, $CwdDir, $script:LogDir, $BackupDir,
        $StrangerProfile.APPDATA, $StrangerProfile.LOCALAPPDATA, $StrangerProfile.TEMP)) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
}

# ---------------------------------------------------------------------------
# Stage: download the published installer
# ---------------------------------------------------------------------------

$script:Setup = $null
$script:Version = $null

if ($run['download']) {
    Write-Stage 'download — the artifact a stranger gets'

    # Unauthenticated, over the same public URL a browser would use. If this
    # ever needs a token again, the repository went private and every newcomer
    # is blocked — which is a finding, not a reason to add one here.
    $api = ($Tag -eq 'latest') ?
        "https://api.github.com/repos/$Repo/releases/latest" :
        "https://api.github.com/repos/$Repo/releases/tags/$Tag"
    $headers = @{ 'User-Agent' = 'entangled-fresh-install-acceptance'; 'Accept' = 'application/vnd.github+json' }
    $release = Invoke-RestMethod -Uri $api -Headers $headers -MaximumRedirection 5
    $script:Version = ($release.tag_name -replace '^v', '')
    Add-Check -Name 'the release resolves without credentials' -Status 'PASS' `
        -Detail "$($release.tag_name) — $($release.name)"

    $asset = $release.assets | Where-Object { $_.name -like 'entangled-desktop-*-setup.exe' } | Select-Object -First 1
    if (-not (Assert-Check -Name 'the release carries a Windows setup .exe' -Condition ($null -ne $asset) `
                -Detail (($release.assets | ForEach-Object { $_.name }) -join ', '))) {
        throw 'nothing to install'
    }

    $script:Setup = Join-Path $DownloadDir $asset.name
    if ((Test-Path $script:Setup) -and (Get-Item $script:Setup).Length -eq $asset.size) {
        Add-Check -Name 'setup .exe already downloaded' -Status 'INFO' -Detail $script:Setup
    }
    else {
        Write-Host "  downloading $($asset.name) ($([math]::Round($asset.size/1MB,1)) MiB)..."
        Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $script:Setup -Headers $headers
    }
    $len = (Get-Item $script:Setup).Length
    Assert-Check -Name 'the setup .exe downloaded whole' -Condition ($len -eq $asset.size) `
        -Detail "$len bytes, sha256 $((Get-FileHash $script:Setup -Algorithm SHA256).Hash.ToLower())" | Out-Null

    # There is no signature and no published checksum; the guide says so, and
    # a check that pretended otherwise would be the lie. Record what arrived.
    $sig = Get-AuthenticodeSignature $script:Setup
    Add-Check -Name 'code signature' -Status 'INFO' `
        -Detail "$($sig.Status) — the project ships unsigned binaries (docs/user-guide.md)"
}
else {
    $script:Setup = Get-ChildItem -Path $DownloadDir -Filter 'entangled-desktop-*-setup.exe' -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty FullName
}

# ---------------------------------------------------------------------------
# Stage: install into a scratch {app}, without disturbing a real one
# ---------------------------------------------------------------------------

<#
    Snapshots the two things a same-AppId install overwrites and an uninstall
    then deletes: the Add/Remove Programs registration, and the shared Start
    Menu group. Nothing else in the .iss writes outside {app}.
#>
function Backup-ExistingInstallation {
    $found = @()
    foreach ($key in $UninstallKeys) {
        $out = & reg.exe query $key 2>&1
        if ($LASTEXITCODE -eq 0) {
            $file = Join-Path $BackupDir (($key -replace '[\\{}]', '_') + '.reg')
            & reg.exe export $key $file /y | Out-Null
            $found += $file
        }
    }
    $group = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Entangled Desktop'
    if (Test-Path $group) {
        Copy-Item -Recurse -Force -LiteralPath $group -Destination (Join-Path $BackupDir 'StartMenu')
        $found += $group
    }
    foreach ($desktop in @((Join-Path $env:PUBLIC 'Desktop\Entangled Desktop.lnk'),
            (Join-Path $env:USERPROFILE 'Desktop\Entangled Desktop.lnk'))) {
        if (Test-Path $desktop) {
            Copy-Item -Force -LiteralPath $desktop -Destination (Join-Path $BackupDir (Split-Path -Leaf $desktop))
            $found += $desktop
        }
    }
    if ($found.Count -gt 0) {
        Add-Check -Name 'an existing installation was found and snapshotted' -Status 'INFO' `
            -Detail (($found | ForEach-Object { "  $_" }) -join "`n")
    }
    else {
        Add-Check -Name 'no existing installation on this host' -Status 'INFO' `
            -Detail 'nothing to protect — this is what a CI runner looks like'
    }
    return $found.Count -gt 0
}

function Restore-ExistingInstallation {
    foreach ($file in (Get-ChildItem -Path $BackupDir -Filter '*.reg' -ErrorAction SilentlyContinue)) {
        & reg.exe import $file.FullName 2>&1 | Out-Null
    }
    $savedGroup = Join-Path $BackupDir 'StartMenu'
    if (Test-Path $savedGroup) {
        $group = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Entangled Desktop'
        New-Item -ItemType Directory -Force -Path $group | Out-Null
        Copy-Item -Recurse -Force -Path (Join-Path $savedGroup '*') -Destination $group
    }
    foreach ($pair in @(@{ f = 'Entangled Desktop.lnk'; d = (Join-Path $env:PUBLIC 'Desktop') })) {
        $saved = Join-Path $BackupDir $pair.f
        if (Test-Path $saved) { Copy-Item -Force -LiteralPath $saved -Destination (Join-Path $pair.d $pair.f) }
    }
}

$script:HadExisting = $false

if ($run['install']) {
    Write-Stage 'install — /VERYSILENT into a scratch {app}'
    if (-not $script:Setup -or -not (Test-Path $script:Setup)) { throw 'no setup .exe; run the download stage' }

    $script:HadExisting = Backup-ExistingInstallation

    $setupLog = Join-Path $script:LogDir 'setup.log'
    $setupArgs = @(
        '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/NOCANCEL',
        # Neither optional task: the desktop icon would land in the user's
        # profile, and `wslengine` would write a Linux engine into their WSL.
        '/MERGETASKS=!desktopicon,!wslengine',
        "/DIR=$AppDir",
        "/LOG=$setupLog"
    )
    $proc = Start-Process -FilePath $script:Setup -ArgumentList $setupArgs -Wait -PassThru
    Assert-Check -Name 'the published setup installs unattended' -Condition ($proc.ExitCode -eq 0) `
        -Detail "exit code $($proc.ExitCode); log $setupLog" | Out-Null
}

# ---------------------------------------------------------------------------
# Stage: what actually landed
# ---------------------------------------------------------------------------

if ($run['tree']) {
    Write-Stage 'tree — what the installer actually shipped'

    foreach ($rel in @('entangled.exe', 'entangled-manager.exe', 'entangled.ico',
            'LICENSE', 'THIRD-PARTY-NOTICES.txt', 'README.md')) {
        $path = Join-Path $AppDir $rel
        Assert-Check -Name "installed: $rel" -Condition (Test-Path $path) -Detail $path | Out-Null
    }

    # ---- REGRESSION 1 ----------------------------------------------------
    # `no artifacts/firmware/CLOUDHV.fd under F:\Program Files\Entangled
    # Desktop`. The firmware is not an optional extra: every Ubuntu and every
    # Fedora machine boots through it, and a Windows user cannot build one
    # (EDK2 does not build on Windows). This is the assertion that would have
    # failed on the day that installer shipped.
    $haveFw = Test-Path $Firmware
    Assert-Check -Name 'REGRESSION: the UEFI firmware ships with the installer' -Condition $haveFw `
        -Detail ($haveFw ?
            "$Firmware ($((Get-Item $Firmware).Length) bytes)" :
            "MISSING — every UEFI guest is impossible without it. installer\entangled.iss must ship {#FirmwareDir}\CLOUDHV.fd with NO skipifsourcedoesntexist") | Out-Null
    if ($haveFw) {
        # 4 MiB exactly, because that is what a pflash-backed CloudHv build is
        # and a truncated one boots to nothing.
        Assert-Check -Name 'the firmware is a whole 4 MiB image' `
            -Condition ((Get-Item $Firmware).Length -eq 4MB) `
            -Detail "sha256 $((Get-FileHash $Firmware -Algorithm SHA256).Hash.ToLower())" | Out-Null
        Add-Check -Name 'firmware provenance note' `
            -Status ((Test-Path "$Firmware.provenance") ? 'PASS' : 'INFO') `
            -Detail ((Test-Path "$Firmware.provenance") ?
                ((Get-Content -Raw "$Firmware.provenance").Trim() -replace "`r?`n", '; ') :
                'no .provenance beside the firmware (optional: only /DFirmwareDir builds lack one)')
    }

    # It must be at the path the resolver searches, which is relative to the
    # EXECUTABLE, not to the working directory (apps/entangled/src/firmware.rs
    # install_dir()). A firmware installed one directory over is a firmware
    # nobody finds.
    Assert-Check -Name 'the firmware sits beside entangled.exe, where install_dir() looks' `
        -Condition ($haveFw -and (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $Firmware))) -eq (Split-Path -Parent $Entangled)) `
        -Detail "exe $Entangled" | Out-Null
}

# ---------------------------------------------------------------------------
# The stranger's working directory: no repository at or above it
# ---------------------------------------------------------------------------

$repoAbove = $null
$probe = Get-Item -LiteralPath $CwdDir
while ($probe) {
    if ((Test-Path (Join-Path $probe.FullName 'Cargo.toml')) -or (Test-Path (Join-Path $probe.FullName 'artifacts'))) {
        $repoAbove = $probe.FullName; break
    }
    $probe = $probe.Parent
}
Assert-Check -Name 'the working directory has no repository at or above it' -Condition ($null -eq $repoAbove) `
    -Detail ($repoAbove ? "found a checkout at $repoAbove — the checkout fallback could answer for the installation" : $CwdDir) | Out-Null

# ---------------------------------------------------------------------------
# Stage: doctor
# ---------------------------------------------------------------------------

$script:DoctorOut = ''

if ($run['doctor']) {
    Write-Stage 'doctor — what a newcomer is told about their host'
    if (-not (Test-Path $Entangled)) { throw "no $Entangled; run the install stage" }

    # NOT $version: PowerShell variable names are case-insensitive and an `if`
    # block is not a scope, so `$version` here IS `$script:Version` — which
    # would replace the published version string with this hashtable, and did.
    $versionRun = Invoke-Stranger -Exe $Entangled -Arguments @('--version') -ProfileVars $StrangerProfile -WorkDir $CwdDir
    Assert-Check -Name 'the installed entangled.exe runs' -Condition ($versionRun.ExitCode -eq 0) `
        -Detail $versionRun.Output.Trim() | Out-Null
    if ($script:Version) {
        Assert-Check -Name 'the installed binary reports the published version' `
            -Condition ($versionRun.Output -match [regex]::Escape($script:Version)) `
            -Detail "expected $($script:Version), got $($versionRun.Output.Trim())" | Out-Null
    }

    $doctor = Invoke-Stranger -Exe $Entangled -Arguments @('doctor') -ProfileVars $StrangerProfile -WorkDir $CwdDir -TimeoutSec 240
    $script:DoctorOut = $doctor.Output
    Write-Host '  --- entangled doctor ---' -ForegroundColor DarkGray
    $doctor.Output -split "`r?`n" | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }

    $hypervisorOk = $doctor.ExitCode -eq 0
    Add-Check -Name 'hypervisor verdict' -Status ($hypervisorOk ? 'PASS' : 'INFO') `
        -Detail ($hypervisorOk ? 'this host can run VMs' :
            'this host cannot run VMs (a GitHub runner never can) — the inventory below must be reported anyway')

    # doctor must answer the inventory questions whether or not the hypervisor
    # is usable. Before this was true, the host that most needed the answer —
    # someone who has just enabled a Windows feature and not rebooted — got one
    # line and an error, and CI could assert nothing at all.
    Assert-Check -Name 'doctor reports the install inventory even without a hypervisor' `
        -Condition ($doctor.Output -match '(?m)^\s*install\s+:') `
        -Detail ($hypervisorOk ?
            'the "install :" section is the portable half of doctor' :
            'this host has no hypervisor and doctor printed no inventory — a release older than the doctor change in apps/entangled/src/doctor.rs report(), which prints what the host HAS before it fails on what it cannot do') | Out-Null

    # ---- REGRESSION 1, the resolver half ---------------------------------
    # The file being present is half the bug; the program looking there is the
    # other half. `from this installation` is Origin::Install — row 3 of the
    # five-place lookup, the only row an ordinary user has.
    $fwLine = ($doctor.Output -split "`r?`n" | Where-Object { $_ -match '^\s*firmware\s' } | Select-Object -First 1)
    Assert-Check -Name 'REGRESSION: doctor resolves the firmware, from this installation' `
        -Condition ($null -ne $fwLine -and $fwLine -match 'from this installation') `
        -Detail ($fwLine ?? 'no firmware line in the output at all') | Out-Null
    if ($fwLine) {
        Assert-Check -Name 'the firmware doctor found is the one that was installed' `
            -Condition ($fwLine -match [regex]::Escape($AppDir)) `
            -Detail "expected a path under $AppDir" | Out-Null
    }
    Assert-Check -Name 'doctor does not report the firmware MISSING' `
        -Condition ($doctor.Output -notmatch 'firmware\s+MISSING') -Detail '' | Out-Null

    # ---- REGRESSION 2 ----------------------------------------------------
    # `execvpe entangled failed 2`. Either the Linux engine is in WSL, or the
    # program says — in this output, without being asked twice — exactly how to
    # put it there. This host's WSL is the user's, so both outcomes pass; what
    # must never happen is silence.
    $lines = $doctor.Output -split "`r?`n"
    $wslIdx = -1
    for ($i = 0; $i -lt $lines.Count; $i++) { if ($lines[$i] -match '^\s*wsl\s+\S') { $wslIdx = $i; break } }
    if ($wslIdx -lt 0) {
        Assert-Check -Name 'REGRESSION: doctor answers for the WSL (KVM) engine' -Condition $false `
            -Detail 'no "wsl" engine line — the backend that used to die with `execvpe entangled failed 2` is unreported' | Out-Null
    }
    else {
        $wslLine = $lines[$wslIdx].Trim()
        if ($wslLine -match 'MISSING') {
            # Only the continuation lines: doctor indents the fix further than
            # the label, and the next section starts back at the label column.
            $indent = $lines[$wslIdx].Length - $lines[$wslIdx].TrimStart().Length
            $fixLines = @()
            for ($j = $wslIdx + 1; $j -lt $lines.Count; $j++) {
                $line = $lines[$j]
                if (-not $line.Trim()) { break }
                if (($line.Length - $line.TrimStart().Length) -le $indent) { break }
                $fixLines += $line.Trim()
            }
            $fix = $fixLines -join ' '
            Assert-Check -Name 'REGRESSION: no engine in WSL, and doctor says how to get one' `
                -Condition ($fix -match 'install-engine|wsl --install|Settings') `
                -Detail "$wslLine`n$fix" | Out-Null
        }
        else {
            Assert-Check -Name 'REGRESSION: the Linux engine in WSL is present and runs' -Condition $true `
                -Detail $wslLine | Out-Null
        }
    }

    # The scrub took: a newcomer's machines go into THEIR profile, not into the
    # developer's ~/entangled-vms. If this fails, every other check in this
    # script was reading the developer's environment.
    $vmLine = ($lines | Where-Object { $_ -match '^\s*VM directory' } | Select-Object -First 1)
    Assert-Check -Name 'the per-user profile really is empty and scratch' `
        -Condition ($null -ne $vmLine -and $vmLine -match [regex]::Escape($ProfileDir)) `
        -Detail ($vmLine ?? 'no VM directory line') | Out-Null

    $isoLine = ($lines | Where-Object { $_ -match '^\s*ubuntu ISO' } | Select-Object -First 1)
    $isoEmpty = ($null -ne $isoLine) -and ($isoLine -match 'MISSING')
    Add-Check -Name 'the media cache starts empty, the way a newcomer''s does' `
        -Status ($isoEmpty ? 'PASS' : 'INFO') -Detail ($isoLine ?? '')
}

# ---------------------------------------------------------------------------
# Stage: fetch — the other way a newcomer gets the firmware
# ---------------------------------------------------------------------------

if ($run['fetch']) {
    Write-Stage 'fetch — `entangled fetch firmware`, with no token'

    $fetch = Invoke-Stranger -Exe $Entangled -Arguments @('fetch', 'firmware') `
        -ProfileVars $StrangerProfile -WorkDir $CwdDir -TimeoutSec 300
    $ok = $fetch.ExitCode -eq 0
    Assert-Check -Name 'a newcomer with no GitHub credentials can fetch the firmware' -Condition $ok `
        -Detail (($fetch.Output -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -Last 4) -join "`n") | Out-Null

    if ($ok) {
        $cached = Get-ChildItem -Recurse -Path (Join-Path $StrangerProfile.LOCALAPPDATA 'entangled\firmware') `
            -Filter 'CLOUDHV.fd' -ErrorAction SilentlyContinue | Select-Object -First 1
        Assert-Check -Name 'the fetched firmware lands in the (empty) verified cache' -Condition ($null -ne $cached) `
            -Detail ($cached ? $cached.FullName : '') | Out-Null
        if ($cached -and (Test-Path $Firmware)) {
            # release.yml's firmware job downloads the pinned asset and refuses
            # to build an installer around anything else, so these two SHOULD
            # be the same bytes. They can legitimately differ for a setup built
            # before the firmware was ever published, when that job falls back
            # to building EDK2 itself. So: information, with both digests,
            # rather than a failure that would cry wolf on such a release.
            $pinnedHash = (Get-FileHash $cached.FullName -Algorithm SHA256).Hash.ToLower()
            $shippedHash = (Get-FileHash $Firmware -Algorithm SHA256).Hash.ToLower()
            if ($pinnedHash -eq $shippedHash) {
                Add-Check -Name 'the shipped firmware is the pinned published firmware' `
                    -Status 'PASS' -Detail $pinnedHash
            }
            else {
                $detail = "different builds — this setup predates the published firmware" +
                    "`n  published $pinnedHash" +
                    "`n  installed $shippedHash" +
                    "`nrelease.yml pins them together from now on"
                Add-Check -Name 'the shipped firmware is the pinned published firmware' `
                    -Status 'INFO' -Detail $detail
            }
        }
    }
}

# ---------------------------------------------------------------------------
# Stage: engine — the WSL half is reachable from the shipped artifact
# ---------------------------------------------------------------------------

if ($run['engine']) {
    Write-Stage 'engine — the Linux half the installer cannot ship'

    # The command the manager's button and the installer's optional task both
    # run must exist in the SHIPPED binary. Checked with --help so nothing is
    # downloaded and the user's WSL is never touched.
    $help = Invoke-Stranger -Exe $Entangled -Arguments @('wsl', 'install-engine', '--help') `
        -ProfileVars $StrangerProfile -WorkDir $CwdDir
    $hasWslCli = $help.ExitCode -eq 0 -and $help.Output -match '--distro'
    Assert-Check -Name 'REGRESSION: the shipped CLI can install the Linux engine into WSL' `
        -Condition $hasWslCli `
        -Detail ($hasWslCli ?
            'entangled wsl install-engine — what the manager button and the installer task both run' :
            "this release has no `wsl` subcommand, so installer/entangled.iss's optional task would exit -1 and the manager's Settings button is the only way in:`n" +
            (($help.Output -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -First 3) -join "`n")) | Out-Null

    # The installer offers the same thing at install time. Read out of the
    # setup log, which records the task list even when nothing was selected.
    $setupLog = Join-Path $script:LogDir 'setup.log'
    if (Test-Path $setupLog) {
        $log = Get-Content -Raw $setupLog
        # Not "does the log mention wslengine" — it does, because our own
        # /MERGETASKS is the first line Inno writes. `WSL engine setup:` is
        # what SetupWslEngine logs when it actually runs, so its absence is
        # evidence about the run rather than about the command line.
        Assert-Check -Name 'this run did NOT touch the WSL engine' `
            -Condition ($log -notmatch 'WSL engine setup:') `
            -Detail '/MERGETASKS=!wslengine — the user''s distribution is left exactly as it was' | Out-Null
    }

    # And the release publishes the asset the download needs. Same
    # unauthenticated route as everything else here.
    if ($script:Version) {
        try {
            $headers = @{ 'User-Agent' = 'entangled-fresh-install-acceptance'; 'Accept' = 'application/vnd.github+json' }
            $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/tags/v$($script:Version)" -Headers $headers
            $asset = $rel.assets | Where-Object { $_.name -eq 'entangled-linux-x86_64' }
            Assert-Check -Name 'the release publishes the Linux engine the installer will download' `
                -Condition ($null -ne $asset) `
                -Detail ($asset ? "$($asset.name), $([math]::Round($asset.size/1MB,1)) MiB" : 'no entangled-linux-x86_64 asset') | Out-Null
        }
        catch {
            Add-Check -Name 'the release publishes the Linux engine' -Status 'FAIL' -Detail $_.Exception.Message
        }
    }
}

# ---------------------------------------------------------------------------
# Stage: guest — nothing to a login prompt
# ---------------------------------------------------------------------------

if ($run['guest']) {
    Write-Stage 'guest — from nothing to a login prompt'

    if (-not $Iso) {
        # A newcomer on Windows has no `bash scripts/fetch-ubuntu-iso.sh`; the
        # guide tells them to download the ISO and pass --iso. So must we.
        Add-Check -Name 'guest install' -Status 'SKIP' -Detail 'no -Iso given (2.9 GiB; see docs/user-guide.md)'
    }
    elseif (-not (Test-Path $Iso)) {
        Add-Check -Name 'guest install' -Status 'FAIL' -Detail "no such ISO: $Iso"
    }
    else {
        New-Item -ItemType Directory -Force -Path $VmDir | Out-Null
        $disk = Join-Path $VmDir 'fresh-ubuntu.raw'
        $profileToml = Join-Path $VmDir 'fresh-ubuntu.toml'

        Write-Host "  installing Ubuntu Server (up to 40 minutes)..." -ForegroundColor DarkGray
        $install = Invoke-Stranger -Exe $Entangled -ProfileVars $StrangerProfile -WorkDir $CwdDir -TimeoutSec (45 * 60) `
            -Log (Join-Path $script:LogDir 'guest-install.log') `
            -Arguments @('install', 'ubuntu', '--disk', $disk, '--size', '16G', '--auto', '--headless', '--iso', $Iso)

        $installedOk = ($install.ExitCode -eq 0) -and (Test-Path $profileToml)
        Assert-Check -Name 'a UEFI machine installs from nothing, on a fresh installation' -Condition $installedOk `
            -Detail "exit $($install.ExitCode); log $($install.Log)" | Out-Null

        if ($installedOk) {
            # Nothing in the generated profile may point back at a checkout.
            $toml = Get-Content -Raw $profileToml
            Add-Check -Name 'generated profile' -Status 'INFO' -Detail $toml.Trim()
            Assert-Check -Name 'the machine boots via UEFI with a persistent NVRAM store' `
                -Condition ($toml -match 'mode\s*=\s*"uefi"' -and $toml -match 'nvram') -Detail '' | Out-Null

            Write-Host '  booting what was installed (up to 6 minutes)...' -ForegroundColor DarkGray
            $boot = Invoke-Stranger -Exe $Entangled -ProfileVars $StrangerProfile -WorkDir $CwdDir -TimeoutSec (6 * 60) `
                -Log (Join-Path $script:LogDir 'guest-boot.log') -UntilMarker 'login:' `
                -Arguments @('run', '--headless', $profileToml)

            Assert-Check -Name 'the installed system reaches a login prompt' -Condition $boot.SawMarker `
                -Detail (($boot.Output -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -Last 6) -join "`n") | Out-Null
            foreach ($needle in @('shimx64.efi', 'GNU GRUB', 'Welcome to Ubuntu')) {
                Assert-Check -Name "boot chain: $needle" -Condition ($boot.Output -match [regex]::Escape($needle)) -Detail '' | Out-Null
            }
        }
    }
}

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

Write-Stage 'cleanup'

if ($KeepInstalled) {
    Add-Check -Name 'cleanup' -Status 'SKIP' -Detail "-KeepInstalled: $AppDir left in place"
    if ($script:HadExisting) { Restore-ExistingInstallation }
}
elseif ($ExistingApp) {
    Add-Check -Name 'cleanup' -Status 'INFO' `
        -Detail "-AppDir: $AppDir was here before this run and is left exactly as it was"
    foreach ($dir in @($ProfileDir, $VmDir, $CwdDir)) {
        Remove-Item -Recurse -Force -LiteralPath $dir -ErrorAction SilentlyContinue
    }
}
else {
    $unins = Join-Path $AppDir 'unins000.exe'
    if (Test-Path $unins) {
        # The uninstaller re-launches itself from TEMP and the first process
        # returns immediately, so -Wait is not enough on its own.
        Start-Process -FilePath $unins -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait
        $deadline = (Get-Date).AddSeconds(120)
        while ((Test-Path $unins) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
        Assert-Check -Name 'the scratch installation uninstalls cleanly' -Condition (-not (Test-Path $unins)) `
            -Detail $AppDir | Out-Null
    }
    if ($script:HadExisting) {
        Restore-ExistingInstallation
        $back = $false
        foreach ($key in $UninstallKeys) { & reg.exe query $key 2>&1 | Out-Null; if ($LASTEXITCODE -eq 0) { $back = $true } }
        $group = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Entangled Desktop'
        Assert-Check -Name "the user's own installation is registered again" -Condition $back -Detail '' | Out-Null
        Assert-Check -Name "the user's Start Menu entries are back" -Condition (Test-Path $group) -Detail $group | Out-Null
        $real = (Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$AppId" -ErrorAction SilentlyContinue)
        if ($real) {
            Add-Check -Name 'restored registration points at the real installation' `
                -Status (($real.InstallLocation -notmatch [regex]::Escape($Root)) ? 'PASS' : 'FAIL') `
                -Detail "$($real.DisplayName) $($real.DisplayVersion) — $($real.InstallLocation)"
        }
    }
    # The logs are the evidence; keep them and drop everything else.
    foreach ($dir in @($AppDir, $ProfileDir, $VmDir, $CwdDir)) {
        Remove-Item -Recurse -Force -LiteralPath $dir -ErrorAction SilentlyContinue
    }
    Add-Check -Name 'scratch removed' -Status 'INFO' -Detail "kept: $script:LogDir, $DownloadDir"
}

# ---------------------------------------------------------------------------
# Verdict
# ---------------------------------------------------------------------------

Write-Stage 'result'
$script:Checks | Format-Table -AutoSize -Property Status, Name | Out-String -Width 200 | Write-Host

$failed = @($script:Checks | Where-Object { $_.Status -eq 'FAIL' })
$passed = @($script:Checks | Where-Object { $_.Status -eq 'PASS' })
Write-Host ("{0} passed, {1} failed, {2} skipped, {3} informational" -f
    $passed.Count, $failed.Count,
    @($script:Checks | Where-Object { $_.Status -eq 'SKIP' }).Count,
    @($script:Checks | Where-Object { $_.Status -eq 'INFO' }).Count)

if ($JsonReport) {
    $script:Checks | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $JsonReport -Encoding utf8
    Write-Host "report: $JsonReport"
}

if ($failed.Count -gt 0) {
    Write-Host ''
    Write-Host 'FAILED:' -ForegroundColor Red
    $failed | ForEach-Object { Write-Host "  $($_.Name)" -ForegroundColor Red }
    exit 1
}
Write-Host 'fresh-install acceptance passed' -ForegroundColor Green
exit 0

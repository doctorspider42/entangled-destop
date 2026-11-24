# SPDX-License-Identifier: Apache-2.0
# Stage a separate, unsigned WDDM package for the D3D12 lab guest.
param(
    [Parameter(Mandatory)][string]$WddmPackage,
    [Parameter(Mandatory)][string]$Umd,
    [Parameter(Mandatory)][string]$Out,
    [string]$ShaderCompiler
)

$ErrorActionPreference = 'Stop'
if (Test-Path -LiteralPath $Out) {
    throw 'The lab output directory must be new.'
}

$inf = Join-Path $WddmPackage 'entangled-wddm.inf'
$files = @('entangled-wddm.sys', 'entangled_d3d9.dll', 'entangled_d3d9_x86.dll',
           'entangled_d3d10.dll', 'entangled_d3d10_x86.dll')
foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath (Join-Path $WddmPackage $file) -PathType Leaf)) {
        throw "Missing WDDM package file: $file"
    }
}
if (-not (Test-Path -LiteralPath $Umd -PathType Leaf)) {
    throw "Missing x64 UMD: $Umd"
}

$text = Get-Content -LiteralPath $inf -Raw
$sections = @{SourceDisksFiles = 'entangled_d3d12.dll = 1'; 'EntangledWddm.UserMode' = 'entangled_d3d12.dll'}
if ($ShaderCompiler) {
    if (-not (Test-Path -LiteralPath $ShaderCompiler -PathType Leaf)) {
        throw "Missing x64 shader compiler: $ShaderCompiler"
    }
    $sections.SourceDisksFiles += "`r`ndxil-spirv-c-shared.dll = 1"
    $sections['EntangledWddm.UserMode'] += "`r`ndxil-spirv-c-shared.dll"
}
foreach ($section in $sections.Keys) {
    $header = '[' + $section + ']'
    $pattern = '(?m)^' + [regex]::Escape($header) + '\r?\n'
    if ([regex]::Matches($text, $pattern).Count -ne 1) {
        throw "Unexpected WDDM INF section: $section"
    }
    $text = [regex]::Replace($text, $pattern, {
        param($match)
        $match.Value + $sections[$section] + "`r`n"
    })
}

$slot = '(?m)^(HKR,,\s*UserModeDriverName,\s*0x00010000,\s*%13%\\entangled_d3d9\.dll,\s*%13%\\entangled_d3d10\.dll,\s*%13%\\entangled_d3d10\.dll,\s*)%13%\\entangled_d3d10\.dll(?=\r?$)'
if ([regex]::Matches($text, $slot).Count -ne 1) {
    throw 'The expected fourth x64 UserModeDriverName slot was not found.'
}
$text = [regex]::Replace($text, $slot, '${1}%13%\entangled_d3d12.dll')

# Preserve the source date and advance its driver version at the same PnP rank.
$version = '(?m)^(DriverVer\s*=\s*\d+/\d+/\d+,\s*\d+\.\d+\.\d+\.)(\d+)(?=\r?$)'
$match = [regex]::Match($text, $version)
if (-not $match.Success -or [int]$match.Groups[2].Value -ge 65535) {
    throw 'The WDDM INF does not have a revision that can be incremented.'
}
$text = [regex]::Replace($text, $version, {
    param($entry)
    $entry.Groups[1].Value + ([int]$entry.Groups[2].Value + 1)
})

New-Item -ItemType Directory -Path $Out | Out-Null
foreach ($file in $files) {
    Copy-Item -LiteralPath (Join-Path $WddmPackage $file) -Destination $Out
}
Copy-Item -LiteralPath $Umd -Destination (Join-Path $Out 'entangled_d3d12.dll')
if ($ShaderCompiler) {
    Copy-Item -LiteralPath $ShaderCompiler -Destination (Join-Path $Out 'dxil-spirv-c-shared.dll')
}
[IO.File]::WriteAllText((Join-Path (Resolve-Path -LiteralPath $Out).Path 'entangled-wddm.inf'),
                       ($text -replace '\r?\n', "`r`n"), [Text.Encoding]::ASCII)
Write-Output "Staged unsigned D3D12 lab WDDM package in $Out"

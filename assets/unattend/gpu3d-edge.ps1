# `entangled install windows --gpu3d` (ADR-0008): run at every logon, as
# SYSTEM. WebGL on the 3D driver draws only through ANGLE's Vulkan backend
# (its Direct3D layer is D3D10-only), Edge has no setting for that backend
# (edge://flags offers Default, D3D11 and WARP), and the command-line switch is
# the only way in. So the switch goes where Windows starts Edge from: the
# shortcuts every user sees and the command that opens a link. Edge's updates
# rewrite both now and then, which is why this runs at every logon.
$switch = '--use-angle=vulkan'
# With Entangled's own stack (ADR-0009) the only Direct3D adapter Windows
# lists is the Microsoft Basic Render Driver, which Chromium's GPU blocklist
# treats as software and turns WebGL off for, although ANGLE runs on the
# Vulkan device; the blocklist is ignored there.
if (Test-Path (Join-Path $PSScriptRoot 'vulkan_virtio.dll')) {
    $switch = '--use-angle=vulkan --ignore-gpu-blocklist'
}

$shell = New-Object -ComObject WScript.Shell
$links = @(
    "$env:PUBLIC\Desktop\Microsoft Edge.lnk",
    "$env:ProgramData\Microsoft\Windows\Start Menu\Programs\Microsoft Edge.lnk"
)
$links += Get-ChildItem -Path 'C:\Users\*\AppData\Roaming\Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar\*Edge*.lnk' -ErrorAction SilentlyContinue |
    ForEach-Object { $_.FullName }
$links += Get-ChildItem -Path 'C:\Users\*\Desktop\*Edge*.lnk' -ErrorAction SilentlyContinue |
    ForEach-Object { $_.FullName }
foreach ($path in $links | Select-Object -Unique) {
    if (-not (Test-Path $path)) { continue }
    $link = $shell.CreateShortcut($path)
    if ($link.TargetPath -like '*msedge.exe' -and $link.Arguments -notlike "*$switch*") {
        $link.Arguments = ("$switch " + $link.Arguments).Trim()
        $link.Save()
    }
}

# Links opened from other programs: the MSEdgeHTM and MSEdgePDF handlers.
foreach ($class in 'MSEdgeHTM', 'MSEdgePDF') {
    $key = "Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Classes\$class\shell\open\command"
    $command = (Get-ItemProperty -Path $key -Name '(default)' -ErrorAction SilentlyContinue).'(default)'
    if ($command -and $command -like '*msedge.exe*' -and $command -notlike "*$switch*") {
        $fixed = $command -replace '(msedge\.exe"?)', "`$1 $switch"
        Set-ItemProperty -Path $key -Name '(default)' -Value $fixed
    }
}

# None of that reaches an Edge that is already running. "Startup boost" starts
# Edge in the background at logon, and background mode keeps it alive after its
# last window closes - both without any switch - and a window opened later joins
# that process, whatever its shortcut says. Edge then draws with Windows'
# software Direct3D: measured on a full-screen 1080p60 video, 17.5 frames a
# second, every one rendered and copied by the CPU, against 60-75 through the
# overlay plane once Edge starts from a shortcut (ADR-0008, the vsync
# amendment's follow-up). Both are policies; Edge then says it is "managed by
# your organization", which is the price.
$policy = 'HKLM:\SOFTWARE\Policies\Microsoft\Edge'
New-Item -Path $policy -Force | Out-Null
foreach ($name in 'StartupBoostEnabled', 'BackgroundModeEnabled') {
    New-ItemProperty -Path $policy -Name $name -PropertyType DWord -Value 0 -Force | Out-Null
}

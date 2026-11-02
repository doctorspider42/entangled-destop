# `entangled install windows --gpu3d` (ADR-0008): run at every logon, as
# SYSTEM. WebGL on the 3D driver draws only through ANGLE's Vulkan backend
# (its Direct3D layer is D3D10-only), Edge has no setting for that backend
# (edge://flags offers Default, D3D11 and WARP), and the command-line switch is
# the only way in. So the switch goes where Windows starts Edge from: the
# shortcuts every user sees and the command that opens a link. Edge's updates
# rewrite both now and then, which is why this runs at every logon.
$switch = '--use-angle=vulkan'

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

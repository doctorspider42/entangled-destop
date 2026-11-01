# `entangled install windows --gpu3d` (ADR-0008): run at every startup and
# logon, as SYSTEM. viogpu3d's INF turns MSI off, and this machine has no _PRT
# for the line-based fallback, so a function left on INTx fails with code 12.
# MSI is a per-instance setting, and the function gets a new instance whenever
# its PCI slot moves (the installer's offline machine has no network card), so
# every instance is checked. Windows assigns interrupts once per boot, and
# restarting the device does not reassign them: with -Reboot, a change
# restarts the machine, which happens once per new instance. first-logon.cmd
# runs it without, because the installer's own power-off follows.
param([switch]$Reboot)
$changed = $false
$enum = 'HKLM:\SYSTEM\CurrentControlSet\Enum\PCI'
Get-ChildItem $enum |
    Where-Object { $_.PSChildName -like 'VEN_1AF4&DEV_1050&SUBSYS_11001AF4*' } |
    Get-ChildItem |
    ForEach-Object {
        $key = Join-Path $_.PSPath 'Device Parameters\Interrupt Management\MessageSignaledInterruptProperties'
        $now = (Get-ItemProperty -Path $key -Name MSISupported -ErrorAction SilentlyContinue).MSISupported
        if ($now -ne 1) {
            New-Item -Path $key -Force | Out-Null
            New-ItemProperty -Path $key -Name MSISupported -Value 1 -PropertyType DWord -Force | Out-Null
            $changed = $true
        }
    }
if ($changed -and $Reboot) {
    shutdown.exe /r /t 5 /c "Entangled: turning on 3D for the display adapter"
}

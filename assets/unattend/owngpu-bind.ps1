# `entangled install windows --gpu3d` with Entangled's own package (ADR-0009):
# run at every startup and logon, as SYSTEM. The render-only function gets a
# new device instance whenever its PCI slot moves (the installer's offline
# machine has no network card, and its answer discs sit on the bus), and on a
# new instance Windows picks the Microsoft-signed viogpudo, which matches any
# virtio-gpu and outranks our test-signed driver. So whenever a present
# render-only function runs anything else, gpu-test.exe installs ours on it
# again (UpdateDriverForPlugAndPlayDevices, forced). The swapped device only
# opens after a restart (until then CreateFile fails with access denied), so
# with -Reboot a change restarts the machine - once per new instance.
# first-logon.cmd runs it without, because the installer's power-off follows.
param([switch]$Reboot, [string]$Dir = (Split-Path -Parent $MyInvocation.MyCommand.Path))
$stray = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
    Where-Object { $_.InstanceId -like 'PCI\VEN_1AF4&DEV_1050&SUBSYS_00401AF4*' } |
    Where-Object {
        $inf = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_DriverInfPath).Data
        $name = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_DriverDesc).Data
        -not $inf -or $name -notlike 'Entangled*'
    }
if ($stray) {
    & (Join-Path $Dir 'gpu-test.exe') install (Join-Path $Dir 'owngpu\entangled-gpu.inf') | Out-Null
    if ($Reboot) {
        shutdown.exe /r /t 5 /c "Entangled: installing the 3D driver"
    }
}

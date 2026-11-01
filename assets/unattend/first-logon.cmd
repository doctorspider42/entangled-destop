@echo off
rem `entangled install windows` (ADR-0008): run once, at the first logon, from
rem the answer disc. %1 is that disc's drive, e.g. "E:".
rem
rem The desktop follows the window: viogpudo tells user mode the host asked
rem for another size, and its viogpuap.exe applies it (SetDisplayConfig).
rem virtio-win ships no service for it, so it is copied off this disc and
rem started at every logon (HKLM Run) in a hidden console: wscript runs
rem resolution.cmd with window style 0, and resolution.cmd starts viogpuap
rem again 10 s after any exit - right at logon the driver's event may not
rem exist yet, and viogpuap exits with ERROR_PATH_NOT_FOUND. A scheduled task
rem and a headless conhost were both tried, and neither stayed up at logon.
setlocal
set "disc=%~1"
set "dir=%ProgramFiles%\Entangled"
if not exist "%disc%\$WinPEDriver$\viogpudo\viogpuap.exe" exit /b 1
mkdir "%dir%" 2>nul
copy /y "%disc%\$WinPEDriver$\viogpudo\viogpuap.exe" "%dir%\" >nul
> "%dir%\resolution.cmd" (
  echo @echo off
  echo :again
  echo "%dir%\viogpuap.exe"
  echo ping -n 11 127.0.0.1 ^>nul
  echo goto again
)
> "%dir%\resolution.vbs" echo CreateObject("WScript.Shell").Run """%dir%\resolution.cmd""", 0, False
reg add "HKLM\Software\Microsoft\Windows\CurrentVersion\Run" /v "Entangled display resolution" /t REG_SZ /d "\"%SystemRoot%\System32\wscript.exe\" \"%dir%\resolution.vbs\"" /f >nul
rem The guest agent (the shared clipboard), when the installer had one to
rem put on this disc. It has no window and reconnects by itself. vioserial's
rem ports admit administrators only, so it starts at this user's logon with
rem the full token (a scheduled task, run level highest), not from Run.
if exist "%disc%\entangled\entangled-agent.exe" (
  copy /y "%disc%\entangled\entangled-agent.exe" "%dir%\" >nul
  powershell -NoProfile -Command "$a = New-ScheduledTaskAction -Execute '%dir%\entangled-agent.exe'; $t = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME; $p = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest; $s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit 0 -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries; Register-ScheduledTask -TaskName 'Entangled agent' -Action $a -Trigger $t -Principal $p -Settings $s -Force" >nul
)
rem 3D (ADR-0008), when the installer was given the viogpu3d package: its
rem test certificate, test-signing (the driver is test-signed), the Vulkan
rem runtime, the driver, and MSI for the function - the INF turns MSI off,
rem and this machine has no _PRT for the line-based fallback; gpu3d-msi.ps1
rem says why it runs at every startup. The installer's
rem final shutdown is the reboot all of it needs.
if exist "%disc%\entangled\gpu3d\VirtIOTestCert.cer" call :gpu3d
endlocal
exit /b 0

:gpu3d
set "g=%disc%\entangled\gpu3d"
certutil -addstore -f Root "%g%\VirtIOTestCert.cer" >nul
certutil -addstore -f TrustedPublisher "%g%\VirtIOTestCert.cer" >nul
bcdedit /set testsigning on >nul
"%g%\VulkanRT-Installer.exe" /S
pnputil /add-driver "%g%\Win10\amd64\viogpu3d.inf" /install >nul
copy /y "%disc%\entangled\gpu3d-msi.ps1" "%dir%\" >nul
schtasks /create /tn "Entangled 3D interrupts" /sc onstart /ru SYSTEM /rl HIGHEST /tr "powershell -NoProfile -ExecutionPolicy Bypass -File \"%dir%\gpu3d-msi.ps1\" -Reboot" /f >nul
rem And at logon: on the first boot after the install the function's new
rem instance is only enumerated after the startup run.
schtasks /create /tn "Entangled 3D interrupts at logon" /sc onlogon /ru SYSTEM /rl HIGHEST /tr "powershell -NoProfile -ExecutionPolicy Bypass -File \"%dir%\gpu3d-msi.ps1\" -Reboot" /f >nul
powershell -NoProfile -ExecutionPolicy Bypass -File "%dir%\gpu3d-msi.ps1"
exit /b 0

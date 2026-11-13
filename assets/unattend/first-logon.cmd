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
rem Not our disc: nothing to do. A virtio-win without viogpuap only costs the
rem resolution following; the agent, 3D and shared folders still install.
if not exist "%disc%\entangled\first-logon.cmd" exit /b 1
mkdir "%dir%" 2>nul
if exist "%disc%\$WinPEDriver$\viogpudo\viogpuap.exe" call :resolution
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
rem Or 3D through our own stack (ADR-0009): the certificate and test-signing
rem again, the Vulkan runtime, entangled-gpu.sys on the render-only function -
rem gpu-test.exe installs it over the Microsoft-signed display driver, which
rem outranks a test-signed one - and Mesa's Venus ICD, registered machine-wide
rem so elevated programs find it too (the loader ignores VK_ICD_FILENAMES
rem there). Its INF turns MSI on by itself.
if exist "%disc%\entangled\owngpu\entangled-gpu.inf" call :owngpu
rem Shared folders over virtio-fs, when the installer was given WinFsp: WinFsp,
rem then virtio-win's virtiofs service, which mounts the share as a drive.
if exist "%disc%\entangled\winfsp.msi" call :virtiofs
endlocal
exit /b 0

:resolution
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
exit /b 0

:virtiofs
msiexec /i "%disc%\entangled\winfsp.msi" /qn /norestart
copy /y "%disc%\$WinPEDriver$\viofs\virtiofs.exe" "%dir%\" >nul
sc create VirtioFsSvc binPath= "\"%dir%\virtiofs.exe\"" start= auto depend= WinFsp.Launcher DisplayName= "Virtio FS Service" >nul
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
call :edge
exit /b 0

:owngpu
set "g=%disc%\entangled\owngpu"
rem A test-signed driver brings its certificate and needs test-signing mode;
rem a Microsoft-signed one (attestation, ADR-0009 phase F) brings neither.
if exist "%g%\entangled-gpu.cer" (
  certutil -addstore -f Root "%g%\entangled-gpu.cer" >nul
  certutil -addstore -f TrustedPublisher "%g%\entangled-gpu.cer" >nul
  bcdedit /set testsigning on >nul
)
"%g%\VulkanRT-Installer.exe" /S
mkdir "%dir%\owngpu" 2>nul
copy /y "%g%\entangled-gpu.*" "%dir%\owngpu\" >nul
rem Our display driver, when the package has it (ADR-0008, the vsync
rem amendment): viogpudo with a refresh rate and a vsync, so the desktop
rem composes at the display's rate. owngpu-bind.ps1 puts it on the display.
if exist "%g%\entangled-display.inf" copy /y "%g%\entangled-display.*" "%dir%\owngpu\" >nul
rem Opt-in (ADR-0010): the WDDM driver that has DWM compose on the host GPU.
rem Only copied here; wddm-on.cmd installs it, wddm-off.cmd goes back.
if exist "%g%\entangled-wddm.inf" (
  mkdir "%dir%\wddm" 2>nul
  copy /y "%g%\entangled-wddm.*" "%dir%\wddm\" >nul
  copy /y "%g%\entangled_d3d10*.dll" "%dir%\wddm\" >nul
  if exist "%g%\entangled_d3d9.dll" copy /y "%g%\entangled_d3d9*.dll" "%dir%\wddm\" >nul
  copy /y "%disc%\entangled\wddm-on.cmd" "%dir%\" >nul
  copy /y "%disc%\entangled\wddm-off.cmd" "%dir%\" >nul
)
copy /y "%g%\gpu-test.exe" "%dir%\" >nul
copy /y "%disc%\entangled\owngpu-bind.ps1" "%dir%\" >nul
rem owngpu-bind.ps1 says why it runs at every startup and logon.
schtasks /create /tn "Entangled 3D driver" /sc onstart /ru SYSTEM /rl HIGHEST /tr "powershell -NoProfile -ExecutionPolicy Bypass -File \"%dir%\owngpu-bind.ps1\" -Reboot" /f >nul
schtasks /create /tn "Entangled 3D driver at logon" /sc onlogon /ru SYSTEM /rl HIGHEST /tr "powershell -NoProfile -ExecutionPolicy Bypass -File \"%dir%\owngpu-bind.ps1\" -Reboot" /f >nul
powershell -NoProfile -ExecutionPolicy Bypass -File "%dir%\owngpu-bind.ps1"
copy /y "%g%\vulkan_virtio.dll" "%dir%\" >nul
rem OpenGL (Zink) and Direct3D 9-11 (DXVK) over that Vulkan, when the package
rem has them: DLLs a program takes from beside its .exe (copy them there).
if exist "%g%\opengl\opengl32.dll" xcopy /y /i /q "%g%\opengl" "%dir%\opengl" >nul
if exist "%g%\dxvk\d3d11.dll" xcopy /y /i /q /e "%g%\dxvk" "%dir%\dxvk" >nul
powershell -NoProfile -Command "$j = Join-Path '%dir%' 'entangled_icd.json'; @{file_format_version='1.0.0'; ICD=@{library_path=(Join-Path '%dir%' 'vulkan_virtio.dll'); api_version='1.3.0'}} | ConvertTo-Json | Set-Content -Encoding ASCII $j; $k = 'HKLM:\SOFTWARE\Khronos\Vulkan\Drivers'; New-Item -Force $k | Out-Null; New-ItemProperty -Force -Path $k -Name $j -PropertyType DWord -Value 0 | Out-Null"
rem The same driver for 32-bit programs, when the package has it: their
rem Vulkan loader reads the WOW6432Node list. Without it a 32-bit game has no
rem graphics driver here at all, and leaves without a word.
if exist "%g%\x86\vulkan_virtio.dll" (
  xcopy /y /i /q "%g%\x86" "%dir%\x86" >nul
  powershell -NoProfile -Command "$j = Join-Path '%dir%\x86' 'entangled_icd.json'; @{file_format_version='1.0.0'; ICD=@{library_path=(Join-Path '%dir%\x86' 'vulkan_virtio.dll'); api_version='1.3.0'}} | ConvertTo-Json | Set-Content -Encoding ASCII $j; $k = 'HKLM:\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers'; New-Item -Force $k | Out-Null; New-ItemProperty -Force -Path $k -Name $j -PropertyType DWord -Value 0 | Out-Null"
)
call :edge
exit /b 0

:edge
rem WebGL through ANGLE's Vulkan backend: gpu3d-edge.ps1 says why and where.
copy /y "%disc%\entangled\gpu3d-edge.ps1" "%dir%\" >nul
schtasks /create /tn "Entangled 3D browser" /sc onlogon /ru SYSTEM /rl HIGHEST /tr "powershell -NoProfile -ExecutionPolicy Bypass -File \"%dir%\gpu3d-edge.ps1\"" /f >nul
powershell -NoProfile -ExecutionPolicy Bypass -File "%dir%\gpu3d-edge.ps1"
exit /b 0

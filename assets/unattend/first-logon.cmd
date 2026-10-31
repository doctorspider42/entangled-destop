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
endlocal

@echo off
rem Builds entangled-display.sys, our variant of virtio-win's display-only
rem driver viogpudo (ADR-0008, the vsync amendment), without the WDK's Visual
rem Studio integration: cl and link from the MSVC build tools, headers and
rem libraries from the WDK. The source is upstream's at a pinned commit with
rem our patch applied, made by prepare.ps1.
rem Usage: build.cmd <prepared source directory> <output directory>
rem        (then ..\driver\sign.ps1 on the output directory)
rem The defaults are the development host's; CI sets WDK (a kit root laid out
rem like the installed one) and MSVC (the toolset directory).
setlocal
if "%~2"=="" (echo usage: build.cmd ^<prepared source directory^> ^<output directory^> & exit /b 2)
set "TREE=%~1"
set "OUT=%~2"
set "SRC=%~dp0"
if not defined WDK set "WDK=C:\Program Files (x86)\Windows Kits\10"
set "V=10.0.26100.0"
if not defined MSVC for /d %%d in ("C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\*") do set "MSVC=%%d"
set "BIN=%MSVC%\bin\Hostx64\x64"
if not exist "%BIN%\cl.exe" goto :nomsvc
if not exist "%TREE%\viogpu\viogpudo\viogpudo.cpp" goto :notree
set "OBJ=%OUT%\display-obj"
mkdir "%OUT%" 2>nul
mkdir "%OBJ%" 2>nul

rem Upstream includes one WPP trace header per source file. We build without
rem WPP (ENTANGLED_NO_WPP, added by our patch), so they are empty.
for %%n in (bitops viogpu_idr viogpu_pci viogpu_queue driver viogpudo) do type nul > "%OBJ%\%%n.tmh"

set "INC=/I"%OBJ%" /I"%TREE%\viogpu\common" /I"%TREE%\viogpu\shared" /I"%TREE%\VirtIO" /I"%TREE%\viogpu\viogpudo" /I"%WDK%\Include\%V%\km" /I"%WDK%\Include\%V%\km\crt" /I"%WDK%\Include\%V%\shared" /I"%WDK%\Include\%V%\um""
set "DEF=/D_AMD64_ /DAMD64 /D_WIN64 /DWINNT=1 /DNTDDI_VERSION=0x0A000000 /D_WIN32_WINNT=0x0A00 /DWINVER=0x0A00 /DPOOL_NX_OPTIN=1 /DVIOGPU_DOD=1 /DENTANGLED_NO_WPP=1"
set "CFLAGS=/nologo /c /kernel /W3 /O2 /Oi /GS /Gy /Gz /Zi /Zp8 /Zc:wchar_t- /GR- /FS %DEF% %INC% /Fd"%OBJ%\entangled-display.pdb""

set "OBJS="
for %%f in (VirtIOPCICommon VirtIOPCILegacy VirtIOPCIModern VirtIORing VirtIORing-Packed) do (
  "%BIN%\cl.exe" %CFLAGS% /TC /Fo"%OBJ%\%%f.obj" "%TREE%\VirtIO\%%f.c"
  if errorlevel 1 exit /b 1
)
for %%f in (edid viogpu_idr viogpu_pci viogpu_queue baseobj bitops) do (
  "%BIN%\cl.exe" %CFLAGS% /TP /Fo"%OBJ%\%%f.obj" "%TREE%\viogpu\common\%%f.cpp"
  if errorlevel 1 exit /b 1
)
for %%f in (driver viogpudo) do (
  "%BIN%\cl.exe" %CFLAGS% /TP /Fo"%OBJ%\%%f.obj" "%TREE%\viogpu\viogpudo\%%f.cpp"
  if errorlevel 1 exit /b 1
)

set "KLIB=%WDK%\Lib\%V%\km\x64"
"%BIN%\link.exe" /nologo /DRIVER /SUBSYSTEM:NATIVE,10.00 /ENTRY:GsDriverEntry /NODEFAULTLIB ^
  /MACHINE:X64 /RELEASE /DEBUG /INCREMENTAL:NO /MERGE:_TEXT=.text /OPT:REF /OPT:ICF ^
  /OUT:"%OUT%\entangled-display.sys" /PDB:"%OBJ%\entangled-display-link.pdb" "%OBJ%\*.obj" ^
  "%KLIB%\ntoskrnl.lib" "%KLIB%\hal.lib" "%KLIB%\wmilib.lib" "%KLIB%\displib.lib" ^
  "%KLIB%\BufferOverflowFastFailK.lib"
if errorlevel 1 exit /b 1
copy /y "%SRC%entangled-display.inf" "%OUT%\" >nul
copy /y "%TREE%\viogpu\LICENSE" "%OUT%\LICENSE-viogpudo.txt" >nul
rem A new DriverVer every build: with the same one, Windows keeps the copy
rem already in its driver store and the new binary never runs.
"%WDK%\bin\%V%\x64\stampinf.exe" -f "%OUT%\entangled-display.inf" -d * -v * -a amd64 >nul
if errorlevel 1 exit /b 1
echo built %OUT%\entangled-display.sys
endlocal
exit /b 0

:nomsvc
echo no MSVC build tools found
exit /b 1
:notree
echo %TREE% is not a prepared source tree: run prepare.ps1 first
exit /b 1

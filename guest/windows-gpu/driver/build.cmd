@echo off
rem Builds entangled-gpu.sys without the WDK's Visual Studio integration:
rem cl and link from the MSVC build tools, headers and libraries from the WDK.
rem Usage: build.cmd <output directory>   (then sign.ps1 in the same directory)
rem The defaults are the development host's; CI sets WDK (a kit root laid out
rem like the installed one), MSVC (the toolset directory) and MESA_INCLUDE.
setlocal
if "%~1"=="" (echo usage: build.cmd ^<output directory^> & exit /b 2)
set "OUT=%~1"
set "SRC=%~dp0"
if not defined WDK set "WDK=C:\Program Files (x86)\Windows Kits\10"
set "V=10.0.26100.0"
set "KMDF=1.15"
if not defined MSVC for /d %%d in ("C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\*") do set "MSVC=%%d"
if not defined MESA_INCLUDE set "MESA_INCLUDE=F:\entangled-targets\mesa\include"
set "BIN=%MSVC%\bin\Hostx64\x64"
if not exist "%BIN%\cl.exe" goto :nomsvc
mkdir "%OUT%" 2>nul

set "INC=/I"%WDK%\Include\%V%\km" /I"%WDK%\Include\%V%\km\crt" /I"%WDK%\Include\%V%\shared" /I"%WDK%\Include\wdf\kmdf\%KMDF%""
"%BIN%\cl.exe" /nologo /c /kernel /W4 /WX /wd4324 /O2 /GS- /Gy /Zi /Zc:wchar_t- ^
  /D_AMD64_ /DAMD64 /D_WIN64 /DWINNT=1 /DNTDDI_VERSION=0x0A000000 /D_WIN32_WINNT=0x0A00 ^
  /DKMDF_VERSION_MAJOR=1 /DKMDF_VERSION_MINOR=15 /DPOOL_NX_OPTIN=1 ^
  %INC% /Fo"%OUT%\entangled-gpu.obj" /Fd"%OUT%\entangled-gpu.pdb" "%SRC%entangled-gpu.c"
if errorlevel 1 exit /b 1

set "KLIB=%WDK%\Lib\%V%\km\x64"
set "WLIB=%WDK%\Lib\wdf\kmdf\x64\%KMDF%"
"%BIN%\link.exe" /nologo /DRIVER /SUBSYSTEM:NATIVE,10.00 /ENTRY:FxDriverEntry /NODEFAULTLIB ^
  /MACHINE:X64 /RELEASE /DEBUG /INCREMENTAL:NO /MERGE:_TEXT=.text /OPT:REF /OPT:ICF ^
  /OUT:"%OUT%\entangled-gpu.sys" /PDB:"%OUT%\entangled-gpu-link.pdb" "%OUT%\entangled-gpu.obj" ^
  "%KLIB%\ntoskrnl.lib" "%KLIB%\hal.lib" "%KLIB%\BufferOverflowFastFailK.lib" ^
  "%WLIB%\WdfDriverEntry.lib" "%WLIB%\WdfLdr.lib"
if errorlevel 1 exit /b 1
rem The in-guest test tool (user mode).
set "UINC=/I"%WDK%\Include\%V%\um" /I"%WDK%\Include\%V%\shared" /I"%WDK%\Include\%V%\ucrt" /I"%MSVC%\include""
set "ULIB=/LIBPATH:"%WDK%\Lib\%V%\um\x64" /LIBPATH:"%WDK%\Lib\%V%\ucrt\x64" /LIBPATH:"%MSVC%\lib\x64""
"%BIN%\cl.exe" /nologo /W4 /WX /O2 /MT %UINC% /Fo"%OUT%\gpu-test.obj" /Fe"%OUT%\gpu-test.exe" "%SRC%gpu-test.c" /link %ULIB%
if errorlevel 1 exit /b 1
rem The presentation test (phase C), against Mesa's Vulkan headers.
"%BIN%\cl.exe" /nologo /W3 /O2 /MT %UINC% /I"%MESA_INCLUDE%" /Fo"%OUT%\vk-present.obj" /Fe"%OUT%\vk-present.exe" "%SRC%vk-present.c" /link %ULIB% user32.lib
if errorlevel 1 exit /b 1
rem The OpenGL test (phase D), for Mesa's opengl32.dll with Zink.
"%BIN%\cl.exe" /nologo /W3 /O2 /MT %UINC% /Fo"%OUT%\gl-test.obj" /Fe"%OUT%\gl-test.exe" "%SRC%gl-test.c" /link %ULIB% user32.lib gdi32.lib opengl32.lib
if errorlevel 1 exit /b 1
copy /y "%SRC%entangled-gpu.inf" "%OUT%\" >nul
rem A new DriverVer every build: with the same one, Windows keeps the copy
rem already in its driver store and the new binary never runs.
"%WDK%\bin\%V%\x64\stampinf.exe" -f "%OUT%\entangled-gpu.inf" -d * -v * -a amd64 -k 1.15 >nul
if errorlevel 1 exit /b 1
echo built %OUT%\entangled-gpu.sys
endlocal
exit /b 0

:nomsvc
echo no MSVC build tools found
exit /b 1

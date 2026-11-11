@echo off
rem Builds entangled_d3d9.dll, the Direct3D 9 user-mode driver of
rem entangled-wddm.sys (ADR-0011), and d3d9-test.exe, for x64 and x86:
rem
rem   build.cmd <out>   ->  <out>\entangled_d3d9.dll   <out>\d3d9-test.exe
rem                         <out>\x86\entangled_d3d9.dll   <out>\x86\d3d9-test.exe
rem
rem Phase N0: a driver the runtime takes and that draws nothing. The DDI
rem headers are the WDK's (as build-d3d10.cmd says).
setlocal
if "%~1"=="" (echo usage: build.cmd ^<out^> & exit /b 2)
set "OUT=%~f1"
if not defined VCVARS set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat"
if not defined LLVM_BIN set "LLVM_BIN=C:\Users\pawel\scoop\apps\llvm\current\bin"
if not defined WDK_UM set "WDK_UM=C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\um"
set "HERE=%~dp0"
set "PATH=%LLVM_BIN%;%PATH%"
if not exist "%OUT%\x86" mkdir "%OUT%\x86"

call :one x64 "%OUT%" ""
if errorlevel 1 exit /b 1
call :one x64_x86 "%OUT%\x86" -m32
if errorlevel 1 exit /b 1
echo built %OUT%\entangled_d3d9.dll and %OUT%\x86\entangled_d3d9.dll
exit /b 0

:one
setlocal
call "%VCVARS%" %1 >nul
set "INCLUDE=%INCLUDE%;%WDK_UM%;%WDK_UM%\..\shared"
rem The device table as this compiler sees it, for the placeholders.
clang-cl %~3 /nologo /EP /Tc"%HERE%ddi.h" > "%~2\ddi.i" 2>nul
python "%HERE%gen-stubs.py" "%~2\ddi.i" "%~2\entangled_d3d9_stubs.inc"
if errorlevel 1 exit /b 1
clang-cl %~3 /nologo /W3 /O2 /MT /LD /I"%~2" "%HERE%entangled_d3d9.c" ^
  /Fo"%~2\entangled_d3d9.obj" /Fe"%~2\entangled_d3d9.dll" ^
  /link /DEF:"%HERE%entangled_d3d9.def" /MAP gdi32.lib user32.lib
if errorlevel 1 exit /b 1
clang-cl %~3 /nologo /W3 /O2 /MT "%HERE%d3d9-test.c" /Fo"%~2\d3d9-test.obj" /Fe"%~2\d3d9-test.exe" ^
  /link d3d9.lib user32.lib
if errorlevel 1 exit /b 1
endlocal
exit /b 0

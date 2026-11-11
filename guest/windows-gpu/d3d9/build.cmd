@echo off
rem Builds d3d9-test.exe, the Direct3D 9 test program (ADR-0011), for x64 and
rem x86:
rem
rem   build.cmd <out>   ->  <out>\d3d9-test.exe   <out>\x86\d3d9-test.exe
rem
rem The driver itself, entangled_d3d9.dll, is not built here: it is Nine and
rem Zink with this directory's DDI front, and Mesa builds it
rem (..\mesa\build-d3d9.cmd, from the tree ..\mesa\prepare-d3d10.ps1 makes).
setlocal
if "%~1"=="" (echo usage: build.cmd ^<out^> & exit /b 2)
set "OUT=%~f1"
if not defined VCVARS set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat"
if not defined LLVM_BIN set "LLVM_BIN=C:\Users\pawel\scoop\apps\llvm\current\bin"
set "HERE=%~dp0"
set "PATH=%LLVM_BIN%;%PATH%"
if not exist "%OUT%\x86" mkdir "%OUT%\x86"

call :one x64 "%OUT%" ""
if errorlevel 1 exit /b 1
call :one x64_x86 "%OUT%\x86" -m32
if errorlevel 1 exit /b 1
echo built %OUT%\d3d9-test.exe and %OUT%\x86\d3d9-test.exe
exit /b 0

:one
setlocal
call "%VCVARS%" %1 >nul
clang-cl %~3 /nologo /W3 /O2 /MT "%HERE%d3d9-test.c" /Fo"%~2\d3d9-test.obj" /Fe"%~2\d3d9-test.exe" ^
  /link d3d9.lib user32.lib
if errorlevel 1 exit /b 1
endlocal
exit /b 0

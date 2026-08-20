@echo off
setlocal EnableExtensions
rem Build Entangled Desktop natively on Windows, and optionally start it.
rem
rem   build.cmd              build the release binaries
rem   build.cmd -run         build, then open the manager window
rem   build.cmd -run -vm foo build, then run the VM profile "foo" directly
rem   build.cmd -debug -run  same, but the debug profile (keeps a console)
rem
rem The target directory deliberately lives under %LOCALAPPDATA%, never in the
rem repository: D: is small and a cargo target dir grows to tens of gigabytes.

set "PROFILE=release"
set "PROFILE_DIR=release"
set "DO_RUN="
set "VM="

:parse
if "%~1"=="" goto parsed
if /I "%~1"=="-run"    ( set "DO_RUN=1" & shift & goto parse )
if /I "%~1"=="-debug"  ( set "PROFILE=dev" & set "PROFILE_DIR=debug" & shift & goto parse )
if /I "%~1"=="-vm"     ( set "VM=%~2" & set "DO_RUN=1" & shift & shift & goto parse )
if /I "%~1"=="-help"   ( goto usage )
if /I "%~1"=="/?"      ( goto usage )
echo unknown option: %~1
echo.
goto usage

:parsed
if not defined CARGO_TARGET_DIR set "CARGO_TARGET_DIR=%LOCALAPPDATA%\entangled-target"
cd /d "%~dp0"

where cargo >nul 2>&1
if errorlevel 1 (
    echo error: cargo is not on PATH. Install Rust from https://rustup.rs and reopen this window.
    exit /b 1
)

echo Building %PROFILE% into %CARGO_TARGET_DIR% ...
cargo build --profile %PROFILE% -p entangled -p entangled-manager
if errorlevel 1 (
    echo.
    echo BUILD FAILED - nothing was started.
    exit /b 1
)

set "BIN=%CARGO_TARGET_DIR%\%PROFILE_DIR%"
echo.
for /f "delims=" %%v in ('"%BIN%\entangled-manager.exe" --version') do echo Built %%v
echo   %BIN%\entangled-manager.exe
echo   %BIN%\entangled.exe

if not defined DO_RUN (
    echo.
    echo Add -run to start the manager after building.
    exit /b 0
)

if defined VM (
    echo.
    echo Running VM profile "%VM%" ...
    "%BIN%\entangled.exe" run "%USERPROFILE%\entangled-vms\%VM%.toml"
    exit /b %errorlevel%
)

echo.
echo Starting the manager ...
rem `start` returns immediately: the release manager is a GUI-subsystem binary,
rem so it owns its window and this console is free again. The redirection stops
rem the child from holding this console's pipes open, which would otherwise make
rem `build.cmd -run` look like it hangs when someone pipes its output.
start "" "%BIN%\entangled-manager.exe" >nul 2>&1
exit /b 0

:usage
echo Usage: build.cmd [-run] [-vm NAME] [-debug]
echo.
echo   -run      start the manager once the build succeeds
echo   -vm NAME  run %%USERPROFILE%%\entangled-vms\NAME.toml instead of the manager
echo   -debug    build the debug profile (slower guests, keeps a console window)
exit /b 2

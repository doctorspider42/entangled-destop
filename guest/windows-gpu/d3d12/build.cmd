@echo off
rem Build the experimental Windows x64 UMD and its tests. Output is standalone.
setlocal
if "%~1"=="" (echo usage: build.cmd ^<output-directory^> & exit /b 2)
if not defined VCVARS set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat"
if not defined SDK_VERSION set "SDK_VERSION=10.0.26100.0"
set "OUT=%~f1"
set "SRC=%~dp0"
call "%VCVARS%" x64 %SDK_VERSION% >nul
if errorlevel 1 exit /b 1
if not exist "%OUT%" mkdir "%OUT%"
if not exist "%OUT%" exit /b 1

cl /nologo /std:c11 /W4 /WX /O2 /MT /LD /I"%SRC%..\..\..\tools\venus-protocol\include" ^
    "%SRC%entangled_d3d12.c" "%SRC%venus-adapter.c" ^
    /Fo"%OUT%\\" /Fe"%OUT%\entangled_d3d12.dll" /link /IMPLIB:"%OUT%\entangled_d3d12.lib"
if errorlevel 1 exit /b 1
cl /nologo /std:c11 /W4 /WX /O2 /MT "%SRC%ddi-test.c" /Fo"%OUT%\ddi-test.obj" /Fe"%OUT%\ddi-test.exe"
if errorlevel 1 exit /b 1
cl /nologo /std:c++17 /W4 /WX /O2 /MT /EHsc "%SRC%d3d12-smoke.cpp" "%SRC%wddm-fence.cpp" ^
    /Fo"%OUT%\\" /Fe"%OUT%\d3d12-smoke.exe" /link /DEPENDENTLOADFLAG:0x800 d3d12.lib dxgi.lib gdi32.lib
exit /b %errorlevel%

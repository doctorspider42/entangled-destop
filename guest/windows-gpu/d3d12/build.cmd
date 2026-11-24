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

rem Advertise the provisional revision only in disposable runtime contract probes.
set "DDI_PROBE="
set "SHADER_PROBE="
set "PRESENT_PROBE="
if "%ENTANGLED_D3D12_PROBE_DDI%"=="1" set "DDI_PROBE=/DENTANGLED_D3D12_PROBE_DDI=1"
if "%ENTANGLED_D3D12_PROBE_DDI%%ENTANGLED_D3D12_PROBE_COPY%"=="11" set "DDI_PROBE=%DDI_PROBE% /DENTANGLED_D3D12_PROBE_COPY=1"
if "%ENTANGLED_D3D12_PROBE_DDI%%ENTANGLED_D3D12_PROBE_COPY%"=="11" if defined ENTANGLED_D3D12_SHADER_INCLUDE set SHADER_PROBE=/DENTANGLED_D3D12_PROBE_SHADER=1 /I"%ENTANGLED_D3D12_SHADER_INCLUDE%"
if "%ENTANGLED_D3D12_PROBE_RUNTIME%"=="1" if defined SHADER_PROBE set "DDI_PROBE=%DDI_PROBE% /DENTANGLED_D3D12_PROBE_RUNTIME=1"
if "%ENTANGLED_D3D12_PROBE_RUNTIME%%ENTANGLED_D3D12_PROBE_WINTANYL%"=="11" if defined SHADER_PROBE set "DDI_PROBE=%DDI_PROBE% /DENTANGLED_D3D12_PROBE_WINTANYL=1"
if "%ENTANGLED_D3D12_PROBE_RUNTIME%"=="1" if defined SHADER_PROBE if defined ENTANGLED_D3D12_WDDM_INCLUDE set PRESENT_PROBE=/DENTANGLED_D3D12_PROBE_PRESENT=1 /I"%ENTANGLED_D3D12_WDDM_INCLUDE%"
cl /nologo /std:c11 /W4 /WX /O2 /MT /LD %DDI_PROBE% %SHADER_PROBE% %PRESENT_PROBE% /I"%SRC%..\..\..\tools\venus-protocol\include" ^
    "%SRC%entangled_d3d12.c" "%SRC%venus-adapter.c" "%SRC%venus-transfer.c" ^
    /Fo"%OUT%\\" /Fe"%OUT%\entangled_d3d12.dll" /link /IMPLIB:"%OUT%\entangled_d3d12.lib"
if errorlevel 1 exit /b 1
cl /nologo /std:c11 /W4 /WX /O2 /MT %DDI_PROBE% "%SRC%ddi-test.c" /Fo"%OUT%\ddi-test.obj" /Fe"%OUT%\ddi-test.exe"
if errorlevel 1 exit /b 1
cl /nologo /std:c++17 /W4 /WX /O2 /MT /EHsc "%SRC%d3d12-smoke.cpp" "%SRC%wddm-fence.cpp" ^
    /Fo"%OUT%\\" /Fe"%OUT%\d3d12-smoke.exe" /link /DEPENDENTLOADFLAG:0x800 d3d12.lib d3dcompiler.lib dxgi.lib gdi32.lib user32.lib dwmapi.lib dcomp.lib
exit /b %errorlevel%

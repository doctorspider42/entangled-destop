@echo off
rem Builds entangled_d3d10.dll, the Direct3D 10 user-mode driver of
rem entangled-wddm.sys (ADR-0010): Mesa 26.2.6's d3d10umd frontend with the
rem WDDM integration of Mesa MR 24223, over Zink (softpipe where a process
rem has no Vulkan), sharing textures through the kernel driver's allocations.
rem The tree is the one prepare-d3d10.ps1 makes.
rem Same variables as build.cmd (MESA_BUILD defaults to mesa-d3d10 here).
setlocal
if not defined VCVARS set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat"
if not defined LLVM_BIN set "LLVM_BIN=C:\Users\pawel\scoop\apps\llvm\current\bin"
if not defined WINFLEXBISON set "WINFLEXBISON=C:\Users\pawel\scoop\apps\winflexbison\current"
if not defined MESA_SRC set "MESA_SRC=F:\entangled-targets\wddm\mesa"
if not defined MESA_BUILD set "MESA_BUILD=F:\entangled-targets\wddm\mesa-d3d10"
rem The Direct3D DDI headers (d3d10umddi.h) are the WDK's; the SDK vcvars
rem picks may be a newer one that has none.
if not defined WDK_UM set "WDK_UM=C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\um"
call "%VCVARS%" x64 >nul
set "PATH=%LLVM_BIN%;%WINFLEXBISON%;%PATH%"
set "INCLUDE=%INCLUDE%;%WDK_UM%;%WDK_UM%\..\shared"
set CC=clang-cl
set CXX=clang-cl
cd /d "%MESA_SRC%"
if not exist "%MESA_BUILD%\build.ninja" (
  python -m mesonbuild.mesonmain setup "%MESA_BUILD%" -Dbuildtype=release ^
    -Dplatforms=windows -Dvulkan-drivers= -Dgallium-drivers=zink,softpipe -Dopengl=true -Dgles1=disabled ^
    -Dgles2=disabled -Degl=disabled -Dglx=disabled -Dllvm=disabled -Dshared-glapi=enabled ^
    -Dgallium-d3d10umd=true -Dgallium-d3d10-dll-name=entangled_d3d10 -Dc_link_args=/MAP -Dcpp_link_args=/MAP ^
    -Dzstd=disabled -Dexpat=disabled -Dvalgrind=disabled -Dlibunwind=disabled ^
    -Dmicrosoft-clc=disabled -Dspirv-to-dxil=false -Dbuild-tests=false -Db_vscrt=mt -Dzlib:default_library=static
  if errorlevel 1 exit /b 1
)
ninja -C "%MESA_BUILD%" src/gallium/targets/d3d10umd/entangled_d3d10.dll 2>&1
if errorlevel 1 exit /b 1
endlocal

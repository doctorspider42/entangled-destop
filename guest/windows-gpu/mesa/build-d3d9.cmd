@echo off
rem Builds entangled_d3d9.dll, the Direct3D 9 user-mode driver of
rem entangled-wddm.sys (ADR-0011), for 64-bit and for 32-bit programs: Nine
rem (Mesa 25.1.9's, carried forward by 0003) behind our DDI front
rem (..\d3d9), over Zink. The tree is the one prepare-d3d10.ps1 makes.
rem
rem   %MESA_BUILD%\src\gallium\targets\d3d9umd\entangled_d3d9.dll      x64
rem   %MESA_BUILD32%\src\gallium\targets\d3d9umd\entangled_d3d9.dll    x86
rem
rem The x64 one shares build-d3d10.cmd's build directory (the same Gallium
rem and Zink objects); the x86 one is a build of its own, since most Direct3D
rem 9 games are 32-bit. Same variables as build-d3d10.cmd.
setlocal
if not defined VCVARS set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat"
if not defined LLVM_BIN set "LLVM_BIN=C:\Users\pawel\scoop\apps\llvm\current\bin"
if not defined WINFLEXBISON set "WINFLEXBISON=C:\Users\pawel\scoop\apps\winflexbison\current"
if not defined MESA_SRC set "MESA_SRC=F:\entangled-targets\wddm\mesa"
if not defined MESA_BUILD set "MESA_BUILD=F:\entangled-targets\wddm\mesa-d3d10"
if not defined MESA_BUILD32 set "MESA_BUILD32=F:\entangled-targets\wddm\mesa-d3d-x86"
if not defined WDK_UM set "WDK_UM=C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\um"
set "HERE=%~dp0"
set "PATH=%LLVM_BIN%;%WINFLEXBISON%;%PATH%"
set "OPTIONS=-Dbuildtype=release -Dplatforms=windows -Dvulkan-drivers= -Dgallium-drivers=zink,softpipe -Dopengl=true -Dgles1=disabled -Dgles2=disabled -Degl=disabled -Dglx=disabled -Dllvm=disabled -Dshared-glapi=enabled -Dgallium-d3d10umd=true -Dgallium-d3d10-dll-name=entangled_d3d10 -Dgallium-d3d9umd=true -Dc_link_args=/MAP -Dcpp_link_args=/MAP -Dzstd=disabled -Dexpat=disabled -Dvalgrind=disabled -Dlibunwind=disabled -Dmicrosoft-clc=disabled -Dspirv-to-dxil=false -Dbuild-tests=false -Db_vscrt=mt -Dzlib:default_library=static"

call :x64
if errorlevel 1 exit /b 1
call :x86
if errorlevel 1 exit /b 1
exit /b 0

:x64
setlocal
call "%VCVARS%" x64 >nul
set "INCLUDE=%INCLUDE%;%WDK_UM%;%WDK_UM%\..\shared"
set CC=clang-cl
set CXX=clang-cl
cd /d "%MESA_SRC%"
if not exist "%MESA_BUILD%\build.ninja" (
  python -m mesonbuild.mesonmain setup "%MESA_BUILD%" %OPTIONS%
  if errorlevel 1 exit /b 1
)
rem A directory build-d3d10.cmd made before this driver existed has it off.
python -m mesonbuild.mesonmain configure "%MESA_BUILD%" -Dgallium-d3d9umd=true >nul
ninja -C "%MESA_BUILD%" src/gallium/targets/d3d9umd/entangled_d3d9.dll 2>&1
if errorlevel 1 exit /b 1
endlocal
exit /b 0

:x86
setlocal
call "%VCVARS%" x64_x86 >nul
set "INCLUDE=%INCLUDE%;%WDK_UM%;%WDK_UM%\..\shared"
rem As build-x86.cmd says: -m32 through CL, the target through the cross file.
set CL=-m32
cd /d "%MESA_SRC%"
if not exist "%MESA_BUILD32%\build.ninja" (
  python -m mesonbuild.mesonmain setup "%MESA_BUILD32%" --cross-file "%HERE%x86-windows.ini" %OPTIONS%
  if errorlevel 1 exit /b 1
)
ninja -C "%MESA_BUILD32%" src/gallium/targets/d3d9umd/entangled_d3d9.dll 2>&1
if errorlevel 1 exit /b 1
endlocal
exit /b 0

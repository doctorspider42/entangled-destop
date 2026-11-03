@echo off
rem Builds Mesa 26.2.6's Venus Vulkan driver for Windows with clang-cl
rem (ADR-0009 phase B). Expects a clone at MESA_SRC with
rem 0001-venus-builds-with-clang-cl.patch applied, vn_renderer_entangled.c and
rem ..\driver\public.h (as entangled_gpu_public.h) copied into
rem src\virtio\vulkan (prepare.ps1 does all of that), LLVM's clang-cl and the
rem MSVC build tools. The defaults are the development host's; CI sets
rem MESA_SRC, MESA_BUILD, LLVM_BIN and VCVARS.
setlocal
if not defined VCVARS set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat"
if not defined LLVM_BIN set "LLVM_BIN=C:\Users\pawel\scoop\apps\llvm\current\bin"
if not defined MESA_SRC set "MESA_SRC=F:\entangled-targets\mesa"
if not defined MESA_BUILD set "MESA_BUILD=F:\entangled-targets\mesa-clang"
call "%VCVARS%" x64 >nul
set "PATH=%LLVM_BIN%;%PATH%"
set CC=clang-cl
set CXX=clang-cl
cd /d "%MESA_SRC%"
if not exist "%MESA_BUILD%\build.ninja" (
  python -m mesonbuild.mesonmain setup "%MESA_BUILD%" -Dbuildtype=release ^
    -Dplatforms=windows -Dvulkan-drivers=virtio -Dgallium-drivers= -Dopengl=false -Dgles1=disabled ^
    -Dgles2=disabled -Degl=disabled -Dglx=disabled -Dllvm=disabled -Dshared-glapi=disabled ^
    -Dzstd=disabled -Dexpat=disabled -Dvalgrind=disabled -Dlibunwind=disabled ^
    -Dmicrosoft-clc=disabled -Dspirv-to-dxil=false -Dbuild-tests=false -Db_vscrt=mt
  if errorlevel 1 exit /b 1
)
python -m mesonbuild.mesonmain configure "%MESA_BUILD%" "-Dc_args=" >nul
ninja -C "%MESA_BUILD%" src/virtio/vulkan/vulkan_virtio.dll 2>&1
if errorlevel 1 exit /b 1
endlocal

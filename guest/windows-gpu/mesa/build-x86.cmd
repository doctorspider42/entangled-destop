@echo off
rem The 32-bit build of the Venus driver (build.cmd's twin): what a 32-bit
rem program's Vulkan loader takes - DXVK under a 32-bit Direct3D 9 game is
rem the reason. Same tree and patches as build.cmd, a build directory of its
rem own, the x86 MSVC libraries and clang-cl with -m32.
setlocal
if not defined VCVARS set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat"
if not defined LLVM_BIN set "LLVM_BIN=C:\Users\pawel\scoop\apps\llvm\current\bin"
if not defined MESA_SRC set "MESA_SRC=F:\entangled-targets\mesa"
if not defined MESA_BUILD32 set "MESA_BUILD32=F:\entangled-targets\mesa-clang-x86"
call "%VCVARS%" x64_x86 >nul
set "PATH=%LLVM_BIN%;%PATH%"
set CC=clang-cl
set CXX=clang-cl
rem Through CL, which clang-cl reads as cl does: the resource compiler is
rem handed the compiler command whole and knows no -m32. The cross file is
rem what tells meson the target is x86 (it picks the sources by it).
set CL=-m32
cd /d "%MESA_SRC%"
if not exist "%MESA_BUILD32%\build.ninja" (
  python -m mesonbuild.mesonmain setup "%MESA_BUILD32%" --cross-file "%~dp0x86-windows.ini" -Dbuildtype=release ^
    -Dplatforms=windows -Dvulkan-drivers=virtio -Dgallium-drivers= -Dopengl=false -Dgles1=disabled ^
    -Dgles2=disabled -Degl=disabled -Dglx=disabled -Dllvm=disabled -Dshared-glapi=disabled ^
    -Dzstd=disabled -Dexpat=disabled -Dvalgrind=disabled -Dlibunwind=disabled ^
    -Dmicrosoft-clc=disabled -Dspirv-to-dxil=false -Dbuild-tests=false -Db_vscrt=mt
  if errorlevel 1 exit /b 1
)
ninja -C "%MESA_BUILD32%" src/virtio/vulkan/vulkan_virtio.dll 2>&1
if errorlevel 1 exit /b 1
endlocal

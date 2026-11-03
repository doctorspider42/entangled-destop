@echo off
rem Builds Mesa 26.2.6's OpenGL for Windows over Vulkan: Zink behind the gallium
rem WGL frontend, giving opengl32.dll and libgallium_wgl.dll (ADR-0009 phase D).
rem Same tree, patch and variables as build.cmd (MESA_BUILD defaults to
rem mesa-zink here); the GLSL compiler also needs win_flex/win_bison
rem (scoop install winflexbison, or WINFLEXBISON - build-time only).
rem Run beside an application with GALLIUM_DRIVER=zink.
setlocal
if not defined VCVARS set "VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat"
if not defined LLVM_BIN set "LLVM_BIN=C:\Users\pawel\scoop\apps\llvm\current\bin"
if not defined WINFLEXBISON set "WINFLEXBISON=C:\Users\pawel\scoop\apps\winflexbison\current"
if not defined MESA_SRC set "MESA_SRC=F:\entangled-targets\mesa"
if not defined MESA_BUILD set "MESA_BUILD=F:\entangled-targets\mesa-zink"
call "%VCVARS%" x64 >nul
set "PATH=%LLVM_BIN%;%WINFLEXBISON%;%PATH%"
set CC=clang-cl
set CXX=clang-cl
cd /d "%MESA_SRC%"
if not exist "%MESA_BUILD%\build.ninja" (
  python -m mesonbuild.mesonmain setup "%MESA_BUILD%" -Dbuildtype=release ^
    -Dplatforms=windows -Dvulkan-drivers= -Dgallium-drivers=zink -Dopengl=true -Dgles1=disabled ^
    -Dgles2=disabled -Degl=disabled -Dglx=disabled -Dllvm=disabled -Dshared-glapi=enabled ^
    -Dzstd=disabled -Dexpat=disabled -Dvalgrind=disabled -Dlibunwind=disabled ^
    -Dmicrosoft-clc=disabled -Dspirv-to-dxil=false -Dbuild-tests=false -Db_vscrt=mt -Dzlib:default_library=static
  if errorlevel 1 exit /b 1
)
ninja -C "%MESA_BUILD%" 2>&1
if errorlevel 1 exit /b 1
endlocal

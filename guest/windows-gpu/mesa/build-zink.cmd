@echo off
rem Builds Mesa 26.2.6's OpenGL for Windows over Vulkan: Zink behind the gallium
rem WGL frontend, giving opengl32.dll and libgallium_wgl.dll (ADR-0009 phase D).
rem Same tree and patch as build.cmd; the GLSL compiler also needs
rem win_flex/win_bison (scoop install winflexbison - build-time only).
rem Run beside an application with GALLIUM_DRIVER=zink.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" x64 >nul
set "PATH=C:\Users\pawel\scoop\apps\llvm\current\bin;C:\Users\pawel\scoop\apps\winflexbison\current;%PATH%"
set CC=clang-cl
set CXX=clang-cl
cd /d F:\entangled-targets\mesa
if not exist F:\entangled-targets\mesa-zink\build.ninja (
  python -m mesonbuild.mesonmain setup F:\entangled-targets\mesa-zink -Dbuildtype=release ^
    -Dplatforms=windows -Dvulkan-drivers= -Dgallium-drivers=zink -Dopengl=true -Dgles1=disabled ^
    -Dgles2=disabled -Degl=disabled -Dglx=disabled -Dllvm=disabled -Dshared-glapi=enabled ^
    -Dzstd=disabled -Dexpat=disabled -Dvalgrind=disabled -Dlibunwind=disabled ^
    -Dmicrosoft-clc=disabled -Dspirv-to-dxil=false -Dbuild-tests=false -Db_vscrt=mt -Dzlib:default_library=static
  if errorlevel 1 exit /b 1
)
ninja -C F:\entangled-targets\mesa-zink 2>&1

@echo off
rem Builds Mesa 26.2.6's Venus Vulkan driver for Windows with clang-cl
rem (ADR-0009 phase B). Expects the clone at F:\entangled-targets\mesa with
rem 0001-venus-builds-with-clang-cl.patch applied, LLVM from scoop and the
rem MSVC build tools. Our renderer backend is still to come.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" x64 >nul
set "PATH=C:\Users\pawel\scoop\apps\llvm\current\bin;%PATH%"
set CC=clang-cl
set CXX=clang-cl
cd /d F:\entangled-targets\mesa
if not exist F:\entangled-targets\mesa-clang\build.ninja (
  python -m mesonbuild.mesonmain setup F:\entangled-targets\mesa-clang -Dbuildtype=release ^
    -Dplatforms=windows -Dvulkan-drivers=virtio -Dgallium-drivers= -Dopengl=false -Dgles1=disabled ^
    -Dgles2=disabled -Degl=disabled -Dglx=disabled -Dllvm=disabled -Dshared-glapi=disabled ^
    -Dzstd=disabled -Dexpat=disabled -Dvalgrind=disabled -Dlibunwind=disabled ^
    -Dmicrosoft-clc=disabled -Dspirv-to-dxil=false -Dbuild-tests=false -Db_vscrt=mt >nul
)
python -m mesonbuild.mesonmain configure F:\entangled-targets\mesa-clang "-Dc_args=" >nul
ninja -C F:\entangled-targets\mesa-clang src/virtio/vulkan/vulkan_virtio.dll 2>&1

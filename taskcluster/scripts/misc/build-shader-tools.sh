#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.
set -euo pipefail
set -x

export PATH="$MOZ_FETCHES_DIR/clang/bin:$PATH"

target="${1:?Expected a shader-tool host triple}"
cc="$MOZ_FETCHES_DIR/clang/bin/clang"
cxx="$MOZ_FETCHES_DIR/clang/bin/clang++"
exe_suffix=""
cmake_args=()
case "$target" in
x86_64-linux-gnu|aarch64-linux-gnu)
    cmake_args+=(
        -DCMAKE_SYSTEM_NAME=Linux
        "-DCMAKE_SYSROOT=$MOZ_FETCHES_DIR/sysroot-$target"
        "-DCMAKE_EXE_LINKER_FLAGS=-fuse-ld=lld -static-libstdc++ -static-libgcc"
    )
    ;;
x86_64-pc-windows-msvc|aarch64-pc-windows-msvc)
    cc="$MOZ_FETCHES_DIR/clang/bin/clang-cl"
    cxx="$cc"
    exe_suffix=".exe"
    windows_arch=AMD64
    if [ "$target" = aarch64-pc-windows-msvc ]; then
        windows_arch=ARM64
    fi
    windows_flags="-Xclang -ivfsoverlay -Xclang \"$MOZ_FETCHES_DIR/vs/overlay.yaml\" -winsysroot \"$MOZ_FETCHES_DIR/vs\""
    cmake_args+=(
        -DCMAKE_SYSTEM_NAME=Windows
        "-DCMAKE_SYSTEM_PROCESSOR=$windows_arch"
        "-DCMAKE_LINKER=$MOZ_FETCHES_DIR/clang/bin/lld-link"
        "-DCMAKE_RC_COMPILER=$MOZ_FETCHES_DIR/clang/bin/llvm-rc"
        "-DCMAKE_MT=$MOZ_FETCHES_DIR/clang/bin/llvm-mt"
        -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded
        "-DCMAKE_C_FLAGS=$windows_flags"
        "-DCMAKE_CXX_FLAGS=-EHsc $windows_flags"
        "-DCMAKE_EXE_LINKER_FLAGS=-winsysroot:\"$MOZ_FETCHES_DIR/vs\""
    )
    ;;
x86_64-apple-darwin|aarch64-apple-darwin)
    MACOS_TARGET="$target"
    # shellcheck source=taskcluster/scripts/misc/macos-setup.sh
    source "$(dirname "$0")/macos-setup.sh"
    macos_arch=x86_64
    if [ "$target" = aarch64-apple-darwin ]; then
        macos_arch=arm64
    fi
    cmake_args+=(
        -DCMAKE_SYSTEM_NAME=Darwin
        "-DCMAKE_SYSTEM_PROCESSOR=$macos_arch"
        "-DCMAKE_OSX_ARCHITECTURES=$macos_arch"
        "-DCMAKE_OSX_SYSROOT=$MACOS_SDK"
        "-DCMAKE_OSX_DEPLOYMENT_TARGET=$MACOSX_DEPLOYMENT_TARGET"
        "-DCMAKE_AR=$MACOS_AR"
        "-DCMAKE_RANLIB=$MACOS_RANLIB"
        "-DCMAKE_LINKER=$MACOS_LD"
        "-DCMAKE_C_FLAGS=$MACOS_EXTRA_CFLAGS"
        "-DCMAKE_CXX_FLAGS=$MACOS_EXTRA_CFLAGS"
        -DCMAKE_EXE_LINKER_FLAGS=-fuse-ld=lld
    )
    ;;
*)
    echo "Unsupported shader-tool host: $target" >&2
    exit 1
    ;;
esac

ln -sfn "$MOZ_FETCHES_DIR/spirv-tools" "$MOZ_FETCHES_DIR/glslang/External/spirv-tools"

cmake -S "$MOZ_FETCHES_DIR/glslang" -B shader-tools-build -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_C_COMPILER="$cc" \
    -DCMAKE_CXX_COMPILER="$cxx" \
    -DCMAKE_C_COMPILER_TARGET="$target" \
    -DCMAKE_CXX_COMPILER_TARGET="$target" \
    -DBUILD_SHARED_LIBS=OFF \
    -DENABLE_OPT=ON \
    -DGLSLANG_TESTS=OFF \
    -DSPIRV_SKIP_TESTS=ON \
    -DSPIRV_SKIP_EXECUTABLES=OFF \
    -DSPIRV-Headers_SOURCE_DIR="$MOZ_FETCHES_DIR/spirv-headers" \
    "${cmake_args[@]}"

cmake --build shader-tools-build \
    --parallel "${CMAKE_BUILD_PARALLEL_LEVEL:-$(nproc)}" \
    --target glslang-standalone spirv-val spirv-dis

mkdir -p shader-tools/bin "$UPLOAD_DIR"
cp "shader-tools-build/StandAlone/glslang$exe_suffix" "shader-tools/bin/glslangValidator$exe_suffix"
cp shader-tools-build/External/spirv-tools/tools/spirv-{val,dis}"$exe_suffix" shader-tools/bin/
tar -acf "$UPLOAD_DIR/shader-tools.tar.zst" shader-tools

#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.
set -euo pipefail
set -x

export PATH="$MOZ_FETCHES_DIR/clang/bin:$PATH"

ln -sfn "$MOZ_FETCHES_DIR/spirv-tools" "$MOZ_FETCHES_DIR/glslang/External/spirv-tools"

cmake -S "$MOZ_FETCHES_DIR/glslang" -B shader-tools-build -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_SYSTEM_NAME=Linux \
    -DCMAKE_C_COMPILER="$MOZ_FETCHES_DIR/clang/bin/clang" \
    -DCMAKE_CXX_COMPILER="$MOZ_FETCHES_DIR/clang/bin/clang++" \
    -DCMAKE_C_COMPILER_TARGET="$1" \
    -DCMAKE_CXX_COMPILER_TARGET="$1" \
    -DCMAKE_SYSROOT="$MOZ_FETCHES_DIR/sysroot-$1" \
    -DCMAKE_EXE_LINKER_FLAGS="-fuse-ld=lld -static-libstdc++ -static-libgcc" \
    -DBUILD_SHARED_LIBS=OFF \
    -DENABLE_OPT=ON \
    -DGLSLANG_TESTS=OFF \
    -DSPIRV_SKIP_TESTS=ON \
    -DSPIRV_SKIP_EXECUTABLES=OFF \
    -DSPIRV-Headers_SOURCE_DIR="$MOZ_FETCHES_DIR/spirv-headers"

cmake --build shader-tools-build \
    --parallel "${CMAKE_BUILD_PARALLEL_LEVEL:-$(nproc)}" \
    --target glslang-standalone spirv-val spirv-dis

mkdir -p shader-tools/bin "$UPLOAD_DIR"
cp shader-tools-build/StandAlone/glslang shader-tools/bin/glslangValidator
cp shader-tools-build/External/spirv-tools/tools/spirv-{val,dis} shader-tools/bin/
tar -acf "$UPLOAD_DIR/shader-tools.tar.zst" shader-tools

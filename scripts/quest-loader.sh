#!/usr/bin/env bash
# Builds the Khronos OpenXR loader for the Quest (arm64 Android) from the pinned
# SDK submodule into build/quest-loader.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
ndk="${ANDROID_NDK_HOME:-$(ls -d "$HOME"/Library/Android/sdk/ndk/* | sort -V | tail -1)}"
out="$root/build/quest-loader"

cmake -S "$root/third_party/OpenXR-SDK-Source" -B "$out" -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$ndk/build/cmake/android.toolchain.cmake" \
  -DANDROID_ABI=arm64-v8a \
  -DANDROID_PLATFORM=android-29 \
  -DCMAKE_BUILD_TYPE=Release \
  -DDYNAMIC_LOADER=ON \
  -DBUILD_TESTS=OFF \
  -DBUILD_API_LAYERS=OFF
cmake --build "$out" --target openxr_loader

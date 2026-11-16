#!/usr/bin/env bash
# Builds the Khronos loader and hello_xr into build/openxr-sdk.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/third_party/OpenXR-SDK-Source"
out="$root/build/openxr-sdk"

# The Command Line Tools can ship a newer SDK than their linker understands;
# CMake picks the newest one, so pin the toolchain and SDK to Xcode's.
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
sdk="$(xcrun --sdk macosx --show-sdk-path)"

cmake -S "$src" -B "$out" -G Ninja \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DCMAKE_OSX_SYSROOT="$sdk" \
  -DCMAKE_C_COMPILER="$(xcrun -f clang)" \
  -DCMAKE_CXX_COMPILER="$(xcrun -f clang++)" \
  -DDYNAMIC_LOADER=ON \
  -DBUILD_TESTS=ON
cmake --build "$out"

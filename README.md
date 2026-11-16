# Tether

A native macOS OpenXR runtime that streams to a Meta Quest 3 over USB-C or Wi-Fi, so
OpenXR apps on the Mac run in the headset.

- Architecture: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- Milestones: [`docs/ROADMAP.md`](docs/ROADMAP.md)
- Agent / contributor instructions: [`AI.md`](AI.md)

## Setup

```sh
git submodule update --init
brew install cmake ninja
xcodebuild -downloadComponent MetalToolchain
./scripts/openxr-sdk.sh        # Khronos loader + hello_xr → build/openxr-sdk
cargo build                    # runtime → target/debug/libtether_runtime.dylib
```

## Run an OpenXR app against Tether

```sh
./scripts/run.sh build/openxr-sdk/src/tests/hello_xr/hello_xr -g Metal
```

`scripts/run.sh` writes the runtime manifest and points the loader at it through
`XR_RUNTIME_JSON`, so nothing is installed system-wide.

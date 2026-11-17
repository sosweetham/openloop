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

`scripts/run.sh` makes sure this checkout's `tetherd`, the per-user agent that owns the
headset, is installed and running (`scripts/tetherd.sh ensure`), then writes the runtime
manifest and points the loader at it through `XR_RUNTIME_JSON`; the runtime itself is not
installed system-wide. `scripts/tetherd.sh` also restarts, stops, shows, follows and
uninstalls the agent. The headset needs this checkout's Quest app
(`scripts/quest-apk.sh --install`).

To hear the app in the headset instead of on the Mac, grant `tetherd` the System Audio
Recording permission once, sitting at the Mac: `scripts/tetherd.sh audio-permission` (click
Allow), then `scripts/tetherd.sh audio-check`. `TETHER_AUDIO=off scripts/tetherd.sh install`
keeps the sound on the Mac.

The same works for a Godot 4.7+ project, which is the bar for "useful to game
developers": [`examples/godot/hello`](examples/godot/hello) is stock Godot with
OpenXR enabled and nothing Tether-specific.

```sh
godot --headless --import --path examples/godot/hello   # once
./scripts/run.sh godot --path examples/godot/hello
```

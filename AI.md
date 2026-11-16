# Project Instructions

Common instructions for all AI agents and assistants working in this repository.
This file (`AI.md`) is the single source of truth; all agent-specific configuration
files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, etc.) are symlinks to it.

## Project

**Tether** — a native macOS OpenXR runtime that uses a Meta Quest 3 as its display
and tracking hardware, over USB-C or Wi-Fi. Any OpenXR app on the Mac (Metal via
`XR_KHR_metal_enable`) runs in the headset as if the headset were plugged into a PC.
Tether is the baseline every later Mac VR app, game or experience is built on; it
contains no game or app logic of its own.

## Architecture

The process model, frame path, clock domains and transports live in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); milestones live in
[`docs/ROADMAP.md`](docs/ROADMAP.md). Keep that detail there, not in this file.

**Stack at a glance:** Cargo workspace. Rust for the runtime library, the service and
the Quest client; Metal Shading Language for shaders; Swift only for the menu-bar
app, never on a real-time path. Khronos's OpenXR SDK is a pinned submodule under
`third_party/`, built by `scripts/openxr-sdk.sh` into `build/`.

| Path                       | Crate / artifact       | What                                    |
| -------------------------- | ---------------------- | --------------------------------------- |
| `packages/rust/openxr-abi` | `tether-openxr-abi`    | OpenXR C types, generated from headers  |
| `packages/rust/runtime`    | `libtether_runtime`    | The OpenXR runtime the loader loads     |
| `third_party/OpenXR-SDK-Source` | —                 | Khronos loader, headers, `hello_xr`     |

## Toolchain (required)

- **Build C/C++ and Metal against Xcode, not the Command Line Tools.** The CLT can
  ship a macOS SDK newer than their linker understands, and CMake picks the newest
  SDK it finds. `scripts/openxr-sdk.sh` pins `DEVELOPER_DIR` and the sysroot; any new
  native build script does the same.
- **The Metal toolchain is a separate Xcode component**:
  `xcodebuild -downloadComponent MetalToolchain`. Without it CMake silently builds
  `hello_xr` with no Metal plugin.

## Real-Time Paths (required)

Frame pacing, pose handling, compositor submission and packet send/receive run on
real-time threads, and **a real-time thread never allocates, locks or blocks.** A
missed deadline here is a dropped frame in somebody's face, so:

- Pre-allocate at session start; hand data between threads through fixed-capacity
  lock-free rings or atomics, never `Mutex`, channels that allocate, or `Arc` clones
  in the hot loop.
- No logging formatters, `String` building or syscalls that can sleep on those
  threads. Record into a pre-allocated trace buffer and format elsewhere.
- Real-time threads are marked as such (Mach time-constraint policy or an
  `os_workgroup`), and the period/computation budget is derived from the headset's
  refresh rate, never hard-coded.
- Times are `XrTime` nanoseconds in the Mac's monotonic domain unless the type says
  otherwise; anything in the headset's clock domain carries that in its type.

## Comments & Self-Documenting Code (required)

**The code says what it does; a comment says only what the code cannot.** Names,
types and small functions carry the meaning. A comment that restates them is a second
copy of the truth that no compiler checks, and it rots.

- Never explain an abstraction you are only using; name the module or say nothing.
- A comment describes the code, never its history. The diff owns the history.
- Never restate the next line.
- A module header is a few lines: what this is, and the doc of record in `docs/`.
- Where a comment IS right: a why the code cannot show — a platform bug and its
  workaround, an invariant the caller must uphold, an OpenXR spec clause the code is
  satisfying (cite the section or VUID), a safety argument for an `unsafe` block, a
  `TODO` with an owner. Keep them short.

## Commits & AI Attribution (required)

**AI is never to be attributed in commits.** Do not add `Co-Authored-By` lines for
any AI tool, do not mention Claude/Codex/Gemini/Copilot in commit messages, and do
not add "Generated with AI" trailers. The human author owns every commit.

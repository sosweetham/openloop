# Tether — Architecture

Tether is an OpenXR runtime for macOS whose display and tracking hardware is a Meta
Quest 3 reached over USB-C or Wi-Fi. Apps link the Khronos loader as usual; the
loader finds Tether through its runtime manifest.

## Processes

```
 OpenXR app (hello_xr, Godot, Qt, our own)
   │  Khronos loader → runtime manifest → libtether_runtime.dylib
   ▼
 libtether_runtime (in the app's process)
   OpenXR API surface: instance, session state machine, spaces,
   actions, Metal swapchains, frame loop
   │  Mach IPC + shared memory · IOSurface swapchains · MTLSharedEvent
   ▼
 tetherd (one service per Mac)
   headset link · clock sync · pose prediction · frame pacing
   Metal compositor → VideoToolbox HEVC
   transports: USB (Android Open Accessory, native ADB fallback) | Wi-Fi (UDP)
   │
   ▼  video, haptics            ▲ poses, input, display timing
 Quest client (native Android app, OpenXR on the headset)
   MediaCodec decode → submit with the pose each frame was rendered for,
   so the headset reprojects and distorts against its latest pose
```

**The service owns the headset; apps never do.** An app crashing never drops the
link, and the design leaves room for several apps and system UI. Swapchain images are
IOSurface-backed `MTLTexture`s shared between the app and `tetherd` with no copies,
ordered by `MTLSharedEvent`s.

**On the Mac, Tether is background software and never takes a Dock slot.** `tetherd` is a
per-user launchd agent that starts at login and runs whether or not any UI is open. The only
visible surface is a menu-bar item (SwiftUI `MenuBarExtra`, `LSUIElement = YES`) showing link
state, the running app and latency; settings open as a window from that menu and closing it
leaves everything running.

**The headset does lens distortion and reprojection.** Tether composites layers,
encodes and tags every frame with its render pose; the Quest's own compositor does
the rest.

## Time

`XrTime` is nanoseconds in the Mac's monotonic clock. `tetherd` continuously maps it
to the headset's display clock. `xrWaitFrame` paces the app to the headset's vsync,
shifted earlier by the measured render, encode, transmit and decode latency. Predicted
display times handed to the app are in the Mac domain; the mapping lives in one place.

## Transports

One session protocol with prioritized channels runs over either link:

| Channel  | Direction    | Delivery                       |
| -------- | ------------ | ------------------------------ |
| video    | Mac → Quest  | high priority, frames droppable |
| poses    | Quest → Mac  | highest priority, latest wins  |
| control  | both         | reliable                       |
| bulk     | Mac → Quest  | reliable, lowest priority      |
| haptics  | Mac → Quest  | high priority                  |

- **USB:** Android Open Accessory over IOKit, with a native ADB transport as the
  fallback if the Quest firmware blocks AOA. Lossless, so no FEC; higher bitrate.
- **Wi-Fi:** UDP with FEC and selective retransmit, adaptive bitrate, Bonjour
  discovery.

## Graphics API

Apps bind with `XR_KHR_metal_enable` (ratified in OpenXR 1.1.40). Vulkan apps via
MoltenVK are out of scope until the Metal path is complete.

## Repository layout

```
tether/
├── packages/rust/
│   ├── openxr-abi/   tether-openxr-abi  — OpenXR C types generated from the SDK headers
│   └── runtime/      tether-runtime     — the runtime dylib the loader loads
├── scripts/          SDK build, binding generation, run-against-Tether
├── third_party/      Khronos OpenXR-SDK-Source (pinned submodule)
└── docs/
```

`apps/` arrives with `tetherd`, the Quest client and the menu-bar app (see
[`ROADMAP.md`](ROADMAP.md)).

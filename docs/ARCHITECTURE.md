# Tether — Architecture

Tether is an OpenXR runtime for macOS whose display and tracking hardware is a Meta
Quest 3 reached over USB-C or Wi-Fi. Apps link the Khronos loader as usual; the
loader finds Tether through its runtime manifest.

This page is the design that R2 completes; [`ROADMAP.md`](ROADMAP.md) § R2 stages the move and
[Today](#today) says where it stands.

## Processes

```
 OpenXR app (hello_xr, Godot, ...)
   │  Khronos loader → runtime manifest → libtether_runtime.dylib
   ▼
 libtether_runtime (in the app's process)
   OpenXR API surface: instance, session state machine, spaces,
   actions, Metal swapchains, frame loop
   │  control  NSXPC over tetherd's Mach service, setup calls only
   │  data     xpc_shmem: wait-free records and SPSC rings, no message per frame
   │  GPU      MTLSharedTextureHandle swapchain images, MTLSharedEvent ordering
   ▼
 tetherd (per-user launchd agent)
   headset link · adb tunnel · clock sync · tracking · frame pacing
   Metal compositor → VideoToolbox HEVC · Core Audio process tap
   transports: USB | Wi-Fi (UDP)
   │
   ▼  video, audio, haptics           ▲ poses, input, display timing
 Quest client (native Android app, OpenXR on the headset)
   MediaCodec decode → submit with the pose each frame was rendered for,
   so the headset reprojects and distorts against its latest pose;
   AAudio plays the app's sound at its presentation time
```

**The service owns the headset; apps never do.** An app crashing never drops the
link, and the design leaves room for several apps and system UI.

**On the Mac, Tether is background software and never takes a Dock slot.** `tetherd` is a
per-user launchd agent that starts at login and runs whether or not any UI is open. The only
visible surface is a menu-bar item (SwiftUI `MenuBarExtra`, `LSUIElement = YES`) showing link
state, the running app and latency; settings open as a window from that menu and closing it
leaves everything running.

**The headset does lens distortion and reprojection.** Tether composites layers,
encodes and tags every frame with its render pose; the Quest's own compositor does
the rest.

## Runtime ↔ tetherd

**Control plane: NSXPC.** `tetherd` checks in with launchd for the Mach service
`dev.tether.tetherd`, and the runtime connects to it with `NSXPCConnection` when the app
creates its first instance; if it cannot, `xrCreateInstance` fails at once with
`XR_ERROR_RUNTIME_UNAVAILABLE`. `TETHER_SERVICE` points the runtime at another Mach service,
which is how tests reach a `tetherd` of their own. Calls are rare and setup-only (hello with versions and the GPU's
`registryID`, session begin and end, swapchains, shared events): XPC allocates on every
message, so nothing with a frame-rate cadence uses it. NSXPC is the only transport that
carries Metal's shared handles, and `NSXPCInterface` takes only protocol metadata emitted by
clang, so the protocol is the tree's one Objective-C file (in `packages/rust/ipc`, compiled by
its `build.rs`, and usable by the Swift menu-bar app). Everything else is Rust: objc2 for
NSXPC, hand-written libxpc FFI for shared memory.

**Data plane: shared memory.** `tetherd` creates an `xpc_shmem` region for each client and
passes it over the connection. Every field has exactly one writing process, so a crashed peer
leaves nothing locked.
- What real-time threads read (headset profile, tracking, clock mapping, frame timing, focus,
  recenter and keyframe-request counters) is published in wait-free multi-slot records: the
  writer fills a ring of seqlocked slots and never touches the one it published last, so a read
  succeeds on its first copy unless the writer publishes N−1 more values during it. A plain
  seqlock reader can stall for as long as its writer keeps writing, and a classic triple buffer
  allows only one reader, where these records have readers in both processes.
- Frame submissions, haptics and, until S3, encoded video go through fixed-capacity
  single-producer, single-consumer rings.
- A waiting thread sleeps on a word in the region (`os_sync_wait_on_address`, shared);
  waking it never blocks the writer.

**GPU: shared textures and shared events.** Swapchain images are Private-storage textures
from `newSharedTextureWithDescriptor:`, passed between the processes as
`MTLSharedTextureHandle`. IOSurface-backed textures cannot be arrays, mipmapped or depth
(Godot renders multiview into two-layer arrays), and XR_KHR_metal_enable requires Private
storage. A shared texture only works on the GPU that made it, so the runtime hands the app
`tetherd`'s device, matched by `registryID`. At `xrEndFrame` the runtime signals an
`MTLSharedEvent` on the app's command queue; `tetherd`'s compositor waits for it on its own
queue (Metal 4 `MTL4CommandQueue`), composes, and signals a second event that
`xrWaitSwapchainImage` checks before handing the image back.

**The app must render into swapchain images on the queue it passed in
`XrGraphicsBindingMetalKHR::commandQueue`** ("a Metal command queue to be used for
rendering", XR_KHR_metal_enable). Metal orders nothing across queues, so work on any other
queue may still be writing when `tetherd` reads.

**Failures stay on their side.** When an app dies, `tetherd` signals that client's events
past any pending wait, so its own queue never stalls, drops the client's textures and keeps
the headset link. While `tetherd` is gone the runtime holds the last pose and paces itself,
and `xrWaitSwapchainImage` returns `XR_TIMEOUT_EXPIRED` at its timeout instead of waiting on
a dead peer.

**Identity.** `tetherd` serves only its own user. It is signed with an Apple Development
identity under the fixed identifier `dev.tether.tetherd`, with the hardened runtime and the
`com.apple.security.device.audio-input` entitlement, and embeds an Info.plist
(`__TEXT,__info_plist`) with the same bundle identifier, so the audio-capture permission survives
rebuilds. The runtime talks to nothing that fails `tether_ipc::TETHERD_REQUIREMENT` (that
identifier, signed by Tether's team): any process of the user's can register a Mach service
name, but not sign as `tetherd`.

## Time

`XrTime` is nanoseconds in the Mac's monotonic clock (`CLOCK_UPTIME_RAW`). Mac-initiated
pings map it to the headset's clock, keeping the fastest round trip; the mapping lives in one
place, and every time the app sees is in the Mac domain. From R2 S4, `xrWaitFrame` paces the
app to the headset's vsync, shifted earlier by the measured render, encode, transmit and
decode latency. Today it paces a local grid at the headset's nominal refresh rate and predicts
display a constant `PIPELINE_LATENCY_NS` (40 ms) ahead, a figure `tetherd` publishes in each
session's timing record and also delays the app's sound by.

## Audio

`tetherd` plays the sound of the app in front in the headset. It captures that app with a
private, stereo-mixdown Core Audio process tap on its PID, taken from the audit token of the
app's NSXPC connection and carried by a private aggregate device (`tether-audiotap`). The tap
is `CATapMutedWhenTapped`: the Mac is silent only while `tetherd` reads it, so the speakers come
back by themselves if `tetherd` stops. The IOProc only copies into a pre-allocated ring stamped
with the capture time as `XrTime`; one non-real-time thread makes every other HAL call, since
each can stall for tens of seconds. Capture follows the session begun last: the next session's
app replaces it, and it stops when that session ends or its app's connection dies.

Each 5 ms packet is to be heard `PIPELINE_LATENCY_NS` (`tether-stream`) after its capture, on
the headset's clock. A video frame is put on the headset's clock by the same mapping, and its
display time is the same latency after its frame deadline: `tetherd` publishes the latency in
each session's timing record and `xrWaitFrame` adds it to every predicted display time. Sound
and picture of one app tick therefore line up, apart from the app's own mix-to-capture time.
The protocol carries only the headset's rate and channel count, so when the tap's rate differs
`tetherd` converts it with a windowed sinc band-limited to the lower rate, and the headset's
resampler only trims clock drift. A stream opens with `AudioStart` (again on every new link),
ends with `AudioStop`, and a headset in another format gets a new stream.

The Quest client's network thread writes packets straight into the player's ring, never
through the once-per-frame main loop. Its AAudio callback plays what is due at the instant
AAudio says its next frame will be heard, and a ±0.1 % resampling trim absorbs the drift
between the two audio clocks. When the output latency makes that schedule impossible, it
plays late and reports the offset rather than going silent.

**Permission.** Capture needs the System Audio Recording permission for `tetherd` itself.
`tetherd`'s Info.plist carries `NSAudioCaptureUsageDescription`, and TCC was seen keying a bare
binary's grants on its path and code requirement, so the prompt is answered once, at the Mac,
by the installed binary: `scripts/tetherd.sh audio-permission` runs it as a one-shot
launchd job that taps a silent `afplay`, and `scripts/tetherd.sh audio-check` then captures a
known tone through the same path and measures it. Either run that receives audio writes
`audio-granted` beside the binary, and until then the agent never touches Core Audio. No tap
operation is ever issued while a prompt may be pending, since one once wedged audio for every
app on the Mac for hours: a HAL call that stalls, or IO that delivers nothing, writes
`audio-blocked`, and nothing touches Core Audio again until a person answers the prompt and
runs `audio-permission --prompt-answered`. `--audio off` (`TETHER_AUDIO=off` at install) leaves
the sound on the Mac; `--audio synthetic` sends a 440 Hz tone for every app, which is what the
tests use.

## Transports

One session protocol with prioritized channels runs over either link:

| Channel  | Direction    | Delivery                                  |
| -------- | ------------ | ----------------------------------------- |
| video    | Mac → Quest  | high priority, frames droppable           |
| audio    | Mac → Quest  | high priority, played by presentation time |
| poses    | Quest → Mac  | highest priority, latest wins             |
| control  | both         | reliable                                  |
| bulk     | Mac → Quest  | reliable, lowest priority                 |
| haptics  | Mac → Quest  | high priority                             |

- **USB:** Android Open Accessory over IOKit, with a native ADB transport as the
  fallback if the Quest firmware blocks AOA. Lossless, so no FEC; higher bitrate.
- **Wi-Fi:** UDP with FEC and selective retransmit, adaptive bitrate, Bonjour
  discovery.

Today USB is one TCP stream through `adb reverse` to `127.0.0.1:47800`
(`tether_protocol::USB_PORT`). Messages go whole, so priority works only between them: a video
frame waits for every audio packet, haptic and control message already waiting for the link,
but nothing interrupts a frame being written. At the measured ~250 MB/s of the tunnel, a
500 KB keyframe holds the link for about 2 ms. `tetherd` keeps the tunnel up while a headset is attached: the adb server pushes
the device list (`host:track-devices`), so nothing polls while none is, and for each device
`tetherd` runs the SDK's `adb reverse tcp:47800 tcp:<its port>`, re-checked every few seconds
because Android drops it whenever USB or adb resets. With no adb server running it starts one.
`TETHER_USB_PORT` moves the Mac's listener off 47800 (`0`: any free port), which is how tests
stay away from a real headset; adb is then left alone.

## Headsets

Quest 3 is the only target today, but **the Mac side never assumes a headset.** Everything
headset-specific lives in its client and in the device profile the client sends on connect:
displays (per-eye resolution, FOV, refresh rates), decodable codecs and limits, interaction
profiles, and features (hand tracking, passthrough, boundary). `tetherd` configures itself from
that profile, so another Android OpenXR headset is a profile plus quirks, and Vision Pro is a new
client with no Mac-side change.

## Quest client

Pure Rust plus an `AndroidManifest.xml`: `NativeActivity` via `android-activity`, OpenXR
through the Khronos Android loader, and `AMediaCodec` decoding into an `AImageReader` whose
images GLES 3 samples through EGLImages, without copies. Each eye is drawn into a swapchain the
size of the stream's eye, texel for texel (see [Video quality](#video-quality)). Tether's menu is
a cylinder layer (a quad where the runtime lacks cylinders). The APK is packaged with the SDK
command-line tools, without Gradle. The few Android features that are Java-only (USB accessory,
multicast lock, system keyboard) are reached through JNI from Rust; there are no Java or Kotlin
sources.

## Video quality

The Quest 3's panel has about 25.5 pixels per degree at the lens centre, and an eye rendered at
the headset's recommended 1680x1760 has about 13: the headset's compositor magnifies every app
pixel about twice, so aliasing and any resampling on the way show as blocky pixels. Each stage is
set so the picture is resampled once on the Mac, decoded texel for texel, and magnified once, by
the headset. The numbers below are measured on an M3 Max (30-core GPU) at 90 Hz on Kenbu's
frames: quality as PSNR and SSIM of the luma the panel lights near the lens centre against a 4x
supersampled render, through Tether's own compositor and VideoToolbox and a model of the client
and of the headset's bilinear resample.

**Render size.** `xrEnumerateViewConfigurationViews` recommends the headset's size times the
render scale, 1.4 by default (`tether_stream::video`), rounded to 16: 2352x2464 for a Quest 3.
`TETHER_RENDER_SCALE` (0.5 to 2.0) sets it for an app's process; an engine's own multiplier,
such as Godot's `render_target_size_multiplier`, applies on top. Supersampling antialiases
whatever the app draws: Kenbu's gameplay goes from 27.3 to 29.7 dB at the same encoded size,
for 1.3-1.7 ms more GPU time per frame (2.3-3.0 to 3.7-4.6 ms, compositor included). An app
that renders below the encoded size is encoded as it is, not scaled up.

**Encoded size.** Each eye is encoded at the app's size, or at the largest size of the same
aspect, in multiples of 16, that keeps VideoToolbox's p95 encode time inside one frame period:
`ENCODE_PIXEL_RATE`, 600 Mpx/s for both eyes, divided by the refresh rate. At 90 Hz that is
1776x1856 per eye, a 3552x1856 stream. Text is limited by this size, gameplay barely:

| Eye encoded | Encode p50 / p95 (ms) | Menu text | Gameplay |
| ----------- | --------------------- | --------- | -------- |
| 1680x1760   | 8.2-8.3 / 9.0-9.4     | 32.4 dB   | 29.7 dB  |
| 1776x1856   | 8.8-8.9 / 9.7-9.9     | 32.9 dB   | 29.7 dB  |
| 1840x1936   | 9.5-9.8 / 10.5-11.0   | 33.4 dB   | 29.8 dB  |
| 2016x2112   | 11.3 / 12.5           | 34.4 dB   | 29.8 dB  |

Encode times are 20 s runs from 2352x2464 eyes at 150 Mbps, the upper figures with the Mac
busy; quality is after encoding, from 2352x2464 eyes. The Quest 3's HEVC decoder is rated for
7.78 M blocks of 16x16 a second; 3552x1856 at 90 Hz needs 2.3 M.

**Downscale.** The compositor filters an eye larger than the encoded one with a separable
Lanczos-3 kernel stretched over each output pixel's footprint, in gamma-encoded R'G'B', in two
compute passes, the second writing the 4:2:0 planes. From 2352x2464 eyes to 1680x1760, before
encoding, menu text measures 32.4 dB with it, against 31.6 with Catmull-Rom, 31.1 with one
bilinear tap and 30.6 with a box filter; gameplay is within 0.1 dB of each. Filtering in linear
light measured 0.3-0.8 dB worse. For 2352x2464 eyes the passes take about 0.9 ms of GPU time at
full clock and up to 2.8 ms when the GPU idles between frames, against 0.15-0.7 ms for eyes read
texel for texel.

**Encoder.** HEVC Main at 150 Mbps over USB (the decoder is rated to 160), no frame
reordering, speed over quality, keyframes on demand, and:
- `RealTime` off. With it on, VideoToolbox slows itself to the frame rate after about 3 s: Kenbu
  through the runtime encoded in 15.0-15.5 ms p50 / 16.5-16.8 ms p95 before, and 8.8-8.9 /
  9.0-9.4 ms after (at 3552x1856 from 1.4x eyes, against 3360x1760 before).
- A blank frame warms every new session up and is dropped. A session's first frame took 70 ms
  and queued the next ten behind it, past the compositor's six slots, so streams lost frames as
  they started.
- Spatial adaptive QP off: it moves bits from detail to flat areas; off, text and edges keep
  them (+1.2 dB at 150 Mbps).
- Low-latency rate control stays off: it caps HEVC near 100 Mbps at these sizes and takes
  about 22 ms a frame when paced.

**Headset.** The client sizes its eye swapchains from each `ServerHello`, honours MediaCodec's
crop rectangle, and computes texture coordinates in `highp`: Adreno runs `mediump` as fp16, which
cannot address a 3000-texel-wide video, so the right eye repeated 703 of its 1680 columns and
menu text measured 24.7 dB in it (28.0 in the left). The video layer asks the compositor for
quality sharpening (Meta Quest Super Resolution) and the menu for quality supersampling,
through `XR_FB_composition_layer_settings`, with `XR_META_automatic_layer_filter`'s automatic
bit where offered so the compositor filters only where it helps; Meta documents only sharpening
for projection layers. The menu's Sharpening switch turns it off for comparison; its effect
cannot be measured off the headset. A runtime that refuses a frame for its layer settings gets
the stream without them from then on. The client logs whether the runtime offers both
extensions and how many bits `mediump` has.

**End to end**, frame 60 of each 90-frame sequence, after encoding:

| | Gameplay | Menu text |
| --- | --- | --- |
| Before: 1680x1760, 80 Mbps, `mediump` client; left / right eye | 27.0 / 25.9 dB, SSIM 0.875 / 0.847 | 28.0 / 24.7 dB, SSIM 0.950 / 0.910 |
| Now: 2352x2464 → 1776x1856, 150 Mbps; either eye | 29.7 dB, SSIM 0.919 | 32.9 dB, SSIM 0.981 |
| Now with `TETHER_RENDER_SCALE=1.0` | 27.3 dB, SSIM 0.889 | 32.1 dB, SSIM 0.979 |

The runtime logs each stream's frames dropped before and lost after encoding, and its encode
and composition times, when the stream ends; `tetherd`'s session report counts the frames that
never reached it and the time from each frame's start to its encoded access unit.

## Graphics API

Apps bind with `XR_KHR_metal_enable` (ratified in OpenXR 1.1.40). Vulkan apps via
MoltenVK are out of scope until the Metal path is complete.

## Testing

- `cargo test` never touches a headset: the runtime's unit tests run without a listener, and
  link tests bind ephemeral ports.
- `scripts/e2e.sh` runs Khronos's `hello_xr` on this checkout's runtime while
  `tether-fake-headset` stands in for the Quest on a port the kernel picks, in two modes:
  - **in-process** (`TETHER_INPROCESS=1`, `TETHER_USB_PORT=0`): 10 s of `hello_xr`, the fake
    headset on the port the runtime logs;
  - **tetherd**: this checkout's `tetherd`, signed as `tetherd`, under a throwaway launchd label
    that is also its Mach service, with no adb and a synthetic tone for each app's sound. One
    `hello_xr` streams and is killed mid-stream; a second starts a second later on the same
    headset link and streams for 10 s more.

  Either fails unless a stream of the size `tether_stream::video` sets for the headset starts,
  the video arrives at the headset's refresh rate, every stream starts with a keyframe, frame
  indices have no gaps, the fake headset finds no protocol errors and `hello_xr` quits cleanly.
  Tetherd mode also fails unless `tetherd` keeps its process and the headset its one link across
  the crash, drops the dead app's session, ends its stream with `StreamEnded` and writes a report
  per session; and unless the sound arrives as the tone it was, in packets with no gap in
  sequence or frames, spaced by their frames, each due about the pipeline latency after it was
  captured, and the dead app's sound stops with `AudioStop`.
  Neither touches adb, port 47800, the installed `tetherd`, Core Audio or a running app.

## Today

R2 is at stage S2: `tetherd` owns the headset and plays the app's sound in it.
- `tetherd` (`apps/tetherd`) runs the link, the adb tunnel, clock sync, the profile, focus,
  recenter, haptics and audio. It publishes tracking (already on the Mac's clock), the clock
  mapping and the session's levels into each session's region, and beats its heartbeat every
  100 ms.
- The runtime still composites and encodes with VideoToolbox in the app's process (S3 moves
  that), and pushes each access unit into its session's ring. `tetherd` sends the session begun
  last: it puts each display time on the headset's clock, opens every stream with a
  `ServerHello` and a keyframe (asking the app for one through the session's keyframe-request
  counter when it must), and sends `StreamEnded` when that session ends or its app's connection
  dies. A frame that reaches `tetherd` more than half a second after its display time, the
  backlog of a stall, is dropped and the stream resumes at a keyframe. A runtime whose region's
  heartbeat is a second old holds the last pose and stops streaming.
- `tetherd` captures the sound of the session begun last (see [Audio](#audio)). Its live Core
  Audio capture has not yet run on this Mac: that waits for the permission step with the user at
  the Mac. The audio sender takes the headset state's lock and writes to the link like the
  video forwarder; neither is real-time yet (S5).
- `tetherd` writes a JSON report per session to `~/Library/Logs/Tether/sessions/`, including what
  became of its sound.
- `xrWaitFrame` still paces a local grid (S4) and no thread is real-time (S5). A `tetherd` that
  exits takes its sessions with it: the runtime queues `XR_SESSION_STATE_LOSS_PENDING` and the
  session's frame calls return `XR_SESSION_LOSS_PENDING` until the app destroys it. An app that
  then makes a new session, as `hello_xr` does, streams again through the relaunched `tetherd`;
  one that ignores the loss keeps running without video. Keeping sessions across a restart is S5.
- `TETHER_INPROCESS=1` keeps the pre-R2 path for A/B comparisons until S5: the runtime binds the
  headset's port itself, so `tetherd` must be stopped first (`scripts/tetherd.sh stop`).
- `scripts/tetherd.sh` builds `tetherd`, signs a copy into `~/Library/Application Support/Tether/`,
  installs `~/Library/LaunchAgents/dev.tether.tetherd.plist` (`MachServices`, `KeepAlive`,
  `ProcessType Interactive`, `ThrottleInterval 1`, `LimitLoadToSessionType Aqua`) and logs to
  `~/Library/Logs/Tether/tetherd.log`. `scripts/run.sh` runs its `ensure` before every app, which
  restarts `tetherd` only when its binary or plist changed, so the headset's link survives
  relaunching an app.

## Repository layout

```
tether/
├── packages/rust/
│   ├── openxr-abi/     tether-openxr-abi    — OpenXR C types generated from the SDK headers
│   ├── openxr-gen/     tether-openxr-gen    — generates the runtime's dispatch table
│   ├── protocol/       tether-protocol      — wire protocol, shared with the Quest client
│   ├── stream/         tether-stream        — link, clock sync, poses, compositor, encoder
│   ├── runtime/        tether-runtime       — the runtime dylib the loader loads
│   ├── ipc/            tether-ipc           — runtime ↔ tetherd contract
│   ├── audiotap/       tether-audiotap      — Core Audio process-tap capture for tetherd
│   ├── playout/        tether-playout       — the headset's audio playout, host-testable
│   ├── fake-headset/   tether-fake-headset  — scripted headset client for tests
│   └── e2e/            tether-e2e           — the end-to-end check behind scripts/e2e.sh
├── apps/tetherd/       tetherd              — the per-user agent that owns the headset
├── apps/quest/         tether-quest         — the Quest client
├── examples/godot/     stock Godot projects, the developer-facing acceptance test
├── scripts/            SDK build, bindings, run, e2e, Quest packaging
├── third_party/        Khronos OpenXR-SDK-Source (pinned submodule)
└── docs/
```

The menu-bar app arrives in R7.

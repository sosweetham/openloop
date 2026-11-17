# Tether — Roadmap

| #      | Deliverable                                                                                                                         | Headset | Status |
| ------ | ----------------------------------------------------------------------------------------------------------------------------------- | ------- | ------ |
| **R0** | Workspace; Khronos loader and `hello_xr` (Metal) built; the runtime loads and `xrCreateInstance` succeeds                             | No      | Done |
| **R1** | Simulated headset: full API in-process, compositor shows both eyes in a Mac window, keyboard/mouse drive head and controllers; `hello_xr` and stock Godot (`examples/godot/hello`) end to end; CTS built with a core pass rate | No      | API done in-process; the Mac-window simulator and the CTS are not built |
| **R2** | `tetherd` out of process: NSXPC control plane, `xpc_shmem` data plane, `MTLSharedTextureHandle` swapchains, `MTLSharedEvent` ordering, the app's audio in the headset | Yes     | S2 built: `tetherd` owns the headset and streams the app's sound, checked end to end with a synthetic tone; the live Core Audio capture waits for the permission step at the Mac; S3 next |
| **R3** | Pure-Rust Quest client + USB: poses up, test pattern down, round-trip latency measured                                                | Yes     | Done, except that round-trip latency is not reported |
| **R4** | Streaming: VideoToolbox → MediaCodec, pose-tagged frames, frame pacing, recenter; Godot in the headset over USB-C                     | Yes     | Godot streams over USB-C, supersampled and encoded within the latency gate; pacing is not locked to the headset (R2 S4), and the session report measures only frame loss and frame-to-encode time |
| **R5** | Wi-Fi: discovery, FEC + retransmit, network-only adaptive bitrate                                                                   | Yes     | — |
| **R6** | Tuning to the quality gates below; full controller input and haptics                                                                | Yes     | — |
| **R7** | Menu-bar app, launchd agent, in-headset launcher, pairing, system runtime registration                                              | Yes     | — |

## R2 stages

Each stage ships a working build: `cargo test` passes, `scripts/e2e.sh` is green, and
`hello_xr` and `examples/godot/hello` stream in the headset. Design:
[`ARCHITECTURE.md`](ARCHITECTURE.md) § Runtime ↔ tetherd.

| Stage  | Delivers | Quest APK |
| ------ | -------- | --------- |
| **S0** | Seams and test infrastructure, still in-process: `TETHER_USB_PORT` and runtime unit tests that never listen; `tether-ipc` (wait-free records, SPSC rings, cross-process wait and wake, the NSXPC protocol); `tether-fake-headset`; `scripts/e2e.sh` | required once: protocol 2, under which unknown message kinds are skipped and appended fields default, so later stages need no lockstep update |
| **S1** | `tetherd` owns the headset: link, adb tunnel, clock sync, tracking, profile, recenter, focus, haptics, session report. The app still composites and encodes and hands access units to `tetherd` through a shared-memory ring. The link survives the app, and a second app gets the headset | shows its idle state on `StreamEnded` |
| **S2** | Audio: `tetherd` taps the foreground app with Core Audio, muted on the Mac while it plays in the headset; the Quest plays it by presentation time | plays `Audio` with AAudio |
| **S3** | The GPU path moves into `tetherd`: shared textures and shared events, composition on `tetherd`'s own queue, encoding in `tetherd`; the runtime drops VideoToolbox | — |
| **S4** | `xrWaitFrame` locked to the headset's vsync, shifted by measured latency; the pacing gate measured | sends display timing |
| **S5** | Hardening: app crash and `tetherd` restart, several apps, real-time threads, the in-process path deleted | — |
| **S6** | Packaging with R7: the menu-bar app registers the agent; a per-user active runtime | — |

## Quality gates

A milestone that streams is done when its session report passes these, not when it looks right.

| Gate                                                     | Limit                                              | Milestone |
| -------------------------------------------------------- | -------------------------------------------------- | --------- |
| Keyframe requests over USB, 5-minute session             | 0                                                  | R4        |
| Frames dropped before encode, USB, 90 Hz, steady state   | 0                                                  | R4        |
| Encode latency p95                                       | under one frame period; target single-digit ms     | R4        |
| Frame-pacing stddev                                      | under 0.5 ms, no interval over 2× the period       | R4        |
| USB bitrate                                              | never reduced                                      | R4        |
| Meta-button recenter                                     | view recentres; Godot receives `pose_recentered`   | R4        |
| Controller motion-to-photon                              | measured and reported every session                | R6        |
| Encoded frames discarded by the client decoder           | 0; a frame waits for a decoder buffer or the stream skips to the next keyframe | R4 |
| Audio dropouts over USB, 5-minute session                | 0; any A/V offset Tether adds is reported          | R2        |

`packages/rust/stream/tests/encode_latency.rs` holds the encode-latency gate on this Mac's media
engine, on demand (`cargo test -p tether-stream --release --test encode_latency -- --ignored
composed_frames`): 20 s of a moving scene rendered at the default render scale, filtered down and
encoded at the USB bitrate, with no frame dropped before encode.
`tether-fake-headset` stands in for the headset in automated tests and can stall its reads,
request keyframes and drift its clock, which is how decoder backpressure, encoder stalls and
a lossless link are reproduced; `scripts/e2e.sh` runs its steady stream in-process and through
`tetherd`, where one app is killed mid-stream and a second takes over the same link. The
remaining gates come from a scripted session with the headset.

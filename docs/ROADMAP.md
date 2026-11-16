# Tether — Roadmap

| #      | Deliverable                                                                                                                         | Headset |
| ------ | ----------------------------------------------------------------------------------------------------------------------------------- | ------- |
| **R0** | Workspace; Khronos loader and `hello_xr` (Metal) built; the runtime loads and `xrCreateInstance` succeeds                             | No      |
| **R1** | Simulated headset: full API in-process, compositor shows both eyes in a Mac window, keyboard/mouse drive head and controllers; `hello_xr` and stock Godot (`examples/godot/hello`) end to end; CTS built with a core pass rate | No      |
| **R2** | `tetherd` out of process: Mach IPC, IOSurface swapchains, shared events                                                              | No      |
| **R3** | Pure-Rust Quest client + USB: poses up, test pattern down, round-trip latency measured                                                | Yes     |
| **R4** | Streaming: VideoToolbox → MediaCodec, pose-tagged frames, frame pacing, recenter; Godot in the headset over USB-C                     | Yes     |
| **R5** | Wi-Fi: discovery, FEC + retransmit, network-only adaptive bitrate                                                                   | Yes     |
| **R6** | Tuning to the quality gates below; full controller input and haptics                                                                | Yes     |
| **R7** | Menu-bar app, launchd agent, in-headset launcher, pairing, system runtime registration                                              | Yes     |

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

Decoder backpressure, encoder stalls and a lossless link are reproduced by a fake headset
client in automated tests; the remaining gates come from a scripted headset session.

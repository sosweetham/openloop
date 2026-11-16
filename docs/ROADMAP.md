# Tether — Roadmap

| #      | Deliverable                                                                                                                         | Headset |
| ------ | ----------------------------------------------------------------------------------------------------------------------------------- | ------- |
| **R0** | Workspace; Khronos loader, `hello_xr` (Metal) and the CTS built; the runtime loads and `xrCreateInstance` succeeds                    | No      |
| **R1** | Simulated headset: full API in-process, compositor shows both eyes in a Mac window, keyboard/mouse drive head and controllers; `hello_xr` end to end; core CTS pass rate | No      |
| **R2** | `tetherd` out of process: Mach IPC, IOSurface swapchains, shared events, menu-bar app                                                | No      |
| **R3** | Quest client + USB spike (AOA vs native ADB): poses up, test pattern down, round-trip latency measured                                | Yes     |
| **R4** | Streaming: VideoToolbox → MediaCodec, pose-tagged frames, frame pacing; `hello_xr` in the headset over USB-C                          | Yes     |
| **R5** | Wi-Fi: Bonjour, FEC + retransmit, adaptive bitrate                                                                                  | Yes     |
| **R6** | Hardening: full controller input and haptics, latency tuning, CTS pass rate, Godot/Qt apps as real-world tests                        | Yes     |

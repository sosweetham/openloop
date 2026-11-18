# Contributing to OpenLoop

Thank you for helping. OpenLoop is early, so the most useful contributions right now are test
reports from hardware other than the one setup it has run on (see
[Reporting hardware test results](#reporting-hardware-test-results)), bug reports and fixes.
For anything large, such as a new host, a new headset client or a change to the wire protocol,
please open an issue first so the design can be agreed before the code.

## How OpenLoop is built: people and AI agents

OpenLoop is built by a human maintainer who directs AI coding agents and reviews their work. The
maintainer decides what is built, reviews every change and is the author of every commit.
Contributions are welcome from people working with or without AI tools, and the same rules
apply either way.

- **[`AI.md`](AI.md) is the rulebook.** It is the single source of truth that every agent
  follows. `CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `CONVENTIONS.md`, `.cursorrules`,
  `.clinerules`, `.windsurfrules`, `.roorules` and `.rules` are symlinks to it, so most coding
  agents read it without being told. If yours does not, give it `AI.md` yourself. Its rules
  apply to human contributors too.
- **Rules change in `AI.md` and nowhere else.** Do not replace a symlink with a copy or put
  instructions in a tool-specific file. Propose a change to `AI.md` in a pull request of its
  own.
- **You own what you submit.** Whatever tools you used, you are the author of your change.
  Read every line of the diff, run the checks below yourself, and be ready to explain any part
  of it in review. Do not submit agent output you have not reviewed.
- **AI is never attributed in commits.** The human author owns every commit (see
  [Commits and pull requests](#commits-and-pull-requests)).

## The rules in AI.md, in short

`AI.md` has the full text. Where this summary and `AI.md` differ, `AI.md` wins.

- **Toolchain.** Build C, C++ and Metal against Xcode, never the Command Line Tools. A new
  native build script pins `DEVELOPER_DIR` and the sysroot, as `scripts/openxr-sdk.sh` does.
  The Metal toolchain is a separate Xcode component
  (`xcodebuild -downloadComponent MetalToolchain`).
- **Real-time paths.** Frame pacing, pose handling, compositor submission and packet send and
  receive run on real-time threads, and a real-time thread never allocates, locks or blocks.
  Pre-allocate at session start, and hand data between threads through fixed-capacity lock-free
  rings or atomics. No logging formatters or `String` building on those threads. Budgets come
  from the headset's refresh rate, never from constants. Times are `XrTime` nanoseconds in the
  Mac's monotonic clock unless the type says otherwise.
- **Comments.** The code says what it does; a comment says only what the code cannot: a
  platform bug and its workaround, an invariant the caller must uphold, the OpenXR spec clause
  being satisfied, the safety argument for an `unsafe` block, or a `TODO` with an owner. A
  comment describes the code, never its history.
- **One Objective-C file.** `packages/rust/ipc/objc/OpenLoopIPC.m` declares the NSXPC protocol
  and implements nothing. No other Objective-C enters the tree.
- **Docs of record.** The design lives in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and
  the milestones in [`docs/ROADMAP.md`](docs/ROADMAP.md), not in `AI.md`. When a change alters
  the design or the status, update them in the same change.

## Verification

Before you open a pull request, run what applies to your change. In the pull request, say what
you ran, on which hardware, and what you could not run.

| Your change touches | Run |
| --- | --- |
| Anything | `cargo test` |
| The runtime, IPC, streaming, protocol, openloopd or the test harness | `./scripts/e2e.sh`, both modes. Without a signing certificate, run `./scripts/e2e.sh --inprocess` and say so |
| The compositor, the encoder or the video settings | `cargo test -p openloop-stream --release --test encode_latency -- --ignored composed_frames` |
| What the headset shows, plays or sends back, or the Quest app | A session in the headset with `hello_xr` and `examples/godot/hello`, and its session report from `~/Library/Logs/OpenLoop/sessions/` checked against the [quality gates](docs/ROADMAP.md#quality-gates) |
| The OpenXR SDK version or `packages/rust/runtime/extensions.txt` | `./scripts/openxr-abi.sh` (needs `bindgen-cli`), and commit the regenerated files |

The rules that go with these checks:

- **Streaming work is done when its session report passes the quality gates**, not when it
  looks right.
- **Every change ships a working build**: `cargo test` passes, `scripts/e2e.sh` is green, and
  `hello_xr` and `examples/godot/hello` stream in the headset.
- **Automated tests never touch real hardware or the user's setup.** `cargo test` and
  `scripts/e2e.sh` never touch a headset, adb, port 47800, the installed openloopd, Core Audio or
  a running app. Keep it that way: tests use `OPENLOOP_USB_PORT=0`, `OPENLOOP_SERVICE` and
  `openloop-fake-headset`.
- **Generated files are never edited by hand.** `packages/rust/openxr-abi/src/bindings.rs` and
  `packages/rust/runtime/src/generated.rs` come from `scripts/openxr-abi.sh`.
- **Core Audio needs care.** Never make a process-tap call while a permission prompt may be
  pending: that once left the Mac's audio stuck for hours. The permission is requested only
  through `scripts/openloopd.sh audio-permission`, with a person at the Mac.
- **The wire protocol grows compatibly.** Under protocol 2, unknown message kinds are skipped
  and appended fields take their defaults. Add message kinds or append fields; do not change
  existing ones.

## Commits and pull requests

- **The human author owns every commit.** Do not add `Co-Authored-By` lines for any AI tool, do
  not name AI tools (Claude, Codex, Gemini, Copilot or any other) in commit messages, and do not
  add "Generated with AI" trailers.
- **Commit messages** say what the change does in plain words, as the history does ("Keep the
  headset's USB tunnel up while an app runs"). The body says why, and what you checked.
- **Use an email address you are happy to publish**, such as your GitHub noreply address. It
  stays in the history.
- **No personal data in the tree**: no signing identities, team IDs, email addresses,
  home-directory paths, device serial numbers or keystores. Values like these come from the
  environment, as `OPENLOOP_SIGN_IDENTITY` does.
- **One topic per pull request**, with the docs updated in the same pull request.

## Reporting hardware test results

Reports from any setup other than the tested one (an M3 Max MacBook Pro on macOS 27.0.1 with a
Quest 3) are welcome, including reports that everything worked. Open an issue and include:

```text
Mac:        model identifier, chip, memory    sysctl -n hw.model machdep.cpu.brand_string hw.memsize
macOS:      version                           sw_vers
Headset:    model and system software version (from the headset's settings)
OpenLoop:     commit                            git rev-parse --short HEAD
Toolchain:  Xcode, Rust and NDK versions      xcodebuild -version; rustc --version
What ran:   cargo test, scripts/e2e.sh, hello_xr, examples/godot/hello, your own app
Result:     what worked, what did not, and what you saw and heard in the headset
```

Attach what applies:

- the output of `./scripts/e2e.sh`, or of `./scripts/e2e.sh --inprocess`;
- on a Mac other than an M3 Max, the encoder measurement:
  `cargo test -p openloop-stream --release --test encode_latency -- --ignored composed_frames --nocapture`;
- for a headset session, its report from `~/Library/Logs/OpenLoop/sessions/`, the matching part
  of `~/Library/Logs/OpenLoop/openloopd.log`, and the Quest app's log (`adb logcat -s OpenLoop`).

Read logs before you post them. They contain app names and file paths, and paths can include
your user name.

## Licensing of contributions

OpenLoop is licensed under either the MIT licence or the Apache License 2.0, at your option (see
[`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE)).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.

- **Port ideas, not files.** Implementing a documented technique or a published formula is
  fine. Translating another project's source is not, unless its licence allows it and the
  result is listed in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). This applies to code
  an agent writes for you too.
- **Copyleft and closed code are references only.** GPL, LGPL, MPL and other copyleft projects,
  and closed SDKs, runtimes and drivers, may be read for documented behaviour, protocols and
  formats. Never copy or translate their code, and never decompile or disassemble software to
  write OpenLoop's. Cite public sources: a specification section, a documented API or a URL with
  a commit. If you are not sure whether a source may be used, ask in an issue first.
- **New dependencies** must be under licences compatible with both of OpenLoop's: today MIT,
  Apache-2.0, Zlib, ISC, BSD-3-Clause, Unlicense, Unicode-3.0 and BSL-1.0. Update
  `THIRD_PARTY_NOTICES.md` when the set of crates in a shipped binary changes.

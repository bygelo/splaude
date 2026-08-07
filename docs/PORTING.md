# Porting splaude off macOS

The shipping app is the Swift build in `Source/`. It is macOS-only, and not
incidentally so: almost everything it does is an operating-system integration
point. `Crate/` holds an in-progress Rust workspace that targets Windows, Linux
and macOS from one codebase.

**Status: the Rust workspace does not yet produce a usable app.** The portable
half is ported and tested; the OS bindings and the interface are not written.
Use the Swift build.

## Why a rewrite rather than a port

The Swift sources are about 2,900 lines, and the split is lopsided in an
awkward direction:

| | Lines | What it is |
| --- | --- | --- |
| Portable | ~470 | HTTP, JSON, diffing, bookkeeping — imports `Foundation` and nothing else |
| macOS-locked | ~2,400 | Accessibility, Carbon, `CGEvent`, `AVFoundation`, AppKit, Keychain |

The macOS-locked part is not user interface that could be re-skinned. It is
seven distinct system-integration primitives, each of which is a different API
on every platform:

| Concern | Swift | macOS API |
| --- | --- | --- |
| Global hotkey | `Hotkey` | Carbon `RegisterEventHotKey` |
| Find the focused field | `FocusProbe`, `FocusAnchor` | `AXUIElement` |
| Type at the cursor | `TextInserter`, `LiveTyper` | `CGEvent` |
| Capture the microphone | `AudioCapture` | `AVAudioEngine` |
| Menu bar and floating mic | `AppDelegate`, `FloatingMic` | `NSStatusItem`, `NSPanel` |
| Store the credential | `TokenStore` | Keychain |
| Launch at login | `Setting` | `ServiceManagement` |

Swift runs on Windows, but AppKit and Carbon do not, so keeping Swift would
mean hand-writing a Win32 and a GTK layer and maintaining three interfaces.
Rust was chosen because mature crates already cover most of these on all three
platforms, which collapses the work rather than tripling it.

## Shape

```
Crate/core      portable — no OS integration point, compiles anywhere
Crate/platform  the trait set each OS implements, plus the resampler
Crate/app       wiring: tray, floating mic, settings, take lifecycle
```

`Crate/app` never learns which OS it is running on. It talks to the traits in
`Crate/platform`, and `Crate/core` talks to nothing but the network.

The pleasant surprise: three of the six platform concerns need only **one**
implementation, because a cross-platform crate already covers them everywhere.

| Concern | Backend | Windows | macOS | Linux |
| --- | --- | --- | --- | --- |
| Audio capture | `cpal` | WASAPI | CoreAudio | ALSA / PipeWire |
| Global hotkey | `global-hotkey` | ✓ | ✓ | X11 only |
| Text injection | `enigo` | SendInput | `CGEvent` | XTest / uinput |
| Focus guard | this repo | UI Automation | `AXUIElement` | none |
| Autostart | this repo | registry `Run` key | `SMAppService` | XDG autostart |
| Credential | this repo | file | Keychain, then file | file, or secret service |

Credential loading got *simpler* off macOS. `TokenStore` already fell back to
`~/.claude/.credentials.json`, and that file is exactly where Claude Code keeps
the credential on Windows and Linux — so the fallback path became the main one,
and only macOS needs a secret-store implementation.

## What Wayland will not do

Wayland denies both of this app's core gestures to a background client: it
cannot register a global hotkey, and it cannot synthesise input into another
client. That is the security model working as designed, not a gap to route
around.

So the Linux build targets X11 and XWayland for full function. On a Wayland
session it reports reduced capability at startup — see `Capability` in
`Crate/platform/src/lib.rs` — rather than accepting a hotkey that will never
fire and producing silence.

## Why there is no mobile target

iOS and Android cannot do the thing this app is. Neither OS lets a background
process inject text into another app's field, at any permission level. The only
sanctioned route is shipping a custom keyboard — an iOS keyboard extension or
an Android IME — which is a different product with a different interaction
model, not a port of this one. It is out of scope.

## Done

- `Crate/core` (70 tests) — the speech backend with its wire contract
  preserved verbatim, credential loading and health classification, settings,
  the transcript buffer, keyterm packing, quota inspection, diagnostics, and
  the live-typing diff extracted as pure logic. Ten of those drive the socket
  loop against a local WebSocket server rather than Anthropic, which is what
  caught a hand-built upgrade request that would have failed every connection.
- `Crate/platform` (10 tests) — the trait set, and a windowed-sinc resampler
  that turns whatever the input device offers into 16 kHz mono signed-16 PCM.
  `AVAudioConverter` has no portable equivalent, so it is hand-rolled; filter
  state carries across buffers, because resetting per callback clicks.

## Not done

- Every OS binding: audio capture wiring, hotkey listener, text injector, focus
  guard (×3), autostart (×3), macOS secret-store credential source.
- The entire interface: tray icon, floating mic, settings window.
- `Crate/app` is an empty `main`.
- Packaging for three platforms. CI checks all three, but the release workflow
  still builds and publishes the macOS Swift bundle only, on tag.
- One inherited bug is carried deliberately rather than fixed under cover of a
  rewrite — see *Known issues* in [CHANGELOG.md](../CHANGELOG.md).

## Building it

Requires Rust 1.90 or newer. On Windows the MSVC linker comes from Visual
Studio Build Tools; cargo finds it without a developer prompt.

```sh
cargo test --all        # 80 tests across both crates
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo build --release   # builds, but Crate/app does nothing yet
```

The Swift build is untouched by any of this and still builds with `make`, and
now has 23 tests of its own via `swift test`. Both suites run on every push and
pull request through the `Check` workflow, across Linux, macOS and Windows —
all four legs green, so the workspace is confirmed to build and pass on every
target platform even though only the macOS app is usable yet.

# Porting splaude off macOS

The shipping app is the Swift build in `Source/`. It is macOS-only, and not
incidentally so: almost everything it does is an operating-system integration
point. `Crate/` holds an in-progress Rust workspace that targets Windows, Linux
and macOS from one codebase.

**Status: the Rust workspace builds a working `splaude` binary, but its
dictation path has never been exercised on real hardware.** The portable core
and every OS binding are implemented and tested; the interface (tray, floating
mic, settings window) is not. On macOS, use the Swift build.

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

## Direction

Desktop first: macOS, Windows and Linux from this workspace, staying local and
credential-free in the sense that matters — audio goes to the endpoint, text
goes to your cursor, nothing is stored and nothing is uploaded.

A phone or web client is deliberately deferred rather than ruled out, but it
would be a different product sharing this one's core, and it runs into a
blocker worth stating before anyone starts: splaude authenticates by reading
the Claude Code OAuth credential already on the machine. There is no Claude
Code on a phone and none in a browser, so that model does not extend. Getting
speech-to-text there means either a real provider key of your own — which is
what `SpeechBackend` being a three-method protocol is for — or routing many
devices through one credential on an undocumented internal endpoint, which is a
far larger bet than a local tool and is not a foundation to build a synced
service on.

Syncing would also make this a product that stores dictation history on a
server, which today it deliberately does not.

## Why there is no mobile target

iOS and Android cannot do the thing this app is. Neither OS lets a background
process inject text into another app's field, at any permission level. The only
sanctioned route is shipping a custom keyboard — an iOS keyboard extension or
an Android IME — which is a different product with a different interaction
model, not a port of this one. It is out of scope.

## Done

- `Crate/core` (97 tests) — the speech backend with its wire contract
  preserved verbatim, credential loading and health classification, settings,
  the transcript buffer, keyterm packing, quota inspection, diagnostics, and
  the live-typing diff extracted as pure logic. Ten of those drive the socket
  loop against a local WebSocket server rather than Anthropic, which is what
  caught a hand-built upgrade request that would have failed every connection.
- `Crate/platform` (87 tests) — every OS binding. Audio capture on `cpal` with a
  hand-rolled windowed-sinc resampler standing in for `AVAudioConverter`; the
  push-to-talk hotkey on `global-hotkey`, reporting both edges; text injection
  on `enigo`; the focus guard; launch at login. Only the last two needed
  per-OS code.
- `Crate/app` (33 tests) — a binary on a `tao` main-thread event loop.
  `splaude` runs the dictation loop and lives in the tray; `splaude --check`
  reports credential, capability and settings state without opening a window, a
  microphone or a loop, so it is safe over SSH and in CI.
- The tray, on Windows and macOS: the app mark drawn in code, a face that
  doubles as the input level meter, and a menu carrying credential health, the
  last transcript (click to copy), Reveal Log, launch at login, and Quit. Quit
  is the only orderly shutdown path — `Ctrl+C` never reaches `LoopDestroyed`,
  so the hotkey stays registered with the OS and that chord is dead for every
  other app until the session ends.
- Behaviour parity with the Mac app: tap-to-latch at the same 400 ms threshold,
  Return ending a take, a start/stop tone, the Test Paste diagnostic, quota
  reported rather than merely logged, and the settings file opened and reloaded
  from the menu.
- Packaging. The release workflow publishes a Windows `.exe` and a Linux
  `.tar.gz` alongside the macOS bundle, on tag.

## Verified, and not

**Windows dictates end to end.** Hold the key, talk, release, and the text
lands at the cursor in a real editor — hotkey, microphone, socket against the
live endpoint, live typing, the credential read out of
`~/.claude/.credentials.json`. The tray renders and its menu works. CI compiles
the whole workspace on Linux, macOS and Windows.

That matters more for what it taught than for the milestone. **Every
user-visible bug this port has had came from that use, and not one was
reachable from the suite** — 217 green tests did not catch a held modifier
leaking spaces into a sentence, `Alt+Backspace` undoing each live-typing
correction, a remote desktop turning a take into a run of `a`, a byte-order
mark silently discarding the settings file, or a short take losing its tail.
Each was found by a person holding a key, and each is fixed. Read the suite as
protection against regression, not as evidence anything works.

Still unproven, and needing a person rather than a runner:

- **Linux and macOS have never been launched.** Both compile in CI. Nobody has
  pressed the key on either.
- **Several Windows surfaces have never been exercised**: both tap gestures,
  the Return hook, Test Paste, the tone, the quota line, Reload Settings, and
  what a refused rebind looks like from the front. The level meter moving, the
  transcript item copying, and launch at login surviving a restart are equally
  unconfirmed.
- **Whether a modifier can be part of a Windows binding at all.** The default
  is now a bare `F9` precisely because both ways of handling a held modifier
  were tried against a live take and observed failing. A user who sets a
  modified binding still meets the problem.

The macOS hotkey blocker recorded here previously is **resolved**. It was never
a platform problem: `global-hotkey` wants its manager on the main thread on
macOS and pinned to the thread owning its hidden window on Windows, and the
listener spawned its own thread only because the app had no event loop to
borrow. Introducing one satisfies both, and CI now compiles the result on
`macos-15`.

## Not done

- The floating mic button and the settings window. The menu now opens and
  reloads the JSON file at `Setting::path()`, which covers most of what a
  window would have given — but a real one needs a widget toolkit, and `tao`
  supplies a window, not controls. Every candidate wants the main-thread event
  loop `tao` already owns, so this is a migration on its own branch, not a
  feature. It remains the largest open dependency question in the project.
- No tray on Linux, deliberately. `tray-icon` there is a hard GTK3 and
  libappindicator dependency that still renders nothing on a desktop with no
  appindicator host, which stock GNOME is.
- macOS secret-store credential source. The file fallback covers Windows and
  Linux completely and most macOS installs.
- On Windows, splaude will physically press Return if a transcript ever
  contains a newline — `enigo` special-cases `\n` with a real key click instead
  of a unicode payload. Latent, and not fixed.
- One inherited bug is carried deliberately rather than fixed under cover of a
  rewrite — see *Known issues* in [CHANGELOG.md](../CHANGELOG.md).

## Building it

Requires Rust 1.90 or newer. On Windows the MSVC linker comes from Visual
Studio Build Tools; cargo finds it without a developer prompt.

Linux needs headers the runner images do not ship: ALSA for `cpal`, libxdo for
`enigo`, the X11 and xkb headers for the hotkey listener, and **GTK3 and D-Bus
for `tao`**. That last pair is worth stating plainly, because it is the opposite
of what the manifest implies — `tray-icon` is excluded on Linux specifically to
keep GTK out, and then the event loop brings it anyway. The exclusion still
avoids libappindicator, but the heavy dependency it was written to prevent
arrives through `tao`. Whether Linux needs the event loop at all is open:
`global-hotkey` spins its own thread on X11, so the pump is a Windows
requirement and the main run loop a macOS one.

```sh
sudo apt install pkg-config libasound2-dev libxdo-dev libxkbcommon-dev \
  libx11-dev libgtk-3-dev libdbus-1-dev
```

```sh
cargo test --all        # 151 tests across the workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo run -p splaude-app -- --check    # credential and capability report
cargo build --release                  # target/release/splaude
```

The Swift build still builds with `make` and now has 32 tests of its own via
`swift test`. It is mostly untouched by the port, the exception being fixes
that apply to both builds — the remote-desktop paste carve-out is in each.
Both suites run on every push and pull request through the `Check` workflow,
across Linux, macOS and Windows, all four legs green.

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

## What the Swift build is for

One product is implemented twice, and the cost stopped being theoretical: the
remote-desktop paste fix had to be written once in
`Source/splaude/TextInserter.swift` and again in
`Crate/platform/src/injector.rs`, and the Windows floating button shipped
without an off-screen guard the Swift one has had since it was written. Nothing
asserts the two behave alike, so drift is found by users.

**Today, a macOS user should run the Swift app**, and the reasons are specific
rather than sentimental. Against the Rust build on macOS it is the only one
that:

- reads the credential from the Keychain, not just the file fallback;
- has a code identity of its own, so Accessibility and Microphone are granted to
  *splaude* rather than to whichever terminal launched it;
- can tell a text field from a button — `focus::is_supported()` answers `false`
  on macOS, so the Rust build's focus guard is permanently `Unknown`;
- has ever been launched by a person.

**The condition for retiring it** is that list, closed. When the Rust build on
macOS reads the Keychain, ships as a signed `.app` bundle with its own TCC
identity, implements the focus probe against `AXUIElement`, and has been used
for a week of real dictation by somebody, then two codebases stop being worth
their drift and `Source/` goes. Until every one of those is true, it stays, and
the fixes that apply to both keep being written twice — deliberately, and with
the duplication named in the commit rather than discovered later.

This is written down because the alternative is what was happening: no stated
condition, so every fix silently re-opened the question and nobody answered it.

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

- `Crate/core` (144 tests) — the speech backend with its wire contract
  preserved verbatim, credential loading and health classification, settings,
  the transcript buffer, keyterm packing, quota inspection, diagnostics, and
  the live-typing diff extracted as pure logic. Ten of those drive the socket
  loop against a local WebSocket server rather than Anthropic, which is what
  caught a hand-built upgrade request that would have failed every connection.
- `Crate/platform` (104 tests, plus 6 quarantined) — every OS binding. Audio capture on `cpal` with a
  hand-rolled windowed-sinc resampler standing in for `AVAudioConverter`; the
  push-to-talk hotkey on `global-hotkey`, reporting both edges; text injection
  on `enigo`; the focus guard; launch at login. Only the last two needed
  per-OS code.
- `Crate/app` (70 tests) — a binary on a `tao` main-thread event loop.
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
- Interface parity with the Mac app, on Windows: a settings window with the same
  four groups and the same words, a floating mic button, Start/Stop Dictation in
  the menu, and a credential warning that puts its own fix on the clipboard.

  Both windows are **hand-written Win32**, which settles the dependency question
  this document had left open. The problem was never that a toolkit could not
  draw a checkbox; it was that every candidate wants to own the main-thread
  event loop, and `tao` already owns it for the hotkey pump — so adopting one
  would have been a migration of the whole app. Win32 makes no such claim: a
  window on a thread of its own has its own message queue, and `windows` was
  already in the graph through `splaude-platform`. Neither window shares state
  with the event loop. The settings window writes the file and says so, and the
  loop re-reads it through the same `reload` the menu item uses; the floating
  button posts what it was asked for and is told the mood by a posted message.
  One reader, one writer, nothing to drift.
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
reachable from the suite** — 217 green tests, as the suite stood then, did
not catch a held modifier
leaking spaces into a sentence, `Alt+Backspace` undoing each live-typing
correction, a remote desktop turning a take into a run of `a`, a byte-order
mark silently discarding the settings file, or a short take losing its tail.
Each was found by a person holding a key, and each is fixed. Read the suite as
protection against regression, not as evidence anything works.

That reading is what `Crate/platform/tests/injection.rs` was written against.
Three of those bugs were in one subsystem — synthetic input — which had thirteen
tests, every one of them pure, while the socket that had never broken in the
field carried 687 lines of integration test. The coverage was inverted with
respect to the risk. There is now a harness that owns a real window and reads
the characters back out of it, and it has been shown to go red on two of the
exact regressions that shipped.

Still unproven, and needing a person rather than a runner:

- **Linux and macOS have never been launched.** Both compile in CI. Nobody has
  pressed the key on either.
- **Several Windows surfaces have never been exercised**: both tap gestures,
  the Return hook, Test Paste, the tone, the quota line, Reload Settings, and
  what a refused rebind looks like from the front. The level meter moving, the
  transcript item copying, and launch at login surviving a restart are equally
  unconfirmed.
- **The settings window and the floating button were driven, but not by a
  person.** Both were built, launched and worked on Windows 11: all four pages
  render, a checkbox writes the file and the file round-trips byte for byte, the
  window closes cleanly, the button draws with its corners genuinely
  transparent, a click reaches the take path, and a drag moves it by the pointer
  delta and saves where it landed. That was done by posting messages at them
  from another process, which is a real exercise of the code and *not* a
  substitute for a hand on a mouse: nothing here has been checked at a display
  scale other than 100%, in a high-contrast theme, or against a second monitor.
- **No take has been started on this machine at all**, so the mood the button
  and the tray show while recording is drawn but unseen. The reason is worth
  recording because it is the same class of problem the port keeps finding: the
  session it was tested in is a Remote Desktop session with no audio redirected,
  so `cpal` reports no input device and every take fails at the microphone. That
  failure used to go only to stderr, which a tray app has nobody to show; it is
  now in the log.
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

What is left, in priority order with a test attached to each, lives in
[ROADMAP.md](../ROADMAP.md); what was considered and refused is in
[roadmap-rejected.md](./roadmap-rejected.md). This section is the shape of the
port, not the work queue.

- macOS and Linux have no settings window and no floating mic button. Both are
  Win32 (see below), which is a decision about *this* platform rather than a
  toolkit for all three. macOS has the Swift build's own; Linux has no tray to
  open one from and, on Wayland, no permission to place a window above other
  clients.
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
cargo test --all        # 318 tests across the workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo run -p splaude-app -- --check    # credential and capability report
cargo build --release                  # target/release/splaude
```

The Swift build still builds with `make` and has 54 tests of its own via
`swift test`. Most of the port does not touch it, the exceptions being changes
that apply to both builds — the remote-desktop paste carve-out and the
project-aware recogniser bias are in each.
Both suites run on every push and pull request through the `Check` workflow,
across Linux, macOS and Windows, all four legs green.

# Changelog

Notable changes to splaude. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Warn about the credential before a take fails on it. splaude reads the Claude
  Code OAuth token but never refreshes it, so an install that is never opened
  alongside `claude` eventually finds it dead. The menu now warns within ten
  minutes of expiry and again once expired, rather than letting it surface as a
  dictation that mysteriously produces nothing.
- Cross-platform Rust workspace under `Crate/`, targeting Windows, Linux and
  macOS from one codebase. Not yet runnable — see [docs/PORTING.md](docs/PORTING.md)
  for what is done and what is not. The shipping app is still the Swift build.
- `Crate/core` — the portable half, ported from Swift with 60 tests: the
  Anthropic speech backend (endpoint, query parameters, headers, keyterm
  packing, keepalive and close framing all preserved verbatim), credential
  loading, settings, transcript buffering, quota inspection and diagnostics.
- `Crate/platform` — every OS binding, 63 tests. Audio capture on `cpal` with a
  hand-rolled 16 kHz resampler replacing `AVAudioConverter`; the push-to-talk
  hotkey on `global-hotkey`, reporting press and release rather than a tap;
  text injection on `enigo`, layout-blind and defended against the held
  push-to-talk modifier turning a backspace into a word delete; the focus
  guard; launch at login. Only the last two needed per-OS code.
- `Crate/app` — a binary at last. `splaude` runs the dictation loop; `splaude
  --check` reports credential, capability and settings state without opening a
  window or a microphone, so it is safe over SSH and in CI. Confirmed on
  Windows reading a real Claude Code credential.
- A test target for the macOS app, which previously had none: 23 tests over
  transcript bookkeeping, keyterm packing and credential-expiry classification.
  It tests the executable target directly, so the app did not have to be split
  into a library and have its internals marked `public` to be visible.
- Integration tests that drive the Rust speech backend's socket loop against a
  local WebSocket server — the keepalive on connect, audio framing, CloseStream
  and the close grace, text the server never endpointed, and what a 4xx on the
  upgrade does to the cached credential. Nothing in the suite reaches Anthropic.
- `Check` workflow running formatting, clippy and tests on Linux, macOS and
  Windows, plus the Swift build, test, bundle and headless smoke check. Until
  now the only workflow ran on a tag, so nothing was verified until release.
  Green on all four legs on its first run.
- A `tao` main-thread event loop in `Crate/app`, replacing the blocking channel
  the app used to sit on. This is what unblocks macOS: `global-hotkey` wants
  its manager on the main thread there and pinned to the thread owning its
  hidden window on Windows, and the listener only spawned threads because there
  was no loop to borrow. It now owns none.
- A tray icon on Windows and macOS, drawn in code from the same proportions as
  `Script/makeicon.swift` so both builds show the same mark. The face doubles
  as the input level meter, filling bottom-up across five steps — a silent take
  reads as recording first and quiet second, so a dead microphone is visible at
  a glance rather than after a take that produced nothing.
- Tray menu: credential health (rendered only when there is something to say),
  the last transcript with click-to-copy, Reveal Log, launch at login, and
  Quit — which is the only orderly shutdown path, and therefore the only one
  that unregisters the hotkey. Every one of these was already implemented in
  `core` or `platform` and had no caller; the work was wiring.

### Fixed

- The Rust speech backend built its upgrade request by hand, which skipped
  `Sec-WebSocket-Key`, `Upgrade` and `Connection`, so every connection would
  have been refused with what looked like a network failure. Caught by the new
  socket tests before the port ever ran. Never shipped — the released app is
  the Swift build, which was unaffected.

### Changed

- Settings move from `UserDefaults` to a JSON file in the platform config
  directory. The property that mattered is kept: the file and the settings
  window remain two views of one thing, and hand-editing is still supported.
  Only the Rust build reads this; the Swift build still uses `defaults`.
- The push-to-talk binding is stored as a portable `Alt+Space`-style string
  rather than a Carbon virtual keycode, which is a macOS integer with no
  meaning on another platform.
- The live-typing diff moves into `Crate/core` as pure logic. It never needed
  an OS — it was portable code sitting in a file that also posted `CGEvent`s.

### Known issues

- The live-typing model can disagree with the screen. When the locked-prefix
  rule holds back a deletion the diff asked for, the bookkeeping rebuilds its
  copy from the *target's* prefix rather than the characters actually left in
  place, so a later diff measures against the wrong baseline. Present in the
  shipping Swift build (`LiveTyper.update`) and carried into the Rust port
  deliberately rather than changed under cover of a rewrite; the Rust test
  `a_longer_contradicting_target_may_only_append_past_the_lock` pins the
  current behaviour and names it as a divergence.
- The dictation loop has never run. `splaude --check` is confirmed against a
  real machine; the run path is not. No hotkey has been pressed, no socket
  opened against the endpoint, no microphone captured — so the OS integration
  is verified by the compiler and by platform documentation, not by use.
- The injector's held-modifier defence asserts a modifier key-up before every
  synthetic event on Windows and Linux, because neither exposes a per-event
  modifier mask. A hotkey layer that polls key state may therefore see the
  chord end early, and a lone Alt key-up can activate a Windows menu bar. Both
  beat deleting the user's words a word at a time; neither has been observed.
- The tray renders, but nothing beyond that is confirmed: not the level meter
  moving, not the transcript item copying, not Reveal Log landing on the right
  file, not launch at login surviving a restart.
- `tao` puts GTK3 and D-Bus on Linux, which is the dependency `tray-icon` is
  excluded there to avoid. Whether Linux needs the event loop at all is open —
  `global-hotkey` spins its own thread on X11.
- Still no floating mic and no settings window; settings are the JSON file. See
  [docs/PORTING.md](docs/PORTING.md).

## [0.1.0] — 2026-07-31

First public release. Menu bar push-to-talk dictation for macOS, pointed at the
speech endpoint the Claude Code IDE extension uses for its dictation button.

### Added

- Push-to-talk dictation: hold to talk, release to insert. Tap to latch.
- Live typing — text lands at the cursor as you speak and rewrites itself when
  the recogniser revises a word, instead of waiting for you to stop.
- Committed text is locked, so a revision can never backspace into words you
  typed yourself.
- Focus guard: live typing is refused when the accessibility API says the
  focused surface is not text, falling back to a single paste at the end.
- Input anchoring — a take is pinned to the field it started in, so changing
  window mid-sentence no longer sprays the rest of the dictation elsewhere.
- Return ends the take, on the grounds that submitting means you are done
  talking. Turn it off for prose.
- Floating mic button as a non-activating panel that cannot steal focus, with a
  remembered position.
- Menu bar icon doubling as an input level meter, with a transcript preview.
- Settings across four tabs, every value still a plain `defaults` key
  underneath. Keyterm biasing, capped at the server's 1024-character budget.
- `QuotaWatch`, which records the WebSocket handshake headers so the "does this
  spend Claude quota" question is answered with evidence rather than assurance.
- Credential lookup from the macOS Keychain, falling back to
  `~/.claude/.credentials.json`, cached for the session to avoid a Keychain
  prompt per dictation.
- Diagnostic log at `~/Library/Logs/splaude.log`, plus Test Paste and
  Accessibility probes in the menu — every failure mode here looks identical
  from outside ("nothing happened").
- Hotkey recorder built on a local event monitor, allowing bare function keys.
- App icon, README banner, and a tag-triggered release workflow that builds,
  verifies and publishes the bundle.

[Unreleased]: https://github.com/bygelo/splaude/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/bygelo/splaude/releases/tag/v0.1.0

# Changelog

Notable changes to splaude. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.1] — 2026-08-21

### Fixed

- `--check` reported the builtin list alone, not the project bias. It reads the
  same wire list a take does, and that list now comes from a background-warmed
  cache which a one-shot `--check` process has not had time to fill — so the one
  diagnostic meant to show the harvested terms showed none of them. It now
  harvests synchronously, which a short-lived report can afford and a take
  cannot. The live dictation path is unchanged.


## [0.3.0] — 2026-08-21

### Fixed

- **The project harvest hung the take by several seconds.** Resolving the
  active project read every Claude Code session log whole — 3200+ files, 2.5 GB
  here — to pull a `cwd` from line five, synchronously on the hotkey path, so
  the microphone did not open for whole seconds after the keypress. Three fixes:
  the harvest now runs on a background queue and a take reads the last cached
  result instantly (warmed at launch, refreshed every five minutes, never
  computed inline); recency is ranked by project-*directory* mtime, one `stat`
  each, instead of by per-file mtime across thousands of files; and `cwd` is
  read from a bounded 64 KB prefix rather than the whole transcript. Cold first
  scan is ~11 s of background work at launch; every take after is instant.
- **A harvest past 93 terms silenced dictation entirely.** The endpoint answers
  a take carrying more than 93 keyterms with `TranscriptError` and drops the
  socket — not a truncated bias, no text at all. Both builds now cap at 64
  terms alongside the existing 1024-character budget — synthetic speech tripped
  the limit at 94 and live takes at 90, so the ceiling sits clear of both. Found by bisecting
  against the live endpoint with `--bench --say`; only the byte budget was
  bounded before, and the new project harvest routinely produces 145 terms.
- **Keyterms are sent in the `x-config-keyterms` header only.** The 2.1.98
  extension bundle appends one `keyterms` query parameter per term and sends no
  such header, so splaude briefly sent both to cover either. Measurement says
  that was backwards: a take carrying `keyterms` parameters fails with
  `TranscriptError` whether or not the header rides along, while the header
  alone transcribes and measurably biases the result. Whatever the extension
  talks to, it is not what this credential reaches. The header splaude has
  always sent was working; an earlier line here claiming otherwise was wrong.
- The test suite no longer writes into the real log. Opening the log file used
  to be automatic, so anything that linked `splaude-core` and logged a line
  appended to `splaude.log` — including every test, run in parallel by cargo,
  which interleaved fragments of unrelated runs into the one file *Reveal Log*
  exists to show when dictation misbehaves. The file is now something a program
  asks for with `diagnostic::to_file`, which only the dictation loop does; a
  test cannot pollute it by forgetting to opt out. `--check` no longer creates a
  log file either — a report that silently writes somewhere is a report that
  surprises whoever ran it.
- Log lines can no longer interleave. The file handle is held across the write
  rather than reopened per line, so two threads cannot split each other's
  sentences.
- A startup that fails now says so in the log. The error was returned to the
  runtime, which prints it to stderr — and a tray app launched from Explorer, a
  shortcut or launch-at-login has no stderr anyone will read. What that looked
  like from outside was the whole failure mode this log exists to prevent: no
  icon, a hotkey that does nothing, and a log holding a `start` line with
  nothing after it. Found in a real log, not in the suite.

### Added

- **Project-aware recogniser bias**, in both the Swift and the Rust build. The IDE extension seeds
  its recogniser with the workspace it is open in; splaude has no workspace, so
  it infers one from the newest session log under `~/.claude/projects` — the
  `cwd` field inside the file, not the directory name, which encodes `/`, `.`
  and `_` all as `-` and cannot be inverted. From that directory it harvests the
  project name, the branch words in `.git/HEAD`, the crate or package name, the
  top-level directory names, and the identifiers the README puts in backticks.
  Nothing to configure: the terms change when you change repo or branch. Visible
  in the tray and in `--check`, and switchable off there.
- **House and catalog bias.** The words a dictation gets wrong are rarely inside
  the file you have open, so two sources rank above the current project's own
  README vocabulary: the twenty most recently worked-in projects, taken from the
  same session scan that already ran, and a JSON catalog of the machine's
  infrastructure. `catalog_path` names any inventory file; splaude walks it and
  keeps the values under name-like keys (`name`, `project`, `host`, `slug`,
  `host_code`, `alias`), skipping objects with a `pid` because a running
  process's name is an OS artefact and not a word anyone says. Unset, it probes
  `~/.booted/inventory.json`. A file, not the endpoint that serves the same
  data: a file read cannot hang on the hotkey path, and splaude has no business
  shipping an HTTP client that talks to whatever a setting points at.
- Tier ordering, because packing truncates at the wire budget rather than
  sampling. Custom terms lead — they were typed because something was being
  misheard. Then the project's identity, then the builtin developer list, then
  the house (capped at half the budget so a machine with two hundred catalog
  entries cannot evict `TypeScript`), then the current README's identifiers.
- **`splaude --bench`** — a keyterm A/B that records a phrase once and replays
  the identical audio down two sockets, one biased and one not, so the only
  difference between the transcripts is the bias rather than how the phrase was
  said the second time. `--say` renders the phrase with the system voice for a
  repeatable, microphone-free check of a harvester change. Measured 14/21
  target words recovered with the bias against 11/21 without.
- splaude says when a newer version has been published. It asks GitHub once at
  startup and whenever the menu item is clicked, and the item does double duty:
  an update that exists opens the release page, anything else goes and looks
  again. Reported in `--check` too, which is the only part of that report that
  reaches the network.
- An available update is promoted to the top of the menu, beside the credential
  warning, and only when there is one. "You are up to date" is an answer to a
  question, not news, so it stays in the diagnostics group below.
- `SPLAUDE_LOG` overrides where the log is written — a full file path, for a
  portable install that wants its log beside the binary, or a machine where the
  usual location is not writable.

It checks and reports; it does not download, replace or restart anything. That
is a stopping point rather than an unfinished feature, and macOS is the reason:
this build is ad-hoc signed, so its code identity changes every release, and
macOS keys Accessibility and Microphone grants to that identity. A Mac that
updated itself would silently lose permission to do the only thing it exists to
do. Self-installation is tractable on Windows and Linux and is worth doing
there — behind a signature, not a bare download — but it is a separate change.

## [0.2.0] — 2026-08-08

### Added

- Warn about the credential before a take fails on it. splaude reads the Claude
  Code OAuth token but never refreshes it, so an install that is never opened
  alongside `claude` eventually finds it dead. The menu now warns within ten
  minutes of expiry and again once expired, rather than letting it surface as a
  dictation that mysteriously produces nothing.
- Cross-platform Rust workspace under `Crate/`, targeting Windows, Linux and
  macOS from one codebase. Windows and Linux are downloads as of this release;
  macOS still ships the Swift `.app`, and deliberately — see
  [docs/PORTING.md](docs/PORTING.md) for why, and for what is still not done.
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
  Green on all four legs on its first run. It also runs `splaude --check` on
  every platform: the suite links the crates and calls into them but never
  executes the binary, so without this the first run on Linux would have
  happened after a tag was pushed and every artifact was already built.
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
- Downloadable builds. The release workflow now publishes a Windows `.exe` and
  a Linux `.tar.gz` alongside the macOS bundle, so the Rust build is something
  people can run rather than something they must compile.
- Tap to latch, at the Swift build's own 400 ms threshold — the one value that
  has shipped and been lived with. Holding still works; a tap starts a take and
  leaves it running, and a second tap ends it.
- Return ends the take on Windows, through a low-level keyboard hook that
  observes and never swallows the key. It stands itself down if the hotkey is
  Enter, and it keys off a marker splaude stamps on its own events rather than
  the injected flag, so a user driving their keyboard through PowerToys or an
  on-screen keyboard is still a user pressing Return.
- Test Paste in the menu — types a known string through the same injector,
  carve-out and fallback a real take uses, with no microphone, socket or
  credential involved. Injection is the least verified part of this app and has
  produced every user-visible bug so far.
- A start and stop tone, synthesised through the output device `cpal` already
  provides, so no new dependency. Off by default, as on macOS.
- Quota has an answer rather than a log line. The README's central claim is
  that dictation does not spend Claude quota, and the handshake headers are the
  evidence — until now they only ever reached the log. Reported in `--check`
  and in the menu, keeping "never asked" and "asked and saw nothing" as
  separate readings, because the second is evidence and the first is silence.
- Edit Settings, which writes the current state out first if the file does not
  exist so the editor never opens nothing; and Reload Settings, which re-reads
  it, rebinds the hotkey, reconciles launch-at-login against the machine, and
  refuses mid-take rather than moving a binding under a held key. This is the
  first caller `HotkeyListener::rebind` has ever had.
- `splaude.exe` carries the splaude mark in Explorer, the taskbar and Alt-Tab,
  rendered at build time from the same pixel math the tray uses rather than
  from a committed image a human regenerates by hand. A second copy of a mark
  is a copy that drifts, which is the whole reason `Script/makeicon.swift`
  exists on the macOS side.
- A carve-out for applications that re-encode a keystroke by its keycode
  instead of reading the unicode payload it carries — remote desktop and VM
  clients. Fourteen are recognised by executable name, and the list is
  extensible from the settings file.

### Fixed

- Dictation into a Remote Desktop window produced a run of `a` instead of the
  transcript. Synthetic text is delivered on virtual key 0 with the codepoint
  as its payload, which is what makes typing layout-independent; a remote
  desktop client re-encodes keyboard input into scancodes, reads the keycode,
  and sends whatever key 0 is — on macOS, `kVK_ANSI_A`. splaude now pastes into
  those applications rather than typing at them, in both the Rust and Swift
  builds.
- Long takes lost their tail. The recogniser was fed faster than real time, so
  a take could be closed while audio was still undecoded. Audio is now paced
  against the wall clock with a small lead, which also bounds how far ahead the
  client can ever get.
- A byte-order mark silently discarded every setting. Notepad and PowerShell's
  `Out-File -Encoding utf8` both write one, `serde_json` rejects it as "expected
  value at line 1 column 1", and the whole file reverted to defaults over three
  bytes the user cannot see. Hand-editing is a supported path, so the file has
  to survive the editors people actually have.
- A settings file that fails to parse is now named, in the log, in `--check`
  and in the menu, instead of silently reverting to defaults. Nothing rewrites
  it — the broken text is the user's edit and it is what they need to see — a
  reload refuses outright and keeps what is already running, and toggling
  launch-at-login no longer saves defaults over a file it could not read, which
  would have destroyed a keyterm list as a side effect of clicking a checkbox.

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
- Windows defaults to a bare `F9` rather than macOS's `Alt+/`, and the reason
  is not taste. A modified binding needs its modifier held for the OS to keep
  matching the chord. macOS obliges — synthetic events carry their own flags,
  so the physical modifier is never touched. Windows has no per-event modifier
  mask, which forces a choice, and both options were tried against a live take
  and observed failing: release the modifier and the still-held key falls
  through and auto-repeats, so a take on `Alt+Space` came back shot through
  with spaces mid-word; hold it and every keystroke inherits it, and since
  `Alt+Backspace` is undo in a great many applications, each live-typing
  correction wiped the sentence before it. There is no safe modifier —
  `Ctrl+Backspace` deletes a word, `Shift` uppercases, `Win` turns every
  keystroke into a shortcut. A binding with no modifier has nothing to release
  and nothing to inherit.

### Known issues

- The live-typing model can disagree with the screen. When the locked-prefix
  rule holds back a deletion the diff asked for, the bookkeeping rebuilds its
  copy from the *target's* prefix rather than the characters actually left in
  place, so a later diff measures against the wrong baseline. Present in the
  shipping Swift build (`LiveTyper.update`) and carried into the Rust port
  deliberately rather than changed under cover of a rewrite; the Rust test
  `a_longer_contradicting_target_may_only_append_past_the_lock` pins the
  current behaviour and names it as a divergence.
- Dictation has only ever happened on Windows. It is confirmed end to end there
  — hotkey, microphone, socket, live typing into a real editor — and every bug
  fixed above came from that use, not from the suite. Linux and macOS now run
  `splaude --check` in CI, so the binary is known to start, find its
  configuration and exit cleanly on both; nobody has held the key on either.
- `--check` reports `global hotkey yes` and `type into apps yes` on a machine
  with no display at all. The probe only separates Wayland from not-Wayland, so
  an unset `DISPLAY` reads as a working X11 session. It is the honest output of
  what the probe measures and the wrong answer for the person reading it — a
  headless SSH user is told they are fine when nothing can work.
- Whether a modifier can be part of a Windows binding at all is unsettled. The
  default avoids the question; a user who sets one still meets it, since the
  injector must assert a modifier key-up before every synthetic event on
  Windows and Linux and neither platform exposes a per-event mask.
- Several surfaces have never been exercised by a person: both tap gestures,
  the Return hook, Test Paste, the tone, the quota line, Reload Settings, and
  what a refused rebind looks like from the front. The level meter, the
  transcript copy and launch-at-login surviving a restart are equally unproven.
- On Windows, splaude will physically press Return if a transcript ever
  contains a newline. `enigo`'s typing path special-cases `\n` with a real key
  click rather than a unicode payload. Not the Return hook's doing, and not
  fixed here.
- Two `rebind` failures are logged and not recoverable: an unregister that
  fails while the new registration succeeds leaves the old chord dead for every
  other application, and a new binding that is refused whose restore also fails
  leaves no hotkey and no way back but a restart. Neither is reachable from an
  ordinary edit.
- `tao` puts GTK3 and D-Bus on Linux, which is the dependency `tray-icon` is
  excluded there to avoid. Whether Linux needs the event loop at all is open —
  `global-hotkey` spins its own thread on X11.
- Still no floating mic and no settings window. Settings are the JSON file,
  which the menu now opens and reloads; a real window wants a toolkit that does
  not fight `tao` for the main thread, and that is its own change. See
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

[Unreleased]: https://github.com/bygelo/splaude/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/bygelo/splaude/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/bygelo/splaude/releases/tag/v0.1.0

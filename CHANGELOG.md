# Changelog

Notable changes to splaude. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] — 2026-11-26

### Added

- **`splaude transcribe` — subtitles and a timed transcript from a video.**
  A file or URL goes through ffmpeg, is cut at silences into pieces, and the
  pieces are transcribed side by side over the Claude Code sign-in (or
  `--backend deepgram`). Writes `srt`, `vtt`, `txt` and `json`, the last with
  every word timed. `-c` / `-t` / `--context-file` bias recognition towards the
  names in the recording. A 7:26 video takes 1:50.
- **Word timings that survive a lagging server.** The endpoint returns no
  timestamps, and some sessions fall 300–800 ms behind partway through, with
  sentence ends up to 2.2 s late. Each phrase's lag is now measured against the
  speech it sits over and taken off before words are snapped to the audio; a
  phrase whose words still land outside it is spread over it instead. On a
  7:26 run replayed offline, starts within 250 ms went 71% → 83% and the
  worst start 2642 → 1932 ms; on clean speech 92% → 94%; noisy audio is left as it was.
- **Words that arrived in one burst are spread over their phrase.** Inside
  a phrase, so the stray check passed, but piled against its end: "The old
  scheduler struggled" all ending at 28.4 s. A phrase holding a word shorter
  than a fifth of its spoken length is now spread over the phrase (0–0.5
  swept over four captured runs).
- **Word ends under background noise.** An end estimate in what the envelope
  called silence was walked back to the last loud frame, and under noise a
  word's quiet tail reads as silence: every end came out ~190 ms early, 68–69%
  within 250 ms. The walk now stops 100 ms past the last speech, and the start
  is searched from that speech: 89% in both live noisy runs.
- **A piece the server fell far behind on keeps its last sentences.** One
  session ran 12 s behind, and the close after a fixed 1.2 s of silence
  finalised only what had been decoded: two sentences gone, and 234 of 668
  words more than 3 s off. Silence now keeps flowing until the transcript has
  been unchanged for 2 s (at most 30 s); the next two runs kept 658 words and
  matched 637 and 636.
- **The run respects the account's 5 live speech sessions.** Measured: a 6th
  is refused with `rate_limit` / close 4029. `-p` stops at 5, defaults to 4,
  and a refused piece shrinks the run's concurrency and retries.

### Changed

- **The Rust build reads the Claude Code token from the macOS Keychain first.**
  It was file-only on macOS, where the file is usually a stale copy; the
  Keychain item is now read through `/usr/bin/security`, the tool Claude Code
  itself uses, so it does not prompt. The file stays as the fallback.
  `splaude transcribe` needs this to sign in on a Mac.

### Fixed

- **Typing into the field mid-take pauses live typing instead of ending it.**
  Any key used to stop the typer for the rest of the take and put the untyped
  remainder on the clipboard — 12 takes on 2026-11-25, 8 of them on Backspace.
  The typer now waits until you have stopped for 0.9 s, then starts a fresh
  segment at the cursor with the words not yet on screen, never backspacing
  into text from before the pause. A take that ends while you are still typing
  pastes its remainder automatically. Keys typed in another app while the
  anchored field is unfocused no longer count (3 of the 12). A halted burst now
  records only the characters it actually posted.
- **Sessions synced from another machine no longer poison the project bias.**
  A session recording `C:\Users\maran\Code\bygelo@main` made that whole string
  the project name, with a root that does not exist, so the README vocabulary
  came back empty and Windows paths went on the wire instead of `bygelo`, `tna`
  and `college` (654 → 492 of 1024 characters once fixed). A foreign `cwd` is
  cut to its repo name, `@branch` dropped, and found locally through the session
  directory's name or `~/Code`. Same fix in the Rust port, both directions.
- **A take started right after another one is no longer killed by the old one's close.**
  A take's close arrives up to 3 s after you stop it. A hotkey press in that gap
  started the next take, and the old close then cleared the current backend,
  stopped the new take's mic and left its socket open for 12 minutes (2026-11-25
  15:24:20). Every speech callback now names its backend, and the app ignores one
  that is not the current take (`[take] ignored … from a finished take` in the log).
- **The last words no longer type twice after Return, and live typing never
  backspaces over your own text.** The final transcript arrives ~0.3 s after
  Return; the typer still diffed against the text it believed was on screen
  and typed the tail into the emptied box. It now halts the moment Return or a
  user key is seen (its own keys are tagged and ignored), drops the late tail
  after a submit, and (superseded below) resumes once you stop typing.
- **A silent take reports a muted input** ("No sound from the mic — check
  Sound › Input") instead of finishing as an empty take.

- The keyterm budget meter counts against the limit the wire actually enforces.
  It showed characters only, and 0.3.0 had already established — by measuring
  the live endpoint, where 93 terms transcribe and 94 fail outright — that the
  binding limit is a *term count* of 64, and that exceeding it produces no text
  at all rather than a truncated bias. A meter reading "238 / 1024 characters"
  while the wire silently stopped at 64 terms was the wrong number in the one
  place a user goes to find out whether their list fits. It now reads both, from
  the same constant the packer uses.

- **A newline in a transcript no longer presses Return.** `enigo` special-cases
  `'\n'` on Windows into a real `VK_RETURN` click, so a transcript containing one
  submitted whatever the user was dictating into — a chat message sent
  mid-sentence, with the rest of the take going wherever came next.

  Worth recording how this was fixed, because the first fix did not work and
  looked like it did. Carrying the break as a unicode payload on virtual key
  zero — the way every other character travels, and exactly what macOS does —
  reads as obviously correct and still pressed Return: Windows translates an
  injected `U+000A` back into the Return key inside the keyboard layer, below
  anything this process controls. The new integration test measured that, which
  is the entire argument for having written it. A line break is now replaced
  with a space, which loses the break and keeps the words; a paragraph arriving
  as one line is a cosmetic loss somebody can see and fix, and a message sent
  before it was finished is not.
- **`--check` no longer tells a headless machine it can dictate.** The
  capability probe asked only "is this Wayland?" and treated every other answer
  as a working X11 session, so an unset `DISPLAY` — which is what an SSH login
  is — reported that the hotkey and typing were both fine. That is the worst
  shape a diagnostic can take: confidently wrong exactly where somebody is
  running it *because* nothing works.
- **Opening the settings window no longer waits on the network.** The status
  page called the update check while building itself, so nothing appeared until
  GitHub answered — seconds on a good connection, a timeout's worth on a bad
  one, with no window and no spinner to say why. Measured at over four seconds
  and now 0.56. The check belongs to the *Re-check* button.
- **`splaude --settings` writes a log.** It was the one entry point that opened
  no log file, so a settings window that failed to save failed in silence.

- The floating button no longer restores onto a display that is no longer there.
  Windows took the saved position on trust, so dragging it to a second monitor
  and later undocking left a 56-pixel topmost window off-screen — no menu on it,
  and no way back but hand-editing the settings file. The Swift build has guarded
  this since it shipped (`isOnScreen` in `FloatingMic.swift`) and the port did
  not; the position is now checked against the monitor set with
  `MONITOR_DEFAULTTONULL`, which answers null rather than helpfully naming the
  nearest display, and an unusable one falls back to the default placement.
- A hotkey edge is timed when it happens rather than when the event loop gets to
  it. Tap-versus-hold is a comparison against a 400 ms threshold, and both ends
  of it used to be read at the moment the loop drained the event — so anything
  that held the loop up between a press and its release could turn a tap into a
  hold. The edge now carries the instant it occurred, which is also what lets
  Start Dictation and the floating button ask for a take by posting the two
  edges of a tap: they are stamped with one instant, so the pair cannot be
  mistaken for a hold however long the socket takes to open in between.
- A take pasted into a Remote Desktop, VM or screen-sharing window no longer
  arrives as a single letter `v`. The paste chord set `.maskCommand` as a *flag*
  on the `V` and posted no ⌘ event of its own, which is a complete ⌘V to any app
  that reads the event we hand it — and nothing at all to a client that does not
  read our event but re-encodes the keyboard the way hardware presents it:
  modifier state from the ⌘ key going down and up, the letter from the keycode.
  It never saw a ⌘, so it shipped a bare `v` to the far end, replacing the take
  with one character on top of whatever was selected there. This is the exact
  mirror of the bug the paste route was built to fix — the route survived
  keycode translation, the chord that drove it did not — and it meant the
  carve-out had never actually worked in the app it was written for. Remote
  clients now get a chord shaped like hardware: ⌘ posted as its own
  `.flagsChanged` event, 12 ms between each event so a sampling client cannot
  land between the modifier and the key, and posted at the HID tap, which is
  upstream of the event taps these clients read the keyboard through.
- A take pasted into a remote window no longer arrives as the *previous*
  clipboard, or as nothing. The clipboard was handed back 0.35 s after the
  chord — right for a local app, which reads the pasteboard synchronously
  inside its own paste, and far too early for a redirected one, where the
  client must first notice the clipboard changed, announce the format list over
  the wire, and wait for the far end to ask for the bytes. splaude was answering
  that request with the contents it had already restored. Remote pastes now wait
  250 ms before the chord goes out and hold the clipboard for 2 s afterwards.
  Both failures read as "nothing happened", which is why they outlived the
  feature that caused them.
- The same two on Windows, where `Injector::paste` is only ever reached for an
  app on the keycode list and so is remote by construction: a 250 ms lead before
  the chord, 12 ms either side of the keystroke inside it, and the clipboard
  held for 2 s rather than 0.35.

### Added

- **A second speech provider, so the endpoint splaude depends on is no longer
  the only one.** `speechBackend: "deepgram"` and an `apiKey` of your own send a
  take to Deepgram directly instead of to Anthropic's internal WebSocket. The
  README has always answered "what if that endpoint goes away" by pointing at
  `SpeechBackend` being a small trait — but the trait had exactly one
  implementor and the app named it directly, so the mitigation was a claim about
  the shape of the code rather than something anyone had done. It is now a
  setting. The reason to spend the effort before it is needed: Anthropic began
  rejecting third-party use of Claude Code OAuth tokens in early 2026 and
  several tools built on that credential stopped working the same week, without
  notice. splaude's speech endpoint still answers; the point is that the day it
  stops, the fix is a line in a settings file.
- **`splaude --toggle` and `splaude --cancel`.** The hotkey is not always
  deliverable — a full-screen remote-desktop client takes the keyboard before
  any local application sees it — so anything that can run a command can now
  start or stop a take: a launcher, a Stream Deck, AutoHotkey, the remote host
  itself. `--cancel` ends a take and throws its words away, which is the
  different thing you want when you notice you are dictating into the wrong
  window.
- **Starting splaude twice raises the copy that is running.** It used to start a
  second process, fail to register the hotkey, and exit — with no window and no
  console, so double-clicking the icon did nothing at all, and doing it again
  did nothing again. The single-instance test and the command channel above are
  deliberately the same mechanism: "is anyone there" and "please take this" are
  one question, and a separate mutex would be a second source of truth about
  whether splaude is running.
- **A latched take stops itself after 30 seconds of silence.** Only latched
  takes, and that is the whole design: a held take ends when you let go, so it
  cannot be forgotten, while a latched one runs until something stops it. Set
  `latchTimeoutSecond` to 0 to keep the old behaviour. Silence is measured
  against a noise floor rather than against the level meter's first step, which
  is a fifth of full scale and would call a quiet voice silence.
- **Corrections: a `replacement` map, applied to the transcript before it is
  typed.** Keyterms bias what the recogniser *hears*; nothing could fix a name it
  spells the same wrong way every time. Longest rule wins, and the output is
  never re-scanned, so a rule whose replacement contains its own key terminates
  instead of looping.
- **`bufferOnlyApp`: applications that get the take pasted at the end rather
  than typed live.** Distinct from the keycode list, which is about applications
  where typing is *broken*; this is for applications where it is merely unwise.
  A terminal running a full-screen program reads characters as commands, and
  live typing revises itself with backspaces — the focus guard already refuses
  to claim it can tell a terminal's state, so this is the user answering the
  question the guard honestly cannot.
- **The settings window carries all of it**: the latch timeout, the corrections
  box, and a provider picker with a key field on the status page. The Connection
  paragraph now describes the provider actually selected, because the sentence
  about not spending Claude tokens is true of only one of the two.
- **A microphone row in the settings window and in `--check`.** The Swift window
  has a Microphone permission row; the Windows shape of that question is not a
  grant but a device, and a Remote Desktop session with no audio redirected, an
  output-only headset and a microphone disabled under Privacy all present as the
  same *no input device* — which is the most common reason dictation does
  nothing here.
- **The injection path has an integration test.** `Crate/platform/tests/
  injection.rs` owns a real window, injects through the same `Injector` a take
  uses, and reads the characters back. It is the correction of an inverted
  coverage story: the socket, which has never broken in the field, had 687 lines
  of integration test, while injection — which has broken in front of users
  three times — had thirteen tests and every one was pure. It is `#[ignore]`d
  because it seizes the keyboard and the foreground window; run it deliberately
  with `cargo test -p splaude-platform --test injection -- --ignored
  --test-threads=1`.
- **Releases carry build provenance.** The `.sha256` beside each asset answers
  "did this arrive intact" and cannot answer "did splaude build this", because
  whoever replaces the file can replace the checksum next to it. Each asset is
  now attested by GitHub's own identity for this workflow, repository and
  commit: `gh attestation verify splaude-<version>-windows-x86_64.exe --repo
  bygelo/splaude`. Chosen over shipping a minisign key because that key has to
  live somewhere, be rotated by somebody, and be trusted on the word of the same
  README that would be compromised alongside it. It is not code signing and does
  nothing for SmartScreen.
- **A winget manifest**, rendered by the release workflow with the version and
  hash already filled in. winget does not require code signing, so this route is
  reachable now; Homebrew's cask tap does require it, which is why
  `brew install --cask splaude` is still behind the $100/year Apple Developer
  Program in Triage.
- **The take has tests.** Ten of them, driving `consume` — the seam immediately
  below the microphone — with a stream of speech events and reading back the
  keystrokes it produces. That seam was chosen for a reason worth recording:
  everything above it needs hardware and a credential, everything below it is
  what the user actually sees, and the machine this was written on has no input
  device. They cover a buffered take, a live one, a revision that backspaces,
  corrections reaching both paths, a keycode application being pasted rather
  than typed, and a cancelled take inserting and reporting nothing.

  The first run of them found a bug in the tests rather than the code, which is
  the right way round: the helper concatenated the `Type` commands and ignored
  the backspaces, so it read `Cloudaude Code` where a user sees `Claude Code`
  and called a working revision a failure. It now simulates the screen.
  Running it in a hostile environment cost two more rounds and was worth both.
  It failed once with the contents of a *Mac's* clipboard in the field, which
  looked exactly like a remote-desktop session stealing the clipboard — and was
  not. The real cause was the harness: `Injector::paste` blocks while it holds
  the clipboard and then puts the user's contents back, so a test that injected
  first and pumped afterwards delivered `Ctrl+V` to a window that would not
  process it until after the restore. A real application pumps its own queue
  continuously; the test now does too. A speculative "the clipboard was stolen"
  retry written against the first diagnosis was reverted once the second one
  held — a probe showed the clipboard write surviving perfectly well.

  It also needs an idle desktop, and says so: every test takes the foreground
  and checks it still has it at the end, so a window stealing focus fails with
  `something else took the foreground` rather than with an assertion about
  text. That is the harness being honest about its preconditions.
- **The latch timeout is a pure rule with eight tests**, extracted the way `act`
  already was. It cannot be exercised in a real take without a microphone in a
  quiet room, so the decision — grip, clock, setting — was separated from the
  event loop that acts on it. The test that matters most is the one asserting a
  *held* take is never ended by silence, however long: the key is the contract.
- **A take logs how long after the socket opened the first words came back.**
  Endpointing is reported to be the largest single contributor to perceived
  latency in streaming recognition, and splaude sets `endpointing_ms` on the
  wire — but it had never recorded a number of its own, so any change to that
  setting would have been guesswork. Measured before tuned.
- **`ROADMAP.md` and `docs/roadmap-rejected.md`.**

- **The Windows build has the Mac app's interface.** Everything the menu bar and
  the settings window offer on macOS is now offered on Windows, and the gap that
  closes is not a small one: until now the settings *file* was the interface
  there, which is a fair answer for someone who already knows the file exists
  and a dead end for everyone else.
  - A settings window, with the same four groups as the Swift one and the same
    words in them: the four dictation toggles, the typing-speed slider, the
    language list, the binding — typed, or recorded by pressing the chord — the
    keyterm list with its built-in terms and its 1024-character budget, the
    interface and startup switches, and a status page carrying the credential,
    what this machine is capable of, and what the speech connection is.
  - A floating mic button. Click to start or stop, drag to move, and it never
    takes keyboard focus — which is not a detail: a button that took focus to be
    clicked would take the text with it.
  - **Start Dictation** in the tray menu. The hotkey is not always available —
    a full-screen remote desktop swallows it, another app may already own it —
    and until now the menu could name a binding while offering no way to dictate
    when that binding never arrived.
  - The credential warning is clickable, and puts `claude` on the clipboard.
    Same as `showCredentialHelp` in the Swift build, for the same reason: the
    fix has to be typed into a terminal, and nobody should transcribe it from a
    menu that closed when they clicked it.
  - The status page answers the microphone question, which is the Windows shape
    of the Swift window's Microphone permission row. There is no per-app grant
    to check here the way macOS has one; what there is instead is a device, or
    not — and a Remote Desktop session with no audio redirected, an output-only
    headset and a microphone switched off under Privacy all present as the same
    *no input device*. It names the device when there is one, because a user
    looking at the wrong headset knows immediately what to change. `--check`
    reports the same line.
  - `splaude.exe --settings` opens the window with no hotkey, microphone or
    tray. That is the way in when the notification-area icon is still hidden in
    the overflow, which is where Windows puts every new icon.

  Both windows are hand-written Win32. `docs/PORTING.md` had this filed as the
  largest open dependency question in the project — a settings window needs
  controls, `tao` supplies only a window, and every candidate toolkit wants the
  main-thread event loop `tao` already owns for the hotkey pump. Win32 makes no
  such claim: a window on its own thread has its own message queue, and the
  `windows` crate was already in the graph. Neither window shares state with the
  loop — the settings window writes the file and says so, and the loop re-reads
  it through the same path *Reload Settings* uses.
- A roadmap, and a record of what was turned down. `ROADMAP.md` carries tiered
  work where every row names the thing that will exist and the test that decides
  it is done; `docs/roadmap-rejected.md` carries what was considered and refused,
  with the reason, so a killed idea stays killed. Both came out of a sweep of the
  open-source dictation field — Handy, VoiceInk, WhisperWriter, Speech Note,
  nerd-dictation, OpenWhispr and others — filtered against what is actually true
  of this repo rather than adopted because a sibling project has it.
- A take that cannot start says so in the log. It said so on stderr, which a
  tray app launched from Explorer or from launch-at-login has nobody to show —
  so a microphone that will not open looked exactly like a hotkey that did
  nothing. The common cause is worth naming: a Remote Desktop session with no
  audio redirected has no input device at all, and `cpal` correctly reports
  none.
- Every take names its destination's bundle identifier in the log, not only the
  ones already known to need pasting. The paste-only list is bundle identifiers
  and cannot be complete, an unlisted remote client costs the whole take, and
  the identifier that would fix it was the one thing the log never printed —
  leaving the user to guess a string they have no way to see. It is now on the
  `record` line of every take, ready to paste into the custom list.
- NoMachine, Jump Desktop, UTM and VirtualBox join the built-in paste-only list.

## [0.3.2] — 2026-08-21

### Fixed

- The Keychain password prompt no longer appears on its own. Credential health
  is shown on a five-minute timer that runs whether or not you are dictating,
  and it read the token by going to the Keychain — an ACL decision macOS answers
  with a login-password dialog whenever the "Always Allow" grant does not apply,
  which is exactly the case the moment the token passes expiry and Claude Code
  has rewritten the item with a fresh one. So the dialog surfaced spontaneously,
  every few minutes around each token refresh, with no take in sight. Health now
  classifies the already-cached token and never reads on a timer; the Keychain
  is read only by an actual take, which is the only thing that needs a live
  token. Fixed in both builds. (A rebuild still invalidates "Always Allow"
  because an ad-hoc signature's code hash changes every build — that part is
  inherent to shipping unnotarized.)


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

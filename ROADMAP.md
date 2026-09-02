# Roadmap

What splaude does next, and how each item will be known to be done.

Edited honestly. A gap is a gap, not an opportunity. Nothing reaches a tier
without a falsifiable acceptance test in its row — if there is no way to say
"this worked", the item is in [Triage](#triage), not on the board.

Two companions to this file:

- [docs/roadmap-rejected.md](docs/roadmap-rejected.md) — what was considered and
  turned down, with the reason. Read it before proposing something; it is what
  stops a killed idea coming back every quarter.
- [docs/PORTING.md](docs/PORTING.md) — *Verified, and not*, which is the honest
  record of what has actually been run by a person. Several rows here exist
  because that section says nobody has tried something.

**Surface** is the literal thing that will exist when the row is done — a
command, a control, a file. A row whose surface is a test rather than a product
says so, so nobody counts a harness as shipped work.

Dated 2026-09-02. Every row on it was built the same day it was written, so the
Status column is the interesting one: it separates what was *made* from what has
been *proved*, and several rows are honest about being only the first.

The recurring reason is worth stating once rather than in twelve rows: **this
machine has no microphone.** It is a Remote Desktop session with no audio
redirected, so `cpal` reports no input device and no take has ever started here.
Everything downstream of the microphone — live typing a real transcript, the
latch timeout firing, a correction landing in a real take — is built, unit
tested, and unproven end to end.

---

## P0 — before anything new

| Item | What it closes | Surface | Effort | Benchmark | Status |
| --- | --- | --- | --- | --- | --- |
| Dictate through a second speech provider with your own key | The endpoint splaude depends on is undocumented and unsanctioned, and the README's own mitigation — "swapping in your own Deepgram key is one file" — has never been exercised. `Crate/core/src/speech/mod.rs:100` defines the trait; `Crate/app/src/take.rs:103` hard-codes `AnthropicSpeechBackend`, and it is the only implementor on either build. An abstraction with one implementation is a claim, not a seam. | `speechBackend` + `apiKey` in the settings file and the settings window; `splaude --check` names the active backend | M | **Tier 3.** Set `speechBackend: "deepgram"` and a key, dictate a sentence, and it lands at the cursor. PASS iff the take completes and `--check` prints the backend in use. **Run 2026-09-02**: `--check` reports the backend; the Deepgram path is selected at runtime and refuses with `speechBackend is "deepgram" but apiKey is empty`, naming the file to fix. **The socket itself is unproven** — nobody here has a Deepgram key. | **Done** 2026-09-02 · socket unproven |
| Restore the floating button onto a display that is still attached | The Windows button restores `floatingButtonPoint` blindly and defaults with `SM_CXSCREEN` (primary monitor only) — so a button dragged to a second display and then unplugged comes back off-screen, as a 56×56 topmost window with no menu and no way back but hand-editing JSON. The Swift build already guards this at `Source/splaude/FloatingMic.swift:83` (`isOnScreen`, falling back to a default placement). A regression the port introduced, not an original gap. | The button, visible, after the monitor it was on is disconnected | S | **Tier 3.** Drag the button to a second display, disconnect it, restart splaude. PASS iff the button is visible on a remaining display. **Run 2026-09-02**: a position of `9000,9000` planted in the settings file is rejected and the button comes back at `1680,1009` on the 1800×1129 display. | **Done** 2026-09-02 |
| Prove a synthetic keystroke actually arrives | Injection has broken in front of users three times — `57c1074`, `7575e07`, and the ⌘-as-a-flag bug fixed this release — and the suite caught none of them. `Crate/platform/src/injector.rs` has 13 tests and every one is pure (`chunk_char`, `binding_modifier`); not one sends a keystroke. Meanwhile `Crate/core/tests/socket.rs` is 687 lines of integration test for the socket, which has never broken in the field. The coverage is inverted with respect to the risk. | `cargo test -p splaude-platform --test injection` — **a test command, not a product surface** | M | **Tier 1** (escalated: three field regressions in one subsystem, all invisible to the suite). `Crate/platform/tests/injection.rs` — six tests owning a real window, injecting through the same `Injector` a take uses, and reading the characters back. **Red-green proven**: reinstating the paste-modifier bug makes the field read `"v"`; reinstating the newline bug makes a Return arrive. Stays `#[ignore]`d — it seizes the keyboard and the foreground window, so the default gate stays green. Run: `cargo test -p splaude-platform --test injection -- --ignored --test-threads=1`. **It needs an idle desktop**: run on a machine somebody is using and a window occasionally steals focus mid-test, which fails as `something else took the foreground` rather than as a text assertion. Two of three consecutive runs were fully green inside an active Remote Desktop session; the third named its own precondition. | **Done** 2026-09-02 · needs an idle desktop |

---

## P1 — the failures that look like nothing happening

Every row here is a state where splaude does something reasonable and the user
sees no evidence of it. That is the failure mode this project keeps re-learning.

| Item | What it closes | Surface | Effort | Benchmark | Status |
| --- | --- | --- | --- | --- | --- |
| Launching splaude twice raises the copy that is running | There is no single-instance guard anywhere in `Crate/app/src/main.rs`. The second process starts, fails to register the hotkey, and exits — this machine's own log says `[start] cannot register F9: HotKey already registered`. It has no console and no window, so from outside, double-clicking the exe does nothing at all, twice. | Running `splaude.exe` while it runs raises the settings window instead of exiting | S | **Tier 3.** Start splaude, then start it again. PASS iff exactly one process remains and the settings window comes to the front. **Run 2026-09-02**: the second launch prints `splaude is already running`, raises the settings window, and exactly one process remains. | **Done** 2026-09-02 |
| Start a take from outside splaude | The hotkey is the only way in, and it is not always deliverable: a full-screen remote-desktop client swallows it before any local app sees it — the reason `Start Dictation` was added to the menu this release. A command that toggles the running instance lets a Stream Deck, AutoHotkey, a Shortcut, or the remote host itself start a take. `grep` finds no IPC of any kind in the workspace. | `splaude --toggle` and `splaude --cancel` | M | **Tier 3.** With splaude running and focus in an editor, run `splaude --toggle`, speak, run it again. PASS iff text lands and no second instance starts. **Run 2026-09-02**: `--toggle` reaches the take path and exits 0; with nothing running it refuses with exit 1. The speaking half is unproven — this machine has no microphone. | **Done** 2026-09-02 · take unproven |
| A newline never presses Return in someone's chat box | `enigo` special-cases `\n` in its typing path with a real `VK_RETURN` click rather than a unicode payload — documented at `Crate/platform/src/injector.rs` and never fixed. Nothing strips newlines before typing. splaude's own Return watcher ignores it (it carries our marker), but the focused application does not: a transcript containing a newline submits the message mid-dictation. | Dictating a transcript containing a newline | S | **Tier 3.** Feed the injector `"one\ntwo"` and count Return key-downs. PASS iff zero arrive and both words do. **Run 2026-09-02**, and it took two attempts: carrying the break as a unicode payload — the obvious fix, and what macOS does — still pressed Return, because Windows translates an injected `U+000A` back into the Return key below anything this process controls. The harness caught that. A line break is now replaced with a space. | **Done** 2026-09-02 |
| A latched take stops itself when you stop talking | `Grip::Latched` runs until the next press; nothing watches the audio. A latched microphone left open is exactly the state the start/stop tone exists to make audible, which is an admission the state is easy to reach. The level is already computed per buffer (`resample::level_of`, `Capture::last_peak`), so this needs no new dependency and no VAD model. | `latchTimeoutSecond` in settings; a latched take ends after N seconds of silence | M | **Tier 3.** Latch a take, say nothing for the configured window. PASS iff the take ends, the text is inserted, and the icon goes idle. **The rule is proven, the firing is not.** The decision was extracted as a pure function over the grip, the clock and the setting — the same shape as `act` — and eight tests cover it, including the one that matters most: a *held* take is never ended by silence, however long. Nobody has watched it fire in a real take, because that needs a microphone in a quiet room. | **Done** 2026-09-02 · rule proven, firing unproven |

---

## P2 — sharper edges

| Item | What it closes | Surface | Effort | Benchmark | Status |
| --- | --- | --- | --- | --- | --- |
| Choose which applications get buffered text instead of live typing | `Crate/platform/src/focus.rs` refuses to classify terminal hosts on purpose — "a full-screen TUI turns that text into commands, and we will not claim confidence" — but the only remedy is turning live typing off everywhere. The per-app mechanism already exists for a different question (`customKeycodeApp`), so the shape is proven; this is a second list, not a new idea. | `bufferOnlyApp` in settings and the settings window | S | **Tier 3.** Add a terminal to the list, dictate into it. PASS iff nothing is typed until the take ends, and live typing still runs elsewhere in the same session. The list, its matching rules and its independence from the keycode list are covered by three unit tests, and the delivery it selects — buffer to the end rather than type live — is covered by the take-lifecycle tests in `take.rs`. What is still unproven is only the step that needs hardware: a real microphone feeding a real terminal. | **Done** 2026-09-02 · unproven with a microphone |
| Correct a word the recogniser always gets wrong | Keyterms bias what the recogniser *hears*; they cannot fix a term it consistently spells another way. There is no replacement step anywhere between the transcript and the keystrokes. Pure, string-in/string-out, and testable without a device — which is the kind of code this repo's suite is already good at. | `replacement` map in settings, applied to committed text | S | **Tier 3.** Map `"Cloud Code" → "Claude Code"`, dictate the phrase. PASS iff the corrected form is what lands, and an interim revision does not double-apply it. **Run 2026-09-02**: the rule saves from the settings window; eleven unit tests cover longest-match, non-rescanning, idempotence across frames and multibyte keys; and two take-lifecycle tests prove it reaches the keystrokes of a live take *and* the text of a buffered one, with a revision mid-word not double-applying it. | **Done** 2026-09-02 |
| `--check` stops claiming a headless machine can dictate | `Capability::detect` (`Crate/platform/src/lib.rs:62`) sets `global_hotkey` and `inject_text` from `!is_wayland()` alone, so an unset `DISPLAY` reads as a working X11 session. A headless SSH user is told everything is fine when nothing can work. Already named in the CHANGELOG's own Known issues. | `splaude --check` on a machine with no display | S | **Tier 3.** Run `--check` over SSH with `DISPLAY` unset. PASS iff `global hotkey` and `type into apps` both read `no` with a note naming the missing display. **Covered by six unit tests** over the pure rule, and CI runs the real path on Linux on every push. | **Done** 2026-09-02 |
| The settings window is sharp on a scaled display | Nothing in the build declares DPI awareness — no manifest, no `SetProcessDpiAwareness` call — so Windows bitmap-stretches both new windows on any display above 100%. Everything shipped this release was only ever seen at 100%. | The settings window and the floating button at 125/150/200% | M | **Tier 3.** Set the display to 150%, open the settings window. PASS iff text is crisp and no control is clipped. **The arithmetic is proven, the pixels are not.** The scale rule was separated from the call that reads the display, and six tests cover it at 100/125/150/175/200/250% — including that ordering survives scaling, so controls cannot overlap, and that a useless DPI reading means no scaling rather than a collapsed window. Nobody has *looked* at the window above 100%: this machine is at 100%, and changing a working display's scale was not worth the risk. | **Done** 2026-09-02 · arithmetic proven, pixels unseen |
| Install splaude with a package manager | Today's Windows story is "download an unsigned exe and click through SmartScreen". winget explicitly does not require code signing, so a manifest is reachable now, with no certificate and no money. Signing the release assets with minisign costs nothing either and is what comparable projects (Handy) do. | `winget install splaude`; `.minisig` beside each release asset | M | **Tier 3.** On a clean machine, `winget install splaude` then `splaude --check`. PASS iff it installs and the report prints. **Not run** — it needs a published release and a submitted manifest. The manifest is written, rendered by the release workflow with the version and hash filled in, and parses. Build provenance replaced minisign; the CHANGELOG says why. | Built 2026-09-02 · unsubmitted |

---

## P3 — the two-codebases question

| Item | What it closes | Surface | Effort | Benchmark | Status |
| --- | --- | --- | --- | --- | --- |
| Decide what the Swift build is for | One product is implemented twice, and the cost is now measurable rather than theoretical: this release's remote-desktop fix had to be written once in `Source/splaude/TextInserter.swift` and again in `Crate/platform/src/injector.rs`, and the P0 off-screen row above exists precisely because the port lost a guard the Swift build has. `docs/PORTING.md` frames the Rust build as eventually covering macOS but sets no condition for retiring the Swift one. Nothing asserts the two behave alike. | **Governance — no user surface.** A written decision in `docs/PORTING.md`: what the Rust macOS build must do before the Swift app is retired, or why both stay | S | **Tier 3.** PASS iff `docs/PORTING.md` names the condition and a reader can say which build a macOS user should run and why. **Done** — *What the Swift build is for* names four conditions and says the Swift app is what a macOS user should run until all four are closed. | **Done** 2026-09-02 |

---

## What we are NOT going to do

Mirrors [docs/roadmap-rejected.md](docs/roadmap-rejected.md), which carries the
full reasoning.

- **Bundle a local Whisper model.** The pitch is "you have run `claude`, there is
  nothing else to set up". A model manager, 1–3 GB of assets and an inference
  engine is a different product; bring-your-own-key buys the same insurance
  against the endpoint vanishing for a fraction of the surface.
- **Clean transcripts up with an LLM.** splaude's whole claim is that dictation
  does not spend Claude tokens. This would spend them, on every take, and can
  invent words the user did not say.
- **Voice commands and spoken formatting grammars.** "scratch that", "snake case
  hello world" — a command system is not a push-to-talk dictation tool.
- **Store a transcript history.** The README says nothing is stored. That is a
  posture, not an oversight.
- **Text-to-speech, translation, subtitles, meeting mode, speaker labels.** Other
  products, sharing only a microphone.
- **A VAD model to detect silence** when a peak level is already computed for
  every buffer to drive the meter.

## Triage

Plausible, and not groundable from here. Each names what would settle it.

Two of the four that were here on 2026-09-02 have been settled since, and are
kept with their answers rather than deleted — a triage row that vanishes teaches
nobody why it was ever open.

| Item | What is blocking |
| --- | --- |
| Signing and notarization | **Still open, and it is a spending decision rather than an engineering one.** macOS notarization needs the $100/year Apple Developer Program, and Homebrew's cask tap will not take an unsigned binary, so `brew install --cask splaude` is gated behind it. Windows SmartScreen can be cleared through Azure Trusted Signing at about $10/month. Everything that does not cost money is done: releases carry build provenance, and the winget manifest is written and rendered per release. |
| Typing into Wayland via `ydotool`/uinput | **Still open, and it is a stance decision.** Technically real — it injects below the compositor, so it works where the virtual-keyboard protocol is not implemented, which includes GNOME — but it needs a uinput daemon and a udev rule, and `docs/PORTING.md` says Wayland's refusal is "the security model working as designed, not a gap to route around". Nobody has launched splaude on Linux at all, so building a route around a refusal for an unproven platform is speculation with a compile step. Revisit when somebody runs it. |
| ~~Whether the keyterm budget is the right budget~~ | **Settled in 0.3.0, on the other machine, and better than this roadmap proposed.** It was not the right budget: the binding limit is a term *count*, and past it the server rejects the take rather than truncating the bias — so a list that grew too long produced no text at all. That was found by measuring the live endpoint (93 terms transcribe, 94 fail) and fixed at 64 with headroom. This roadmap had proposed a ceiling of 100 taken second-hand from Deepgram's documentation, which is above the real boundary and would not have worked; it was written, merged into that measurement, and removed as redundant and wrong. What survives from this side is smaller: the settings window's budget meter now counts against the measured constant instead of characters alone. |
| ~~Latency tuning~~ | **Half-settled 2026-09-02.** The blocker was that splaude measured none of its own latency, so any change to `endpointing_ms` would have been guesswork. A take now logs how long after the socket opened the first words came back. The *tuning* is still open and now has somewhere to start — but it needs a microphone, and nobody has produced a number yet. |

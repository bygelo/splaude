# Considered and turned down

The point of this file is that a killed idea stays killed. Each entry says what
was proposed, where it came from, and the one reason it is not on
[ROADMAP.md](../ROADMAP.md). Reopening one is fine — argue with the reason.

Reviewed 2026-09-02, against a sweep of open-source dictation projects
(Handy, VoiceInk, WhisperWriter, Speech Note, nerd-dictation, OpenWhispr, Numen,
Buzz, Vibe, Whispering) and the surrounding literature.

## Ship a local speech model

**From:** Handy, VoiceInk, WhisperWriter, Speech Note, Vibe and most of the
field — local whisper.cpp is close to table stakes for this category.

**Rejected because** splaude's entire premise is that there is nothing to set
up: you have run `claude`, so you are already authenticated. Shipping a model
means a downloader, a model manager, 1–3 GB of assets, a GPU/acceleration story
per platform, and an inference engine — more surface than the whole current
application. The risk it insures against is real (the endpoint is undocumented
and could vanish), but **bring your own key** — P0 on the roadmap — insures
against the same risk for a fraction of the cost, and proves the provider seam
that already exists in the code.

Worth revisiting if the endpoint actually closes, or if someone wants splaude to
work with no network at all. That is a different product decision, not this one.

## Post-process transcripts with an LLM

**From:** VoiceInk's writing modes, OpenWhispr's assistant mode, Ghost Pepper's
filler-word cleanup. Published results are genuinely strong — one 2025 study
reports WER falling from 18.08% to 4.75% after correction.

**Rejected because** the headline fact about splaude is that dictation does not
spend Claude tokens; the speech endpoint runs Deepgram Nova-3 and is not on that
meter. A cleanup pass spends them on every take. It also adds a model round-trip
to a path measured in hundreds of milliseconds, and an LLM asked to fix a
transcript can rewrite words the user actually said.

## Voice commands and spoken formatting

**From:** Talon's formatter grammar ("snake hello world"), nerd-dictation's
spoken-number conversion, "scratch that" style editing commands.

**Rejected because** it changes what the product is. splaude holds a key, hears
a sentence, and puts that sentence at the cursor. A command grammar means a
parser, a command set, per-application bindings and a mode the user has to hold
in their head. Numen and Talon do this well and are the right tools for it.

Note the narrow, non-command version of this — fixing a term the recogniser
always spells wrong — **is** on the roadmap as a replacement map (P2). That is a
lookup table, not a language.

## Keep a transcript history

**From:** OpenWhispr's transcript history, Buzz's searchable viewer.

**Rejected because** the README states that nothing is stored and nothing is
uploaded. Keeping a searchable record of everything the user has dictated is a
change of privacy posture, and it should be argued as one rather than arriving
as a convenience. The tray already offers the last take, and a refused paste
leaves the text on the clipboard, so the immediate loss it would prevent is
already covered.

## Text-to-speech, translation, subtitle export, meeting mode, speaker labels, media import

**From:** Speech Note (10 TTS engines, Bergamot translation, SRT), Buzz
(TXT/SRT/VTT export, playback viewer), Vibe (YouTube/Vimeo import), OpenWhispr
(meeting transcription with speaker identification).

**Rejected because** these are transcription *products*. splaude is an input
method: audio in, keystrokes into the focused field, nothing retained. They share
a microphone and nothing else.

## A VAD model for silence detection

**From:** Handy's use of Silero VAD.

**Rejected because** a peak level is already computed for every audio buffer to
drive the icon's meter (`resample::level_of`, `Capture::last_peak`). Adding a
neural VAD to answer "is anyone talking" would be a model to replace a
comparison. The feature it would enable — stopping a latched take on silence —
is on the roadmap (P1) and is built on the level that already exists.

## Enterprise SSO and SCIM

**From:** OpenWhispr's enterprise tier.

**Rejected because** splaude authenticates as the person sitting at the machine,
by reading a credential that is already there. There is no server, no account and
no organisation to administer.

## Vulkan/Metal acceleration, NixOS packaging, Waybar and systemd integration

**From:** Vocalinux, whisper-dictation for NixOS, hyprwhspr.

**Rejected because** every one of them is downstream of shipping a local model,
which is the first entry in this file. Nothing here is meaningful without it.

## Custom speech-model training

**From:** Deepgram's own material — custom training is reported to improve
accuracy 5–15%.

**Rejected because** splaude does not own the model, does not hold the contract,
and has no training data. Keyterm prompting is the lever available to a client,
and splaude already uses it.

## Harden the Keychain ACL around the credential

**From:** a disclosed weakness in how the Claude Code CLI stores its credential —
the item is created without access-control arguments, so any process on the
machine can read it.

**Rejected because** the weakness is in how that credential is *written*, by
another program. splaude reads its own user's credential and never persists it;
every report truncates the token to a twelve-character fingerprint
(`Crate/core/src/credential.rs:248`). There is nothing here for splaude to fix
that would not be security theatre.

## Show interim results because they feel faster

**From:** latency research — partial results shown while speaking feel more
responsive than finals, even at identical total latency.

**Not a gap.** This is what live typing already does, and it is the feature the
project is built around. Recorded here so the next sweep does not surface it as a
finding.

## Wayland injection through the GlobalShortcuts and RemoteDesktop portals

**From:** the XDG portal work — KDE and Hyprland implement GlobalShortcuts, and
the RemoteDesktop portal can carry input through libei.

**Rejected for now because** nobody has ever launched splaude on Linux. Building
portal integrations for a platform with no confirmed first run is speculation
with a compile step. The related, more promising `ydotool`/uinput route is in
Triage on the roadmap, where it belongs until someone decides whether routing
around Wayland's refusal is something this project wants to do at all.

//! User-tunable knobs.
//!
//! The Swift build backed these with `UserDefaults`, so the settings window and
//! `defaults write` were two views of the same state. Nothing portable behaves
//! like `UserDefaults`, so this is a JSON file in the platform's config
//! directory — which keeps the same property: the file and the window are two
//! views of one thing, and editing it by hand is still supported.

use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use keyboard_types::{Code, Modifiers};

/// Recogniser bias, shipped with the extension's own developer-speech list.
pub const BUILTIN_KEYTERM: [&str; 18] = [
    "VS Code",
    "IDE",
    "webview",
    "IntelliSense",
    "MCP",
    "symlink",
    "grep",
    "regex",
    "localhost",
    "codebase",
    "TypeScript",
    "JSON",
    "OAuth",
    "webhook",
    "gRPC",
    "dotfiles",
    "subagent",
    "worktree",
];

/// Applications that re-encode a keystroke by its keycode instead of reading
/// the unicode payload it carries.
///
/// Synthetic text is delivered as a keystroke on virtual key 0 whose payload is
/// the codepoint itself — `KEYEVENTF_UNICODE` on Windows, `keyboardSetUnicode
/// String` on macOS. That is what makes typing layout-independent, and every
/// native app reads it. A remote-desktop or VM client does not: it re-encodes
/// keyboard input into scancodes to ship to the far end, reads the keycode, and
/// sends whatever key 0 happens to be — on macOS that is `kVK_ANSI_A`, which is
/// why a take dictated into a Remote Desktop window arrives as a run of `a`.
///
/// Windows executable names, because the window class is useless here: these
/// apps render into one opaque HWND like every other framework does, so the
/// class says nothing about what is behind it. Compared case-insensitively and
/// without the extension — the Windows filesystem is case-insensitive, and a
/// user writing this list by hand should not have to guess the capitalisation.
pub const BUILTIN_KEYCODE_APP: [&str; 14] = [
    // Microsoft: the classic Remote Desktop Connection, the newer client that
    // ships with Windows App / Azure Virtual Desktop, and Hyper-V's console.
    "mstsc.exe",
    "msrdc.exe",
    "vmconnect.exe",
    // Citrix: the ICA engine that owns the session window, and the Desktop
    // Viewer chrome wrapped around it.
    "wfica32.exe",
    "CDViewer.exe",
    // VMware Workstation's VM console, and the Horizon view client.
    "vmware.exe",
    "vmware-view.exe",
    // VirtualBox runs each machine's window in its own process.
    "VirtualBoxVM.exe",
    // VNC viewers. The protocol carries keysyms, but the clients still read the
    // keycode off the event to work out what to send.
    "vncviewer.exe",
    "tvnviewer.exe",
    // General-purpose remote control.
    "TeamViewer.exe",
    "AnyDesk.exe",
    "rustdesk.exe",
    "parsecd.exe",
];

/// Supported by the recogniser; the wire accepts any BCP-47 tag, these are just
/// the ones worth putting in a menu.
pub const AVAILABLE_LANGUAGE: [(&str, &str); 15] = [
    ("en", "English"),
    ("en-GB", "English (UK)"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("nl", "Dutch"),
    ("hi", "Hindi"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("zh", "Chinese"),
    ("id", "Indonesian"),
    ("tl", "Tagalog"),
    ("multi", "Multilingual"),
];

const TYPING_INTERVAL_FLOOR: u32 = 200;
const TYPING_INTERVAL_CEILING: u32 = 8_000;

/// A UTF-8 byte-order mark.
///
/// Hand-editing this file is a supported way to change a setting — that is the
/// property the module header is about — so the file has to survive the editors
/// people actually have. On Windows both Notepad and PowerShell's `Out-File
/// -Encoding utf8` write these three bytes at the front, `serde_json` rejects
/// them as "expected value at line 1 column 1", and the whole file used to
/// revert to defaults over a mark the user cannot even see. Stripping it is not
/// leniency about malformed JSON; the bytes after it are the document.
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

// MARK: - Hotkey

/// A push-to-talk binding, in the W3C `KeyboardEvent.code` vocabulary.
///
/// The Swift build stored a Carbon virtual keycode, which is a macOS number
/// with no meaning anywhere else. `Code` is the one naming scheme that survives
/// the trip: it is what `global-hotkey` registers with on every platform, and
/// it is positional, so a binding still lands on the same physical key under a
/// different keyboard layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifier: Modifiers,
    pub code: Code,
}

impl Default for Hotkey {
    /// The platforms want different shapes here, and the reason is not taste.
    ///
    /// A modified binding needs the modifier held down for the OS to keep
    /// matching the registered chord. macOS obliges: synthetic events are built
    /// from a private event source and stamped with flags the injector tracks
    /// itself, so the physical modifier is never touched and `⌥/` survives a
    /// whole take. Windows has no per-event modifier mask — the only thing
    /// expressible is a real key-up — which forces a choice, and both options
    /// were tried against a live take and observed failing:
    ///
    /// - **Release it** and the OS stops matching the chord, so the still-held
    ///   key falls through to the focused window and auto-repeats. A dictation
    ///   on `Alt+Space` came back shot through with spaces, mid-word.
    /// - **Hold it** and every synthetic keystroke inherits it. `Alt+Backspace`
    ///   is undo in a great many applications, and the live-typing diff corrects
    ///   itself with backspaces — so each revision wiped the sentence before it.
    ///
    /// There is no third option, and no safe modifier: `Ctrl+Backspace` deletes
    /// a word, `Shift` uppercases everything, `Win` turns each keystroke into a
    /// shortcut. A binding with **no modifier at all** has nothing to release
    /// and nothing to inherit, which is why Windows defaults to a bare function
    /// key. `is_safe` already permits those precisely because the Swift build
    /// went out of its way to allow a bare function key.
    fn default() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self {
                modifier: Modifiers::empty(),
                code: Code::F9,
            }
        }

        // macOS keeps the chord the shipping Swift build uses, and Linux has
        // the same per-event problem as Windows in principle — but X11 is
        // untested here, so it is not given a different default on a guess.
        #[cfg(not(target_os = "windows"))]
        {
            Self {
                modifier: Modifiers::ALT,
                code: Code::Slash,
            }
        }
    }
}

impl std::fmt::Display for Hotkey {
    /// `Ctrl+Shift+KeyD`. Order is fixed so the same binding always renders the
    /// same way, whatever order the user pressed the modifiers in.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (flag, name) in Self::MODIFIER_NAME {
            if self.modifier.contains(flag) {
                write!(formatter, "{name}+")?;
            }
        }
        write!(formatter, "{}", self.code)
    }
}

impl Hotkey {
    /// Canonical order, and the spellings `from_str` accepts.
    const MODIFIER_NAME: [(Modifiers, &'static str); 4] = [
        (Modifiers::CONTROL, "Ctrl"),
        (Modifiers::ALT, "Alt"),
        (Modifiers::SHIFT, "Shift"),
        (Modifiers::META, "Meta"),
    ];

    /// A binding with no modifier is legal — a bare function key is a perfectly
    /// good push-to-talk key, and the Swift build went out of its way to allow
    /// it. Anything else unmodified would swallow a character the user needs.
    pub fn is_safe(&self) -> bool {
        if !self.modifier.is_empty() {
            return true;
        }
        matches!(
            self.code,
            Code::F1
                | Code::F2
                | Code::F3
                | Code::F4
                | Code::F5
                | Code::F6
                | Code::F7
                | Code::F8
                | Code::F9
                | Code::F10
                | Code::F11
                | Code::F12
                | Code::F13
                | Code::F14
                | Code::F15
                | Code::F16
                | Code::F17
                | Code::F18
                | Code::F19
                | Code::F20
        )
    }
}

impl FromStr for Hotkey {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut modifier = Modifiers::empty();
        let mut code: Option<Code> = None;

        for token in text.split('+').map(str::trim).filter(|t| !t.is_empty()) {
            let matched = Self::MODIFIER_NAME
                .iter()
                .find(|(_, name)| name.eq_ignore_ascii_case(token))
                // Accept the platform spellings people actually type.
                .or_else(|| match token.to_ascii_lowercase().as_str() {
                    "control" => Some(&Self::MODIFIER_NAME[0]),
                    "option" | "opt" => Some(&Self::MODIFIER_NAME[1]),
                    "cmd" | "command" | "win" | "super" => Some(&Self::MODIFIER_NAME[3]),
                    _ => None,
                });

            match matched {
                Some((flag, _)) => modifier |= *flag,
                None => {
                    code = Some(
                        Code::from_str(token).map_err(|_| format!("unknown key \"{token}\""))?,
                    );
                }
            }
        }

        code.map(|code| Hotkey { modifier, code })
            .ok_or_else(|| format!("no key in \"{text}\""))
    }
}

impl Serialize for Hotkey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        // A binding that no longer parses must not wipe the rest of the file.
        Ok(Hotkey::from_str(&text).unwrap_or_default())
    }
}

// MARK: - Setting

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Setting {
    // Recognition
    /// Terms the user added. Kept separate so the built-in list is never lost.
    pub custom_keyterm: Vec<String>,
    pub use_builtin_keyterm: bool,
    /// Bias the recogniser with the project Claude Code was last used in.
    ///
    /// The IDE extension does this from its own workspace; splaude has none, so
    /// it infers one. See [`crate::project`] for what is read and why.
    pub use_project_keyterm: bool,
    /// A JSON inventory of this machine's own infrastructure, harvested for the
    /// proper nouns in it — hosts, sites, databases, repos that live only on a
    /// server. `None` probes the locations splaude already knows about; see
    /// [`crate::project::catalog_keyterm`].
    pub catalog_path: Option<PathBuf>,
    pub language: String,

    // Output
    /// Types into the focused app as you speak, rewriting revised words.
    pub live_typing: bool,
    /// Microseconds between synthetic keystrokes. Lower is snappier; too low
    /// and Electron apps start dropping characters.
    pub typing_interval: u32,
    /// Refuse to type into surfaces the OS says are not text.
    pub guard_focus: bool,
    /// End the take when Return is pressed.
    ///
    /// Submitting is a statement that you are done talking — in a chat box or a
    /// search field the next words would land somewhere you cannot see. Worth
    /// turning off when dictating prose, where Return is just a new paragraph.
    pub stop_on_return: bool,
    /// Executables the user added to [`BUILTIN_KEYCODE_APP`]. Kept separate for
    /// the same reason `custom_keyterm` is: adding one must never cost the
    /// built-ins.
    pub custom_keycode_app: Vec<String>,
    pub use_builtin_keycode_app: bool,
    /// Pin a take to the field it started in, rather than following focus.
    ///
    /// Off means keystrokes go wherever focus is when they are posted, so
    /// changing window mid-sentence splits a dictation across both.
    pub anchor_input: bool,

    // Interface
    pub show_floating_button: bool,
    pub floating_button_point: Option<Point>,
    pub play_sound: bool,

    // Hotkey
    pub hotkey: Hotkey,

    /// Intent, not truth. Unlike macOS's `SMAppService`, the Windows registry
    /// key and the XDG autostart file cannot report whether *this* build
    /// installed them, so the platform layer reconciles the machine to this
    /// value at launch rather than the other way round.
    pub launch_at_login: bool,
}

impl Default for Setting {
    fn default() -> Self {
        Self {
            custom_keyterm: Vec::new(),
            use_builtin_keyterm: true,
            use_project_keyterm: true,
            catalog_path: None,
            language: "en".into(),
            live_typing: true,
            typing_interval: 1_200,
            guard_focus: true,
            stop_on_return: true,
            custom_keycode_app: Vec::new(),
            use_builtin_keycode_app: true,
            anchor_input: true,
            show_floating_button: true,
            floating_button_point: None,
            play_sound: false,
            hotkey: Hotkey::default(),
            launch_at_login: false,
        }
    }
}

/// An executable name reduced to what is worth comparing.
///
/// Strips the `.exe`, because a user writing this list by hand will leave it off
/// as often as not and both spellings mean the same process. Case is left to the
/// caller's `eq_ignore_ascii_case`, which is the right comparison on Windows —
/// `MSTSC.EXE` and `mstsc.exe` are one file there.
fn bare_executable(name: &str) -> &str {
    let name = name.trim();
    match name.len().checked_sub(4) {
        Some(stem) if name[stem..].eq_ignore_ascii_case(".exe") => &name[..stem],
        _ => name,
    }
}

impl Setting {
    /// What actually goes on the wire, project bias interleaved.
    ///
    /// Packing truncates at the wire budget rather than sampling, so this order
    /// *is* the priority. Terms the user typed lead: they were added because
    /// something was being misheard, and nothing harvested outranks that. See
    /// [`crate::project::Harvest`] for why the builtin list sits in the middle
    /// rather than at either end.
    pub fn wire_keyterm(&self) -> Vec<String> {
        self.wire_keyterm_from(crate::project::cached_harvest(self.catalog_path.as_deref()))
    }

    /// The wire list computed synchronously, for `--check`.
    ///
    /// The live path reads a background-warmed cache so a take never waits; a
    /// one-shot `--check` process has no warmed cache and would otherwise report
    /// the builtin list alone — the opposite of what a diagnostic meant to show
    /// the harvested bias should print.
    pub fn wire_keyterm_sync(&self) -> Vec<String> {
        self.wire_keyterm_from(crate::project::harvest(self.catalog_path.as_deref()))
    }

    fn wire_keyterm_from(&self, harvest: crate::project::Harvest) -> Vec<String> {
        let harvest = if self.use_project_keyterm {
            harvest
        } else {
            crate::project::Harvest::default()
        };

        let builtin: Vec<String> = if self.use_builtin_keyterm {
            BUILTIN_KEYTERM
                .iter()
                .map(|term| term.to_string())
                .collect()
        } else {
            Vec::new()
        };

        [
            self.custom_keyterm.clone(),
            harvest.identity,
            builtin,
            harvest.house,
            harvest.vocabulary,
        ]
        .concat()
    }

    /// The configured terms alone — builtin plus custom, no filesystem read.
    pub fn keyterm(&self) -> Vec<String> {
        let builtin = if self.use_builtin_keyterm {
            BUILTIN_KEYTERM
                .iter()
                .map(|term| term.to_string())
                .collect()
        } else {
            Vec::new()
        };
        [builtin, self.custom_keyterm.clone()].concat()
    }

    /// Every executable that is known to re-encode keystrokes by keycode.
    ///
    /// Composed exactly like [`Setting::keyterm`]: built-ins first, then the
    /// user's own, so turning the built-ins off is a deliberate act rather than
    /// a side effect of adding an entry.
    pub fn keycode_app(&self) -> Vec<String> {
        let builtin = if self.use_builtin_keycode_app {
            BUILTIN_KEYCODE_APP
                .iter()
                .map(|name| name.to_string())
                .collect()
        } else {
            Vec::new()
        };
        [builtin, self.custom_keycode_app.clone()].concat()
    }

    /// Whether `executable` names an application that would turn our unicode
    /// keystrokes into the wrong characters.
    ///
    /// Pure over the name, so the whole carve-out is testable with no desktop,
    /// no focused window and no remote-desktop session.
    pub fn is_keycode_app(&self, executable: &str) -> bool {
        let name = bare_executable(executable);
        if name.is_empty() {
            return false;
        }
        self.keycode_app()
            .iter()
            .any(|known| bare_executable(known).eq_ignore_ascii_case(name))
    }

    /// `%APPDATA%\splaude\setting.json`, `~/Library/Application Support/…`,
    /// `~/.config/splaude/…`.
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("splaude")
            .join("setting.json")
    }

    /// Parses the bytes of a settings file, normalised.
    ///
    /// Pure over the bytes, so both the mark [`BOM`] describes and a genuine
    /// typo can be exercised without a config directory.
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        let body = data.strip_prefix(&BOM).unwrap_or(data);
        let mut setting: Self = serde_json::from_slice(body).map_err(|error| format!("{error}"))?;
        setting.normalise();
        Ok(setting)
    }

    /// Reads the file, saying what is wrong with it rather than only logging.
    ///
    /// The second half of the pair is `None` when the file parsed, or when
    /// there is no file at all — a first run is not a complaint. It is `Some`
    /// exactly when a file exists and could not be read, which is the case that
    /// used to be invisible: a mistyped comma cost the user their hotkey and
    /// their keyterms, and the only evidence was a line in a log they had no
    /// reason to open.
    ///
    /// The setting handed back in that case is still defaults, because at
    /// startup there is nothing else to fall back to. A caller that already
    /// holds a live setting — the reload in `splaude-app` — must keep its own
    /// and act on the note instead. **Nothing here rewrites the file**: the
    /// broken text is the user's edit, and it is what they need to see to fix
    /// it.
    pub fn load_checked() -> (Self, Option<String>) {
        let Ok(data) = std::fs::read(Self::path()) else {
            return (Self::default(), None);
        };

        match Self::parse(&data) {
            Ok(setting) => (setting, None),
            // Named by file rather than by full path: this sentence goes in a
            // menu, and a menu is as wide as its widest item.
            Err(error) => (
                Self::default(),
                Some(format!("setting.json is not valid JSON — {error}")),
            ),
        }
    }

    /// Reads the file, falling back to defaults.
    ///
    /// A corrupt or partial file is not an error worth blocking startup for —
    /// `serde(default)` fills every missing field, and an unparsable file logs
    /// and yields defaults rather than leaving the user with no dictation and
    /// no way to fix it except deleting a file they cannot find.
    pub fn load() -> Self {
        let (setting, note) = Self::load_checked();
        if let Some(note) = note {
            crate::diagnostic::log(
                "setting",
                format!("{} — using defaults ({})", note, Self::path().display()),
            );
        }
        setting
    }

    /// The fields that differ, spelled as the file spells them.
    ///
    /// For the reload log. A reload that silently does nothing is
    /// indistinguishable from a reload that did not happen, and naming the keys
    /// in the file's own `camelCase` is what lets a user match the log line to
    /// the line they edited.
    pub fn difference(&self, other: &Self) -> Vec<String> {
        let mut changed: Vec<String> = Vec::new();

        for (name, differs) in [
            ("customKeyterm", self.custom_keyterm != other.custom_keyterm),
            (
                "useBuiltinKeyterm",
                self.use_builtin_keyterm != other.use_builtin_keyterm,
            ),
            (
                "useProjectKeyterm",
                self.use_project_keyterm != other.use_project_keyterm,
            ),
            ("catalogPath", self.catalog_path != other.catalog_path),
            ("language", self.language != other.language),
            ("liveTyping", self.live_typing != other.live_typing),
            (
                "typingInterval",
                self.typing_interval != other.typing_interval,
            ),
            ("guardFocus", self.guard_focus != other.guard_focus),
            ("stopOnReturn", self.stop_on_return != other.stop_on_return),
            (
                "customKeycodeApp",
                self.custom_keycode_app != other.custom_keycode_app,
            ),
            (
                "useBuiltinKeycodeApp",
                self.use_builtin_keycode_app != other.use_builtin_keycode_app,
            ),
            ("anchorInput", self.anchor_input != other.anchor_input),
            (
                "showFloatingButton",
                self.show_floating_button != other.show_floating_button,
            ),
            (
                "floatingButtonPoint",
                self.floating_button_point != other.floating_button_point,
            ),
            ("playSound", self.play_sound != other.play_sound),
            ("hotkey", self.hotkey != other.hotkey),
            (
                "launchAtLogin",
                self.launch_at_login != other.launch_at_login,
            ),
        ] {
            if differs {
                changed.push(name.to_string());
            }
        }

        changed
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut copy = self.clone();
        copy.normalise();
        let body = serde_json::to_vec_pretty(&copy)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        std::fs::write(&path, body)
    }

    /// Clamps anything a hand-edited file could put out of range.
    pub fn normalise(&mut self) {
        self.typing_interval = self
            .typing_interval
            .clamp(TYPING_INTERVAL_FLOOR, TYPING_INTERVAL_CEILING);
        if self.language.trim().is_empty() {
            self.language = "en".into();
        }
        if !self.hotkey.is_safe() {
            crate::diagnostic::log(
                "setting",
                format!("{} needs a modifier — falling back", self.hotkey),
            );
            self.hotkey = Hotkey::default();
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// The default differs by platform on purpose — see `Hotkey::default`. The
    /// property that matters on Windows is not *which* key it is but that it
    /// carries **no modifier**, since a modified binding cannot be delivered
    /// safely there: releasing the modifier leaks the bound key into the take,
    /// and holding it turns every corrective backspace into undo.
    #[test]
    fn the_default_binding_suits_its_platform() {
        let default = Hotkey::default();
        assert!(default.is_safe(), "a default that is_safe rejects is a bug");

        if cfg!(target_os = "windows") {
            assert!(
                default.modifier.is_empty(),
                "a modified default is unusable on Windows, got {default}"
            );
        } else {
            assert_eq!(default.to_string(), "Alt+Slash");
        }
    }

    #[test]
    fn round_trips_a_binding_through_text() {
        for text in [
            "Alt+Slash",
            "Alt+Space",
            "Ctrl+Shift+KeyD",
            "F13",
            "Ctrl+Alt+Shift+Meta+KeyA",
        ] {
            let parsed: Hotkey = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text, "round trip of {text}");
        }
    }

    #[test]
    fn accepts_the_platform_spellings_people_type() {
        assert_eq!(
            "Option+Slash".parse::<Hotkey>().unwrap(),
            "Alt+Slash".parse::<Hotkey>().unwrap(),
            "Option is what a mac user calls Alt"
        );
        assert_eq!(
            "Cmd+KeyD".parse::<Hotkey>().unwrap(),
            "Meta+KeyD".parse::<Hotkey>().unwrap()
        );
        assert_eq!(
            "win+KeyD".parse::<Hotkey>().unwrap(),
            "Meta+KeyD".parse::<Hotkey>().unwrap()
        );
    }

    #[test]
    fn rejects_a_binding_with_no_key() {
        assert!("Ctrl+Alt".parse::<Hotkey>().is_err());
        assert!("Nonsense+Space".parse::<Hotkey>().is_err());
    }

    #[test]
    fn a_bare_function_key_is_a_legal_binding() {
        // The Swift build allowed a zero modifier specifically so bare function
        // keys worked; losing that in the port would be a silent regression.
        let bare: Hotkey = "F13".parse().unwrap();
        assert!(bare.modifier.is_empty());
        assert!(bare.is_safe());
    }

    #[test]
    fn a_bare_letter_is_not_a_legal_binding() {
        let bare: Hotkey = "KeyA".parse().unwrap();
        assert!(!bare.is_safe());
    }

    #[test]
    fn an_unsafe_binding_falls_back_on_normalise() {
        let mut setting = Setting {
            hotkey: "KeyA".parse().unwrap(),
            ..Setting::default()
        };
        setting.normalise();
        assert_eq!(setting.hotkey, Hotkey::default());
    }

    #[test]
    fn wire_keyterm_puts_custom_first_and_builtin_before_the_house() {
        // Harvesting is off, so this asserts the tier order alone and does not
        // depend on what is on the machine running the test.
        let setting = Setting {
            custom_keyterm: vec!["Ateneo".into()],
            use_builtin_keyterm: true,
            use_project_keyterm: false,
            ..Default::default()
        };
        let wire = setting.wire_keyterm();
        assert_eq!(wire.first().unwrap(), "Ateneo");
        assert_eq!(wire[1], BUILTIN_KEYTERM[0]);
        assert_eq!(wire.len(), BUILTIN_KEYTERM.len() + 1);
    }

    #[test]
    fn keyterm_is_builtin_plus_custom() {
        let setting = Setting {
            custom_keyterm: vec!["splaude".into()],
            ..Setting::default()
        };
        let keyterm = setting.keyterm();
        assert_eq!(keyterm.len(), BUILTIN_KEYTERM.len() + 1);
        assert_eq!(keyterm.last().unwrap(), "splaude");
        assert!(keyterm.contains(&"gRPC".to_string()));
    }

    #[test]
    fn builtin_keyterm_can_be_turned_off_without_losing_custom() {
        let setting = Setting {
            custom_keyterm: vec!["splaude".into()],
            use_builtin_keyterm: false,
            ..Setting::default()
        };
        assert_eq!(setting.keyterm(), vec!["splaude".to_string()]);
    }

    #[test]
    fn the_builtin_remote_desktop_client_is_a_keycode_app() {
        let setting = Setting::default();
        assert!(setting.is_keycode_app("mstsc.exe"));
        assert!(setting.is_keycode_app("msrdc.exe"));
        assert!(setting.is_keycode_app("VirtualBoxVM.exe"));
    }

    #[test]
    fn an_ordinary_app_is_left_alone() {
        // The carve-out must stay a carve-out: everything else still gets live
        // typing exactly as before.
        let setting = Setting::default();
        for name in [
            "Code.exe",
            "notepad.exe",
            "chrome.exe",
            "WindowsTerminal.exe",
            "slack.exe",
            "",
            "   ",
            // Not a prefix or substring match — only the whole name counts.
            "mstscd.exe",
            "notmstsc.exe",
        ] {
            assert!(!setting.is_keycode_app(name), "{name}");
        }
    }

    #[test]
    fn keycode_app_matching_ignores_case_and_the_extension() {
        // Windows filenames are case-insensitive, and a hand-written list will
        // spell them however the user remembers them.
        let setting = Setting::default();
        for name in ["MSTSC.EXE", "MsTsC.exe", "mstsc", "  mstsc.exe  "] {
            assert!(setting.is_keycode_app(name), "{name}");
        }
    }

    #[test]
    fn keycode_app_is_builtin_plus_custom() {
        let setting = Setting {
            custom_keycode_app: vec!["Ericom.exe".into()],
            ..Setting::default()
        };
        assert_eq!(
            setting.keycode_app().len(),
            BUILTIN_KEYCODE_APP.len() + 1,
            "adding one must not cost the built-ins"
        );
        assert!(setting.is_keycode_app("ericom.exe"));
        assert!(setting.is_keycode_app("mstsc.exe"));
    }

    #[test]
    fn builtin_keycode_app_can_be_turned_off_without_losing_custom() {
        let setting = Setting {
            custom_keycode_app: vec!["Ericom.exe".into()],
            use_builtin_keycode_app: false,
            ..Setting::default()
        };
        assert_eq!(setting.keycode_app(), vec!["Ericom.exe".to_string()]);
        assert!(setting.is_keycode_app("Ericom.exe"));
        assert!(!setting.is_keycode_app("mstsc.exe"));
    }

    #[test]
    fn a_file_written_before_the_keycode_carve_out_still_loads() {
        // `serde(default)` is what keeps an existing setting.json working.
        let setting: Setting = serde_json::from_str(r#"{"language":"ja"}"#).unwrap();
        assert!(setting.use_builtin_keycode_app);
        assert!(setting.custom_keycode_app.is_empty());
        assert!(setting.is_keycode_app("mstsc.exe"));
    }

    #[test]
    fn clamps_a_hand_edited_typing_interval() {
        let mut fast = Setting {
            typing_interval: 1,
            ..Setting::default()
        };
        fast.normalise();
        assert_eq!(fast.typing_interval, TYPING_INTERVAL_FLOOR);

        let mut slow = Setting {
            typing_interval: 999_999,
            ..Setting::default()
        };
        slow.normalise();
        assert_eq!(slow.typing_interval, TYPING_INTERVAL_CEILING);
    }

    #[test]
    fn a_partial_file_fills_from_defaults() {
        let setting: Setting = serde_json::from_str(r#"{"language":"ja"}"#).unwrap();
        assert_eq!(setting.language, "ja");
        assert!(setting.live_typing);
        assert_eq!(setting.hotkey, Hotkey::default());
    }

    #[test]
    fn an_unparsable_binding_does_not_wipe_the_file() {
        let setting: Setting =
            serde_json::from_str(r#"{"language":"fr","hotkey":"Ctrl+Nonsense"}"#).unwrap();
        assert_eq!(setting.language, "fr");
        assert_eq!(setting.hotkey, Hotkey::default());
    }

    #[test]
    fn a_byte_order_mark_does_not_disable_the_file() {
        // Notepad and `Out-File -Encoding utf8` both write one, and it used to
        // take every setting in the file down with it — silently, because the
        // mark is invisible in the editor that added it.
        let body = br#"{"language":"ja","typingInterval":2000}"#;
        let mut marked = BOM.to_vec();
        marked.extend_from_slice(body);

        let plain = Setting::parse(body).expect("plain JSON should parse");
        let with_mark = Setting::parse(&marked).expect("a BOM should not disable the file");
        assert_eq!(with_mark, plain);
        assert_eq!(with_mark.language, "ja");
        assert_eq!(with_mark.typing_interval, 2_000);
    }

    #[test]
    fn only_a_leading_mark_is_stripped() {
        // The mark is a prefix, not a character class. One in the middle of the
        // document is still a syntax error, and pretending otherwise would be
        // silently accepting a file we did not understand.
        let mut trailing = br#"{"language":"ja"}"#.to_vec();
        trailing.extend_from_slice(&BOM);
        assert!(Setting::parse(&trailing).is_err());
    }

    #[test]
    fn a_typo_is_reported_rather_than_swallowed() {
        // The other half of the same complaint: losing a hotkey and a keyterm
        // list to a missed comma must at least be visible.
        let error = Setting::parse(br#"{"language":"ja" "liveTyping":false}"#)
            .expect_err("a missing comma is not valid JSON");
        assert!(error.contains("line 1"), "{error}");
    }

    #[test]
    fn parsing_normalises_what_it_read() {
        // `load` used to be the only place that normalised, so anything reading
        // the bytes directly got an unclamped interval.
        let setting = Setting::parse(br#"{"typingInterval":1}"#).unwrap();
        assert_eq!(setting.typing_interval, TYPING_INTERVAL_FLOOR);
    }

    #[test]
    fn an_unchanged_setting_has_no_difference() {
        let setting = Setting::default();
        assert!(setting.difference(&setting).is_empty());
    }

    #[test]
    fn difference_names_every_field_the_file_spells() {
        // One entry per field, or a reload would apply something it never
        // mentioned. Compared against a value that differs in all of them.
        let other = Setting {
            custom_keyterm: vec!["splaude".into()],
            use_builtin_keyterm: false,
            use_project_keyterm: false,
            catalog_path: Some("/tmp/inventory.json".into()),
            language: "ja".into(),
            live_typing: false,
            typing_interval: 2_000,
            guard_focus: false,
            stop_on_return: false,
            custom_keycode_app: vec!["Ericom.exe".into()],
            use_builtin_keycode_app: false,
            anchor_input: false,
            show_floating_button: false,
            floating_button_point: Some(Point { x: 1.0, y: 2.0 }),
            play_sound: true,
            hotkey: "Ctrl+Shift+KeyD".parse().unwrap(),
            launch_at_login: true,
        };

        let named = Setting::default().difference(&other);
        assert_eq!(
            named.len(),
            17,
            "a field missing from difference is a field a reload changes in silence: {named:?}"
        );
        assert!(named.contains(&"hotkey".to_string()));
        assert!(named.contains(&"typingInterval".to_string()));
    }

    #[test]
    fn difference_names_only_what_changed() {
        let other = Setting {
            language: "fr".into(),
            ..Setting::default()
        };
        assert_eq!(Setting::default().difference(&other), vec!["language"]);
    }

    #[test]
    fn survives_a_full_json_round_trip() {
        let original = Setting {
            custom_keyterm: vec!["orsem".into()],
            hotkey: "Ctrl+Shift+KeyD".parse().unwrap(),
            floating_button_point: Some(Point { x: 12.0, y: 34.0 }),
            ..Setting::default()
        };

        let text = serde_json::to_string(&original).unwrap();
        let back: Setting = serde_json::from_str(&text).unwrap();
        assert_eq!(back, original);
    }
}

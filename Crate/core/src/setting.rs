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
    /// Alt+Space — the same physical chord as the Swift build's Option+Space.
    fn default() -> Self {
        Self {
            modifier: Modifiers::ALT,
            code: Code::Space,
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
            language: "en".into(),
            live_typing: true,
            typing_interval: 1_200,
            guard_focus: true,
            stop_on_return: true,
            anchor_input: true,
            show_floating_button: true,
            floating_button_point: None,
            play_sound: false,
            hotkey: Hotkey::default(),
            launch_at_login: false,
        }
    }
}

impl Setting {
    /// What actually goes on the wire.
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

    /// `%APPDATA%\splaude\setting.json`, `~/Library/Application Support/…`,
    /// `~/.config/splaude/…`.
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("splaude")
            .join("setting.json")
    }

    /// Reads the file, falling back to defaults.
    ///
    /// A corrupt or partial file is not an error worth blocking startup for —
    /// `serde(default)` fills every missing field, and an unparsable file logs
    /// and yields defaults rather than leaving the user with no dictation and
    /// no way to fix it except deleting a file they cannot find.
    pub fn load() -> Self {
        let path = Self::path();
        let mut setting = match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|error| {
                crate::diagnostic::log(
                    "setting",
                    format!("{} unreadable ({error}) — using defaults", path.display()),
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        setting.normalise();
        setting
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

    #[test]
    fn default_binding_is_alt_space() {
        assert_eq!(Hotkey::default().to_string(), "Alt+Space");
    }

    #[test]
    fn round_trips_a_binding_through_text() {
        for text in [
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
            "Option+Space".parse::<Hotkey>().unwrap(),
            Hotkey::default(),
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

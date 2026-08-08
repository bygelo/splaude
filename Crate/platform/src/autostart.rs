//! Launch at login.
//!
//! The Swift build asked `SMAppService` whether *this app bundle* was
//! registered, so its toggle could read the machine directly. Nothing else
//! offers that: a `Run` value or an autostart `.desktop` is just a path some
//! installer wrote, with no identity attached. So [`is_enabled`] answers the
//! only question that is actually answerable — does the entry point at the
//! executable running right now — and the core stores the toggle as intent that
//! this module reconciles the machine to (see `splaude_core::setting`).
//!
//! That path comparison is not pedantry. An entry left behind by an old install
//! location launches nothing, or worse, launches a stale build; reporting it as
//! "enabled" would leave the user with a checked box and no app at login.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Registry value name, LaunchAgent label stem, and `.desktop` stem.
const NAME: &str = "splaude";

/// Whether splaude is currently registered to start with the machine.
pub fn is_enabled() -> bool {
    // Never propagates: a caller drawing a checkbox has no better answer than
    // "not as far as we can tell".
    read().unwrap_or(false)
}

pub fn set(enabled: bool) -> Result<()> {
    if enabled {
        install()
    } else {
        uninstall()
    }
}

// MARK: - Executable

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("cannot locate the running executable")
}

/// The exe path as text, rejecting anything that cannot survive being embedded
/// in a registry value, an XML string or a single line of an INI file.
///
/// Failing here beats writing an entry that half-parses: a truncated command
/// line silently launches the wrong thing, or nothing, at every login.
fn exe_text(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    if text.is_empty() {
        bail!("executable path is empty");
    }
    if text.chars().any(char::is_control) {
        bail!("executable path contains a control character: {text}");
    }
    Ok(text.to_string())
}

// MARK: - Rendering
//
// Pure string and path builders, compiled on every host so the tests cover all
// three formats wherever they run — CI only ever exercises one of the three
// `set` paths below.
#[allow(dead_code)]
mod render {
    use std::path::{Path, PathBuf};

    use anyhow::{bail, Result};

    use super::NAME;

    /// Reverse-DNS, matching the Swift bundle identifier.
    pub const LABEL: &str = "com.bygelo.splaude";

    /// What goes in `HKCU\…\Run`.
    ///
    /// Always quoted: the install path contains "Program Files", and an
    /// unquoted space makes the loader try `C:\Program.exe` first.
    pub fn registry_command(exe: &str) -> Result<String> {
        // A quote inside the path would close the quoting early; there is no
        // escape for it in a `Run` value, so there is nothing to do but refuse.
        if exe.contains('"') {
            bail!("executable path contains a quote, which a Run value cannot express: {exe}");
        }
        Ok(format!("\"{exe}\""))
    }

    /// Whether a stored `Run` value launches `exe`.
    ///
    /// Compared as paths, not strings: the value may or may not be quoted
    /// depending on who wrote it, and Windows paths are case-insensitive, so a
    /// byte comparison would report a perfectly good entry as stale and
    /// rewrite it on every launch.
    pub fn is_command_for(stored: &str, exe: &str) -> bool {
        let unquote = |text: &str| {
            let trimmed = text.trim();
            trimmed
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(trimmed)
                .to_string()
        };
        let stored = unquote(stored);
        stored.eq_ignore_ascii_case(&unquote(exe))
    }

    /// `~/Library/LaunchAgents/com.bygelo.splaude.plist`.
    pub fn agent_path_in(home: &Path) -> PathBuf {
        home.join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    /// A minimal `launchd` agent: label, argv, run at load.
    ///
    /// Written as XML rather than through `SMAppService`, which would drag an
    /// FFI dependency in for one boolean. Note that the shipping Swift app
    /// *does* use `SMAppService`, so the two builds register through different
    /// mechanisms and neither can see the other's registration — installing
    /// both means two login items and two running copies.
    pub fn plist(exe: &str) -> String {
        let argument = plist_argument(exe);
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
{argument}
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#
        )
    }

    /// The one line naming the executable the agent launches — also what
    /// [`super::read`] matches on, so an agent left behind by an old install
    /// location reads as not-enabled.
    pub fn plist_argument(exe: &str) -> String {
        format!("\t\t<string>{}</string>", escape_xml(exe))
    }

    /// Only the three characters that can end a `<string>` early or start an
    /// entity; quotes and apostrophes are legal in element content.
    fn escape_xml(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for character in text.chars() {
            match character {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                _ => out.push(character),
            }
        }
        out
    }

    /// `$XDG_CONFIG_HOME/autostart/splaude.desktop`.
    pub fn desktop_path_in(config_root: &Path) -> PathBuf {
        config_root
            .join("autostart")
            .join(format!("{NAME}.desktop"))
    }

    /// An XDG autostart entry.
    ///
    /// `X-GNOME-Autostart-enabled` is not in the spec but GNOME writes it and
    /// its absence is read as disabled by some tooling, so it is stated.
    pub fn desktop_entry(exe: &str) -> Result<String> {
        let exec = desktop_exec(exe)?;
        Ok(format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={NAME}\n\
             Exec={exec}\n\
             X-GNOME-Autostart-enabled=true\n"
        ))
    }

    /// The `Exec=` value, under the desktop-entry quoting rules.
    ///
    /// Reserved characters have a defined escape, so quoting is enough — but a
    /// path we cannot express is a refusal, not a best effort.
    pub fn desktop_exec(exe: &str) -> Result<String> {
        if exe.contains('\n') || exe.contains('\r') {
            bail!("executable path spans lines, which a desktop entry cannot express");
        }
        // Reserved by the spec inside a quoted argument.
        let needs_quoting = exe
            .chars()
            .any(|character| character.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(character));
        if !needs_quoting {
            return Ok(exe.to_string());
        }
        let mut out = String::with_capacity(exe.len() + 2);
        out.push('"');
        for character in exe.chars() {
            if matches!(character, '"' | '\\' | '$' | '`') {
                out.push('\\');
            }
            out.push(character);
        }
        out.push('"');
        Ok(out)
    }
}

// MARK: - Windows

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn read() -> Result<bool> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    use winreg::RegKey;

    let exe = exe_text(&current_exe()?)?;
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ)?;
    let stored: String = key.get_value(NAME)?;
    Ok(render::is_command_for(&stored, &exe))
}

#[cfg(windows)]
fn install() -> Result<()> {
    use winreg::RegKey;

    let command = render::registry_command(&exe_text(&current_exe()?)?)?;
    // `create_subkey` opens the existing key; Run always exists in practice,
    // but a fresh profile is not worth failing over.
    let (key, _) = RegKey::predef(winreg::enums::HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    key.set_value(NAME, &command)
        .with_context(|| format!(r"cannot write HKCU\{RUN_KEY}\{NAME}"))
}

#[cfg(windows)]
fn uninstall() -> Result<()> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;

    let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
    {
        Ok(key) => key,
        // No key means nothing to remove, which is the state asked for.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("cannot open the Run key for writing"),
    };
    match key.delete_value(NAME) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!(r"cannot delete HKCU\{RUN_KEY}\{NAME}")),
    }
}

// MARK: - macOS

#[cfg(target_os = "macos")]
fn agent_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(render::agent_path_in(&home))
}

#[cfg(target_os = "macos")]
fn read() -> Result<bool> {
    let exe = exe_text(&current_exe()?)?;
    let body = std::fs::read_to_string(agent_path()?)?;
    // Same rule as the registry: an agent pointing at an old install location
    // is not this build's login item, whatever the file is named.
    let expected = render::plist_argument(&exe);
    Ok(body.lines().any(|line| line.trim_end() == expected))
}

#[cfg(target_os = "macos")]
fn install() -> Result<()> {
    let body = render::plist(&exe_text(&current_exe()?)?);
    write_entry(&agent_path()?, &body)
}

#[cfg(target_os = "macos")]
fn uninstall() -> Result<()> {
    remove_entry(&agent_path()?)
}

// MARK: - Linux and other unix

#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_path() -> Result<PathBuf> {
    // `dirs` already applies the XDG_CONFIG_HOME-then-~/.config rule.
    let config_root = dirs::config_dir().context("no config directory")?;
    Ok(render::desktop_path_in(&config_root))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn read() -> Result<bool> {
    let exe = exe_text(&current_exe()?)?;
    let body = std::fs::read_to_string(desktop_path()?)?;
    let expected = format!("Exec={}", render::desktop_exec(&exe)?);
    // Line-exact so a stale Exec reads as not-enabled, and so a path that is a
    // prefix of another does not match.
    Ok(body.lines().any(|line| line.trim_end() == expected))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn install() -> Result<()> {
    let body = render::desktop_entry(&exe_text(&current_exe()?)?)?;
    write_entry(&desktop_path()?, &body)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn uninstall() -> Result<()> {
    remove_entry(&desktop_path()?)
}

// MARK: - File-backed entry

#[cfg(unix)]
fn write_entry(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(path, body).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(unix)]
fn remove_entry(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        // Already absent is the state asked for, not a failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot remove {}", path.display())),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const SPACED: &str = r"C:\Program Files\splaude\splaude.exe";

    // Nothing here touches the registry, ~/Library/LaunchAgents or
    // ~/.config/autostart — only the pure builders above are exercised, so the
    // suite is safe to run on a developer's own machine.

    #[test]
    fn a_registry_command_quotes_a_path_with_a_space() {
        assert_eq!(
            render::registry_command(SPACED).unwrap(),
            r#""C:\Program Files\splaude\splaude.exe""#
        );
    }

    #[test]
    fn a_path_containing_a_quote_is_refused_rather_than_written() {
        assert!(render::registry_command(r#"C:\od"d\splaude.exe"#).is_err());
    }

    #[test]
    fn a_stored_command_matches_its_own_executable() {
        let command = render::registry_command(SPACED).unwrap();
        assert!(render::is_command_for(&command, SPACED));
        // Unquoted and differently cased entries are still this build's.
        assert!(render::is_command_for(SPACED, SPACED));
        assert!(render::is_command_for(&command.to_uppercase(), SPACED));
    }

    #[test]
    fn a_stale_install_location_does_not_count_as_enabled() {
        let old =
            render::registry_command(r"C:\Users\a\AppData\Local\splaude\splaude.exe").unwrap();
        assert!(!render::is_command_for(&old, SPACED));
    }

    #[test]
    fn a_plist_carries_the_label_argv_and_run_at_load() {
        let body = render::plist("/Applications/splaude.app/Contents/MacOS/splaude");
        assert!(body.contains("<string>com.bygelo.splaude</string>"));
        assert!(body.contains("<string>/Applications/splaude.app/Contents/MacOS/splaude</string>"));
        assert!(body.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(body.starts_with("<?xml"));
    }

    #[test]
    fn a_plist_path_with_a_space_needs_no_quoting_but_markup_is_escaped() {
        let body = render::plist("/Users/a/My Apps/splaude & co");
        assert!(body.contains("<string>/Users/a/My Apps/splaude &amp; co</string>"));
        assert!(!body.contains("& co"));
        // The argument line the enabled check matches must be in the file it
        // writes, or a fresh install would immediately read as not-enabled.
        let argument = render::plist_argument("/Users/a/My Apps/splaude & co");
        assert!(body.lines().any(|line| line == argument));
    }

    #[test]
    fn an_agent_path_lands_in_launch_agents() {
        let path = render::agent_path_in(Path::new("/Users/a"));
        assert!(path.ends_with("Library/LaunchAgents/com.bygelo.splaude.plist"));
    }

    #[test]
    fn a_desktop_entry_declares_type_name_exec_and_gnome_autostart() {
        let body = render::desktop_entry("/usr/local/bin/splaude").unwrap();
        assert!(body.starts_with("[Desktop Entry]\n"));
        for line in [
            "Type=Application",
            "Name=splaude",
            "Exec=/usr/local/bin/splaude",
            "X-GNOME-Autostart-enabled=true",
        ] {
            assert!(body.lines().any(|have| have == line), "missing {line}");
        }
    }

    #[test]
    fn a_desktop_exec_quotes_a_path_with_a_space() {
        assert_eq!(
            render::desktop_exec("/opt/My Apps/splaude").unwrap(),
            r#""/opt/My Apps/splaude""#
        );
        // Reserved characters get a backslash inside the quotes.
        assert_eq!(
            render::desktop_exec("/opt/a b/spl$ude").unwrap(),
            r#""/opt/a b/spl\$ude""#
        );
    }

    #[test]
    fn a_desktop_exec_refuses_a_path_that_spans_lines() {
        assert!(render::desktop_exec("/opt/spl\naude").is_err());
    }

    #[test]
    fn a_desktop_path_lands_in_the_autostart_directory() {
        let path = render::desktop_path_in(Path::new("/home/a/.config"));
        assert_eq!(path, Path::new("/home/a/.config/autostart/splaude.desktop"));
    }

    #[test]
    fn a_control_character_in_the_executable_path_is_refused() {
        assert!(exe_text(Path::new("/opt/spl\u{7}aude")).is_err());
        assert!(exe_text(Path::new("")).is_err());
        assert!(exe_text(Path::new(SPACED)).is_ok());
    }
}

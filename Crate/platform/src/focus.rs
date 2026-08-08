//! Whether it is safe to type where focus is.
//!
//! Live typing emits backspaces, and a backspace that lands in a file list
//! deletes a file rather than a character. `FocusProbe.swift` gates the macOS
//! build on the Accessibility API for exactly that reason; this is the same
//! gate, expressed with what each platform will actually tell us.
//!
//! Windows answers through `GetGUIThreadInfo`, which names the focused HWND,
//! and `GetClassNameW`, which names its window class. That is a much blunter
//! instrument than an AX role — it only sees the classic USER32 control set.
//! WPF, UWP, Electron, Qt and every browser render their whole UI into one
//! opaque HWND, so the class says nothing about what is under the caret. Those
//! resolve to [`FocusVerdict::Unknown`], never to a guess: a guard that wrongly
//! reports `Editable` is worse than one that admits it cannot tell.

/// Whether it is safe to type where focus currently is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusVerdict {
    /// A text surface that accepts input.
    Editable,
    /// Focus is on something that is not text — typing would fire shortcuts.
    NotEditable,
    /// The platform cannot tell. Treated as permission to proceed, because
    /// refusing every take on a platform with no introspection would make the
    /// app useless there.
    Unknown,
}

/// Window classes that are text entry and nothing else.
///
/// Compared case-insensitively, because `RegisterClass` names are.
#[cfg_attr(not(windows), allow(dead_code))]
const EDITABLE_CLASS: [&str; 5] = [
    // The USER32 edit control, still under most native text fields.
    "Edit",
    // Scintilla is the editor surface in Notepad++, SciTE and friends.
    "Scintilla",
    // VCL (Delphi/Lazarus) keeps one HWND per control, so these are exact.
    "TEdit",
    "TMemo",
    "TRichEdit",
];

/// Versioned rich-edit classes: `RichEdit20W`, `RICHEDIT50W`, `RichEditD2DPT`.
/// Matched by prefix because the suffix tracks the msftedit build.
#[cfg_attr(not(windows), allow(dead_code))]
const EDITABLE_PREFIX: [&str; 1] = ["RichEdit"];

/// Classes that unambiguously cannot take text. Anything destructive a stray
/// keystroke could reach lives here — shell views above all, where a backspace
/// navigates and a letter jumps the selection to a different file.
///
/// `ComboBox` is deliberately absent: a drop-list combo swallows letters as
/// selection jumps while an editable combo takes text, and the class alone does
/// not separate them. `ConsoleWindowClass` and the terminal hosts are absent
/// for the same reason — they accept text, but a full-screen TUI turns that
/// text into commands, and we will not claim confidence we do not have.
#[cfg_attr(not(windows), allow(dead_code))]
const HOSTILE_CLASS: [&str; 22] = [
    // Shell and Explorer surfaces.
    "SysListView32",
    "SysTreeView32",
    "DirectUIHWND",
    "SHELLDLL_DefView",
    "CabinetWClass",
    "ExploreWClass",
    "Shell_TrayWnd",
    // The desktop itself — icons, and a Delete key with no undo.
    "Progman",
    "WorkerW",
    // Chrome and ornament.
    "ToolbarWindow32",
    "ReBarWindow32",
    "MsoCommandBar",
    "SysHeader32",
    "SysTabControl32",
    "SysPager",
    "SysLink",
    "ScrollBar",
    "Static",
    "Button",
    "ListBox",
    "msctls_progress32",
    "msctls_trackbar32",
];

/// Classifies a window class name. Pure, so the table above can be tested
/// without a desktop session.
#[cfg_attr(not(windows), allow(dead_code))]
fn classify(class: &str) -> FocusVerdict {
    let name = bare_class(class.trim());
    if name.is_empty() {
        return FocusVerdict::Unknown;
    }

    if EDITABLE_CLASS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(name))
        || EDITABLE_PREFIX.iter().any(|prefix| {
            name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix)
        })
    {
        return FocusVerdict::Editable;
    }

    if HOSTILE_CLASS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(name))
    {
        return FocusVerdict::NotEditable;
    }

    FocusVerdict::Unknown
}

/// Strips the WinForms decoration from a class name.
///
/// WinForms registers per-instance classes — `WindowsForms10.EDIT.app.0.141b42a`
/// — so the real control is the second dot-segment. Left alone, every WinForms
/// text box would read as unclassifiable.
#[cfg_attr(not(windows), allow(dead_code))]
fn bare_class(name: &str) -> &str {
    if name.len() >= 12 && name[..12].eq_ignore_ascii_case("WindowsForms") {
        name.split('.').nth(1).unwrap_or(name)
    } else {
        name
    }
}

/// Whether the focused thread is showing a menu or dragging a window, from the
/// `GUITHREADINFO` flag word. Either state routes keystrokes to the menu or the
/// move loop rather than to any text, so it is a refusal regardless of class.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_modal_state(flag: u32) -> bool {
    const IN_MOVE_SIZE: u32 = 0x0002;
    const IN_MENU_MODE: u32 = 0x0004;
    const SYSTEM_MENU_MODE: u32 = 0x0008;
    const POPUP_MENU_MODE: u32 = 0x0010;
    flag & (IN_MOVE_SIZE | IN_MENU_MODE | SYSTEM_MENU_MODE | POPUP_MENU_MODE) != 0
}

// MARK: - Windows

#[cfg(windows)]
mod backend {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
        GUITHREADINFO,
    };

    use super::{classify, is_modal_state, FocusVerdict};

    /// Long enough for any registered class name; `RegisterClass` caps at 256.
    const CLASS_BUFFER: usize = 256;

    /// `MAX_PATH` is not the limit for a process image path — long paths are
    /// legal — so this is sized for one rather than for the legacy 260.
    const PATH_BUFFER: usize = 32_768;

    pub fn is_supported() -> bool {
        true
    }

    pub fn verdict() -> FocusVerdict {
        let Some(info) = gui_thread_info() else {
            return FocusVerdict::Unknown;
        };

        if is_modal_state(info.flags.0) || !info.hwndMenuOwner.is_invalid() {
            return FocusVerdict::NotEditable;
        }

        // No focused HWND at all means the foreground thread keeps focus on its
        // top-level window. Common for full-window renderers, so it is unknown
        // rather than hostile.
        if info.hwndFocus.is_invalid() {
            return FocusVerdict::Unknown;
        }

        match class_of(info.hwndFocus) {
            Some(class) => classify(&class),
            None => FocusVerdict::Unknown,
        }
    }

    /// Process id, window class and handle. The handle alone would be enough to
    /// spot a move, but it is recycled after a window closes, and the other two
    /// make the value readable in a diagnostic log.
    pub fn anchor() -> Option<String> {
        let info = gui_thread_info()?;
        let window = if info.hwndFocus.is_invalid() {
            info.hwndActive
        } else {
            info.hwndFocus
        };
        if window.is_invalid() {
            return None;
        }

        let mut process = 0u32;
        // SAFETY: `window` is non-null and came from this same call chain, and
        // the out-parameter points at a live local that outlives the call.
        unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };

        let class = class_of(window).unwrap_or_else(|| "unknown".to_string());
        Some(format!("{process}:{class}:{:#x}", window.0 as usize))
    }

    /// The foreground process's own file name, e.g. `mstsc.exe`.
    ///
    /// Deliberately the *process*, not the window: the callers that care want to
    /// know which application is on the other side of the keystroke, and the
    /// window class cannot tell them — a remote-desktop client is one opaque
    /// HWND like every other modern framework.
    pub fn executable() -> Option<String> {
        let foreground = {
            // SAFETY: no arguments; returns a null handle on a session with no
            // foreground window, which `is_invalid` catches.
            let window = unsafe { GetForegroundWindow() };
            if window.is_invalid() {
                return None;
            }
            window
        };

        let mut process = 0u32;
        // SAFETY: `window` is non-null and the out-parameter points at a live
        // local that outlives the call.
        unsafe { GetWindowThreadProcessId(foreground, Some(&mut process)) };
        if process == 0 {
            return None;
        }

        // QUERY_LIMITED_INFORMATION is the least privilege that still answers
        // this question, and unlike QUERY_INFORMATION it is granted across
        // integrity levels — an elevated remote-desktop window would otherwise
        // be invisible to us.
        // SAFETY: takes a process id by value and returns a handle or an error;
        // nothing is borrowed.
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process) }.ok()?;

        // Heap, not stack: 64 KiB is more than some worker threads get.
        let mut buffer = vec![0u16; PATH_BUFFER];
        let mut length = buffer.len() as u32;
        // SAFETY: `handle` is live until the `CloseHandle` below, and the
        // pointer/length pair describes the exclusively-borrowed local buffer,
        // which is the whole contract of this call. It writes the used length
        // back through `length`.
        let queried = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        };
        // SAFETY: `handle` came from `OpenProcess`, is still open, and is not
        // used again after this point.
        let _ = unsafe { CloseHandle(handle) };
        queried.ok()?;

        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        // The API answers with a full path; only the leaf names the program.
        // Split on both separators because the Win32 form uses backslashes and
        // nothing forbids the other one.
        let name = path.rsplit(['\\', '/']).next().unwrap_or(&path).trim();
        (!name.is_empty()).then(|| name.to_string())
    }

    /// Focus is per-thread on Windows, and only the foreground thread's focus
    /// is the one a keystroke would reach.
    fn gui_thread_info() -> Option<GUITHREADINFO> {
        // SAFETY: takes no arguments and returns a handle that is null when no
        // window is foreground — a headless session, a lock screen, a service.
        // Checked before use.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.is_invalid() {
            return None;
        }

        // SAFETY: `foreground` is non-null; `None` declines the process-id
        // out-parameter, which the binding turns into a null pointer the API
        // documents as optional.
        let thread = unsafe { GetWindowThreadProcessId(foreground, None) };
        if thread == 0 {
            return None;
        }

        let mut info = GUITHREADINFO {
            // The API rejects the call unless the caller stamps its own size.
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: the pointer is to a live, exclusively-borrowed local of
        // exactly `cbSize` bytes, which is the whole contract of this call. It
        // fails rather than writes when the thread is gone or on another
        // desktop.
        unsafe { GetGUIThreadInfo(thread, &mut info) }.ok()?;

        Some(info)
    }

    fn class_of(window: HWND) -> Option<String> {
        let mut buffer = [0u16; CLASS_BUFFER];
        // SAFETY: the binding passes the slice's own length as the capacity, so
        // the API cannot write past a buffer that is borrowed for the call.
        let length = unsafe { GetClassNameW(window, &mut buffer) };
        if length <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buffer[..length as usize]))
    }
}

// MARK: - macOS

/// macOS is covered by the shipping Swift app, whose `FocusProbe` already asks
/// the Accessibility API for the focused element's role and whether its value
/// is settable. Porting that to `AXUIElement` FFI is the follow-up; until then
/// this reports honestly that it cannot tell, rather than faking a verdict the
/// Rust build has not earned.
#[cfg(target_os = "macos")]
mod backend {
    use super::FocusVerdict;

    pub fn is_supported() -> bool {
        false
    }

    pub fn verdict() -> FocusVerdict {
        FocusVerdict::Unknown
    }

    pub fn anchor() -> Option<String> {
        None
    }

    /// `NSWorkspace.frontmostApplication` would answer this without the
    /// Accessibility API, but that is AppKit FFI this crate does not carry yet.
    /// `None` means "cannot tell", and callers proceed as normal.
    pub fn executable() -> Option<String> {
        None
    }
}

// MARK: - Linux

/// Linux cannot answer this question, on either display server.
///
/// X11 exposes window geometry and properties but has no notion of a focused
/// *widget*: `XGetInputFocus` names a top-level window, and everything inside
/// it is the toolkit's private business. AT-SPI does know, but it is an opt-in
/// accessibility bus that many toolkits only populate when a screen reader has
/// already asked, so a negative answer from it is indistinguishable from an app
/// that never registered. Wayland refuses the introspection outright — reading
/// another client's state is exactly what its security model exists to prevent.
///
/// So: `Unknown` everywhere, which callers treat as permission to proceed.
#[cfg(not(any(windows, target_os = "macos")))]
mod backend {
    use super::FocusVerdict;

    pub fn is_supported() -> bool {
        false
    }

    pub fn verdict() -> FocusVerdict {
        FocusVerdict::Unknown
    }

    pub fn anchor() -> Option<String> {
        None
    }

    /// Naming the foreground process means naming the foreground window first,
    /// which is the introspection Wayland exists to refuse and X11 only answers
    /// for cooperating clients. `None` means "cannot tell", and callers proceed
    /// as normal.
    pub fn executable() -> Option<String> {
        None
    }
}

/// Whether this platform can distinguish a text field at all.
///
/// False does not disable dictation — [`verdict`] returns
/// [`FocusVerdict::Unknown`] there, which is permissive. It only tells the UI
/// not to promise a guard it cannot deliver.
pub fn is_supported() -> bool {
    backend::is_supported()
}

/// Never panics and never blocks: a take must not be lost because the guard
/// could not reach the window manager.
pub fn verdict() -> FocusVerdict {
    backend::verdict()
}

/// Identifies the surface a take started in, so `anchor_input` can refuse to
/// follow focus mid-sentence. `None` when the platform cannot tell.
pub fn anchor() -> Option<String> {
    backend::anchor()
}

/// The file name of the process behind the foreground window, e.g. `mstsc.exe`.
///
/// `None` is "cannot tell", not "nothing there" — it is what macOS and Linux
/// always answer, and what Windows answers when there is no foreground window.
/// Every caller must read it as permission to proceed as normal, because the
/// alternative is changing behaviour on two platforms that never asked.
pub fn executable() -> Option<String> {
    backend::executable()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn the_classic_edit_control_is_editable() {
        assert_eq!(classify("Edit"), FocusVerdict::Editable);
    }

    #[test]
    fn class_matching_ignores_case_like_registerclass_does() {
        assert_eq!(classify("EDIT"), FocusVerdict::Editable);
        assert_eq!(classify("syslistview32"), FocusVerdict::NotEditable);
    }

    #[test]
    fn every_richedit_generation_is_editable() {
        for class in [
            "RichEdit",
            "RichEdit20A",
            "RichEdit20W",
            "RICHEDIT50W",
            "RichEdit60W",
            "RichEditD2DPT",
        ] {
            assert_eq!(classify(class), FocusVerdict::Editable, "{class}");
        }
    }

    #[test]
    fn a_prefix_shorter_than_the_pattern_does_not_match() {
        // Guards the length check in the prefix comparison.
        assert_eq!(classify("Rich"), FocusVerdict::Unknown);
    }

    #[test]
    fn scintilla_editors_are_editable() {
        assert_eq!(classify("Scintilla"), FocusVerdict::Editable);
    }

    #[test]
    fn winforms_decoration_is_stripped_before_matching() {
        assert_eq!(
            classify("WindowsForms10.EDIT.app.0.141b42a_r9_ad1"),
            FocusVerdict::Editable
        );
        assert_eq!(
            classify("WindowsForms10.RICHEDIT20W.app.0.141b42a"),
            FocusVerdict::Editable
        );
        assert_eq!(
            classify("WindowsForms10.SysListView32.app.0.141b42a"),
            FocusVerdict::NotEditable
        );
        // A WinForms container is still unclassifiable, not a guess.
        assert_eq!(
            classify("WindowsForms10.Window.8.app.0.141b42a"),
            FocusVerdict::Unknown
        );
    }

    #[test]
    fn a_winforms_name_with_no_segment_falls_back_to_itself() {
        assert_eq!(classify("WindowsForms10"), FocusVerdict::Unknown);
    }

    #[test]
    fn shell_surfaces_refuse_typing() {
        // The dangerous case: a backspace here navigates Explorer.
        for class in [
            "SysListView32",
            "SysTreeView32",
            "DirectUIHWND",
            "SHELLDLL_DefView",
            "CabinetWClass",
            "Progman",
            "WorkerW",
        ] {
            assert_eq!(classify(class), FocusVerdict::NotEditable, "{class}");
        }
    }

    #[test]
    fn a_renderer_hwnd_is_unknown_rather_than_a_guess() {
        // Chromium, Electron, WPF, UWP and Qt each draw a whole UI into one
        // window. Claiming Editable here would be a lie either way.
        for class in [
            "Chrome_RenderWidgetHostHWND",
            "HwndWrapper[app;;abc]",
            "Windows.UI.Core.CoreWindow",
            "Qt5152QWindowIcon",
            "ConsoleWindowClass",
            "CASCADIA_HOSTING_WINDOW_CLASS",
            "ComboBox",
        ] {
            assert_eq!(classify(class), FocusVerdict::Unknown, "{class}");
        }
    }

    #[test]
    fn an_empty_or_blank_class_is_unknown() {
        assert_eq!(classify(""), FocusVerdict::Unknown);
        assert_eq!(classify("   "), FocusVerdict::Unknown);
    }

    #[test]
    fn menu_and_move_loops_refuse_typing() {
        assert!(!is_modal_state(0));
        // GUI_CARETBLINKING alone is an ordinary text caret.
        assert!(!is_modal_state(0x0001));
        assert!(is_modal_state(0x0002));
        assert!(is_modal_state(0x0004));
        assert!(is_modal_state(0x0008));
        assert!(is_modal_state(0x0010));
        assert!(is_modal_state(0x0001 | 0x0004));
    }

    /// The whole point of the contract: these run in CI with no desktop, so
    /// they must answer rather than panic.
    #[test]
    fn the_probe_answers_without_a_session() {
        let _ = is_supported();
        let _ = anchor();
        let _ = executable();
        // Nothing about the verdict is asserted — a headless runner has no
        // foreground window, and a developer machine does.
        let _ = verdict();
    }

    #[test]
    fn support_is_claimed_only_where_it_is_implemented() {
        assert_eq!(is_supported(), cfg!(windows));
    }
}

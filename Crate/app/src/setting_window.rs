//! The settings window.
//!
//! Everything in [`splaude_core::Setting`] has always been reachable — the file
//! is JSON and the tray opens it — but "reachable" and "offered" are not the
//! same thing, and the Swift build offers it. This is that window on Windows:
//! the same four groups, the same controls, the same words.
//!
//! It is hand-written Win32 rather than a widget toolkit, and `Cargo.toml` says
//! why. The consequence worth stating here is the threading one. `tao` owns the
//! main thread for the hotkey pump and never gives it back, so this window runs
//! on a thread of its own with its own message queue — which is a thing Win32
//! is happy to do and most toolkits are not. Nothing is shared with the event
//! loop except a callback: the window writes the settings file, then says so,
//! and the loop re-reads it through exactly the same path *Reload Settings*
//! uses. There is one reader of that file and one writer of the live setting,
//! which is the only arrangement that cannot drift.
//!
//! The window refuses to open over a settings file that did not parse. That is
//! not tidiness: the controls would come up holding defaults, saving would
//! write those defaults over the user's own broken text, and the keyterms and
//! binding they were mid-edit would be gone. `Edit Settings…` is the item for a
//! file in that state, because a text editor can show what is actually wrong.

#[cfg(windows)]
pub use backend::open;

/// Nothing to open. macOS has the Swift build's own settings window, and Linux
/// has no tray to open one from — the caller is gated on the same `cfg` as the
/// tray, so this exists to keep the module's shape rather than to be called.
#[cfg(not(windows))]
pub fn open(_on_save: Box<dyn Fn() + Send>) -> anyhow::Result<Option<std::thread::JoinHandle<()>>> {
    anyhow::bail!("the settings window is only implemented on Windows — edit the file instead")
}

#[cfg(windows)]
mod backend {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::thread::JoinHandle;

    use anyhow::{anyhow, Result};
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateFontIndirectW, DeleteObject, GetSysColor, GetSysColorBrush, SetBkMode, SetTextColor,
        COLOR_BTNFACE, COLOR_BTNTEXT, COLOR_WINDOW, HBRUSH, HDC, HFONT, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Controls::{
        InitCommonControlsEx, BST_CHECKED, BST_UNCHECKED, ICC_BAR_CLASSES, ICC_TAB_CLASSES,
        INITCOMMONCONTROLSEX, NMHDR, TBM_SETPAGESIZE, TBM_SETPOS, TBM_SETRANGE, TBM_SETTICFREQ,
        TCIF_TEXT, TCITEMW, TCM_GETCURSEL, TCM_INSERTITEMW, TCN_SELCHANGE,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, SetFocus, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    use splaude_core::credential::Store;
    use splaude_core::setting::{AVAILABLE_LANGUAGE, BUILTIN_KEYTERM};
    use splaude_core::speech::anthropic::pack_keyterm;
    use splaude_core::{diagnostic, quota, update, Hotkey, Setting};
    use splaude_platform::Capability;

    /// The open window, or zero. A settings window is a place, not a document:
    /// asking for it twice must raise the one that exists rather than stack a
    /// second copy holding a second unsaved edit of the same file.
    static WINDOW: AtomicIsize = AtomicIsize::new(0);

    const CLASS: PCWSTR = w!("splaude.setting.window");

    // Geometry. Fixed rather than laid out by a solver, because there is one
    // window at one size and a solver is a dependency.
    const WIDTH: i32 = 560;
    const HEIGHT: i32 = 760;
    const MARGIN: i32 = 14;
    const PAGE_TOP: i32 = 46;
    const LINE: i32 = 22;
    const GAP: i32 = 8;
    const FULL: i32 = WIDTH - MARGIN * 2;

    // Control ids. Only the ones a message has to be recognised by need one.
    const ID_LIVE_TYPING: usize = 1001;
    const ID_GUARD_FOCUS: usize = 1002;
    const ID_STOP_ON_RETURN: usize = 1003;
    const ID_ANCHOR_INPUT: usize = 1004;
    const ID_INTERVAL: usize = 1005;
    const ID_LANGUAGE: usize = 1006;
    const ID_HOTKEY: usize = 1007;
    const ID_RECORD: usize = 1008;
    const ID_KEYTERM: usize = 1009;
    const ID_BUILTIN_KEYTERM: usize = 1010;
    const ID_FLOATING: usize = 1011;
    const ID_PLAY_SOUND: usize = 1012;
    const ID_LAUNCH: usize = 1013;
    const ID_REVEAL_LOG: usize = 1014;
    const ID_RECHECK: usize = 1015;
    const ID_SOUND: usize = 1017;
    const ID_LATCH: usize = 1018;
    const ID_REPLACEMENT: usize = 1019;
    const ID_BACKEND: usize = 1020;
    const ID_API_KEY: usize = 1021;
    const ID_TAB: usize = 1016;

    /// Shared by every explanatory line under a control. They are the only
    /// statics drawn in a lighter colour, and a `WM_CTLCOLORSTATIC` cannot tell
    /// a caption from a heading any other way — the message carries a device
    /// context and a handle, not a role.
    const ID_CAPTION: usize = 1100;

    /// The providers a take can go to, as the setting spells them and as the
    /// window shows them.
    const BACKEND: [(&str, &str); 2] = [
        ("anthropic", "Anthropic (uses your Claude Code login)"),
        ("deepgram", "Deepgram (your own API key)"),
    ];

    /// The typing-interval range, matching the Swift slider exactly.
    const INTERVAL_FLOOR: i32 = 200;
    const INTERVAL_CEILING: i32 = 5_000;
    const INTERVAL_STEP: i32 = 100;

    /// The server's own term budget. Shown rather than enforced — the packer
    /// already truncates — because a user who cannot see the limit cannot tell
    /// why the last term they added did nothing.
    const KEYTERM_BUDGET: usize = 1024;

    /// How many terms the wire will actually carry, read from the one place
    /// that measured it rather than copied here. Past it the server does not
    /// truncate the bias — it rejects the take and produces no text at all.
    use splaude_core::speech::anthropic::KEYTERM_COUNT_BUDGET as KEYTERM_TERM_CEILING;

    const PAGE_COUNT: usize = 4;

    /// The colour a caption is drawn in: dark grey, as `.foregroundStyle
    /// (.secondary)` is on the Swift side. A `COLORREF` is `0x00BBGGRR`, so
    /// this is the same value in all three channels.
    const SECONDARY_TEXT: u32 = 0x0055_5555;

    /// `TBM_GETPOS`. Written out because the `windows` crate binds the trackbar
    /// messages that take a parameter and not this one, which takes none — it is
    /// `TBM_FIRST` itself, and `TBM_FIRST` is `WM_USER`.
    const TBM_GETPOS: u32 = 0x0400;

    /// Everything the window procedure needs, hung off the window itself.
    struct State {
        setting: Setting,
        /// Told after every successful save. The event loop turns this into the
        /// same reload *Reload Settings* performs.
        on_save: Box<dyn Fn() + Send>,
        font: HFONT,
        tab: HWND,
        /// Controls per tab page, shown and hidden as the selection moves.
        page: [Vec<HWND>; PAGE_COUNT],
        hotkey_field: HWND,
        hotkey_note: HWND,
        record: HWND,
        keyterm: HWND,
        budget: HWND,
        interval_note: HWND,
        credential: HWND,
        microphone: HWND,
        connection: HWND,
        latch: HWND,
        replacement: HWND,
        api_key: HWND,
        /// True between clicking *Record* and the next key. While it is set the
        /// message pump stops handing keystrokes to the controls and hands them
        /// here instead — see [`pump`].
        arming: bool,
    }

    /// Opens the window, or raises the one already open.
    ///
    /// `on_save` is called on the window's own thread, so whatever it does must
    /// be safe from a thread that is not the event loop's. Posting to the loop
    /// is exactly that, and is the only thing it is used for.
    pub fn open(on_save: Box<dyn Fn() + Send>) -> Result<Option<JoinHandle<()>>> {
        let existing = WINDOW.load(Ordering::SeqCst);
        if existing != 0 {
            let window = HWND(existing as *mut c_void);
            // SAFETY: the handle is only ever stored while its window is alive
            // — the window procedure clears it on `WM_NCDESTROY` before the
            // handle can be recycled — and both calls tolerate a stale one by
            // failing rather than faulting.
            unsafe {
                let _ = ShowWindow(window, SW_RESTORE);
                let _ = SetForegroundWindow(window);
            }
            return Ok(None);
        }

        // Refused rather than opened over — the module header says why.
        let (setting, note) = Setting::load_checked();
        if let Some(note) = note {
            diagnostic::log(
                "setting",
                format!("{note} — not opening the settings window, because saving would write over the file as it stands. Use Edit Settings…"),
            );
            return Err(anyhow!("{note}"));
        }

        let thread = std::thread::Builder::new()
            .name("splaude-setting".into())
            .spawn(move || {
                if let Err(error) = run(setting, on_save) {
                    diagnostic::log("setting", format!("settings window: {error:#}"));
                }
                WINDOW.store(0, Ordering::SeqCst);
            })
            .map_err(|error| anyhow!("could not start the settings window: {error}"))?;

        Ok(Some(thread))
    }

    /// Builds the window and pumps it until it closes. Owns everything it
    /// makes, so returning from here is the whole teardown.
    fn run(setting: Setting, on_save: Box<dyn Fn() + Send>) -> Result<()> {
        // SAFETY: every call in this function is a plain Win32 constructor over
        // values that outlive it, or a message to a window this thread owns.
        // The one pointer handed across a boundary is the boxed `State`, which
        // is leaked into the window's user data and reclaimed in `WM_NCDESTROY`
        // — the only place it is freed, and after the last message that could
        // read it.
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None)?.into();

            let common = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_TAB_CLASSES | ICC_BAR_CLASSES,
            };
            let _ = InitCommonControlsEx(&common);

            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: CLASS,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH(GetSysColorBrush(COLOR_BTNFACE).0),
                ..Default::default()
            };
            // Zero means the class is already registered, which happens when
            // the window is closed and opened again in one run. Not an error.
            RegisterClassW(&class);

            let window = CreateWindowExW(
                // CONTROLPARENT is what makes Tab walk the controls: it tells
                // `IsDialogMessageW` that this window contains a keyboard
                // interface even though it is not a dialog.
                WS_EX_CONTROLPARENT,
                CLASS,
                w!("splaude Settings"),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                scaled(WIDTH),
                scaled(HEIGHT),
                None,
                None,
                Some(instance),
                None,
            )?;

            // The client area is what the geometry above is written against,
            // and a window is created at its *outer* size. Grow it by the
            // difference rather than guessing at border widths, which change
            // with the theme and the DPI.
            let mut client = Default::default();
            let _ = GetClientRect(window, &mut client);
            let _ = SetWindowPos(
                window,
                None,
                0,
                0,
                scaled(WIDTH) + (scaled(WIDTH) - client.right),
                scaled(HEIGHT) + (scaled(HEIGHT) - client.bottom),
                SWP_NOMOVE | SWP_NOZORDER,
            );

            let state = Box::new(State {
                setting,
                on_save,
                font: message_font(),
                tab: HWND::default(),
                page: [const { Vec::new() }; PAGE_COUNT],
                hotkey_field: HWND::default(),
                hotkey_note: HWND::default(),
                record: HWND::default(),
                keyterm: HWND::default(),
                budget: HWND::default(),
                interval_note: HWND::default(),
                credential: HWND::default(),
                microphone: HWND::default(),
                connection: HWND::default(),
                latch: HWND::default(),
                replacement: HWND::default(),
                api_key: HWND::default(),
                arming: false,
            });
            let state = Box::into_raw(state);
            SetWindowLongPtrW(window, GWLP_USERDATA, state as isize);

            build(window, &mut *state, instance);
            show_page(&*state, 0);

            WINDOW.store(window.0 as isize, Ordering::SeqCst);

            let _ = ShowWindow(window, SW_SHOW);
            let _ = SetForegroundWindow(window);

            pump(window);
        }

        Ok(())
    }

    /// The message loop.
    ///
    /// Two things it does that a bare `GetMessage` loop would not.
    /// `IsDialogMessageW` gives the controls Tab traversal, mnemonics and
    /// space-to-toggle, which is the difference between a window and a form.
    /// And while the hotkey recorder is armed it takes keystrokes *before* that
    /// happens, because otherwise Tab, Space and Escape would be consumed as
    /// navigation — and those are keys a person may well want to bind.
    unsafe fn pump(window: HWND) {
        let mut message = MSG::default();
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            let state = unsafe { state_of(window) };
            // By reference: the `Option<&mut State>` is used again below, and
            // asking it a question must not consume it.
            let arming = state.as_ref().is_some_and(|state| state.arming);

            if arming && (message.message == WM_KEYDOWN || message.message == WM_SYSKEYDOWN) {
                if let Some(state) = state {
                    record_key(window, state, message.wParam.0 as u32);
                }
                continue;
            }

            if unsafe { IsDialogMessageW(window, &message) }.as_bool() {
                continue;
            }

            let _ = unsafe { TranslateMessage(&message) };
            unsafe { DispatchMessageW(&message) };
        }
    }

    // MARK: - Construction

    /// Creates every control on every page. All of them are children of the
    /// window rather than of the tab control, and pages are shown and hidden;
    /// a tab control is a strip of labels with a rectangle under it, not a
    /// container, so parenting to it buys nothing and costs the Tab order.
    unsafe fn build(window: HWND, state: &mut State, instance: HINSTANCE) {
        // WS_VISIBLE here and nowhere else in this module. Every other control
        // belongs to a page and is shown or hidden by `show_page`; the tab
        // strip belongs to all four, so it is the one child that is never
        // hidden and therefore the one that has to be born visible.
        state.tab = child(
            window,
            w!("SysTabControl32"),
            PCWSTR::null(),
            WS_TABSTOP | WS_VISIBLE,
            MARGIN,
            MARGIN - 6,
            FULL,
            HEIGHT - MARGIN * 2,
            ID_TAB,
            instance,
        );
        set_font(state.tab, state.font);

        for (index, title) in ["Dictation", "Vocabulary", "General", "Status"]
            .into_iter()
            .enumerate()
        {
            let mut text: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            let item = TCITEMW {
                mask: TCIF_TEXT,
                pszText: windows::core::PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            unsafe {
                SendMessageW(
                    state.tab,
                    TCM_INSERTITEMW,
                    Some(WPARAM(index)),
                    Some(LPARAM(std::ptr::addr_of!(item) as isize)),
                )
            };
        }

        unsafe { dictation_page(window, state, instance) };
        unsafe { vocabulary_page(window, state, instance) };
        unsafe { general_page(window, state, instance) };
        unsafe { status_page(window, state, instance) };
    }

    unsafe fn dictation_page(window: HWND, state: &mut State, instance: HINSTANCE) {
        let mut y = PAGE_TOP;
        let mut item = Vec::new();

        for (id, title, detail, on) in [
            (
                ID_LIVE_TYPING,
                "Type as I speak",
                "Words appear while you talk and correct themselves as the recogniser revises them. Off buffers the whole take and pastes it once at the end.",
                state.setting.live_typing,
            ),
            (
                ID_GUARD_FOCUS,
                "Only type into text fields",
                "Live typing emits backspaces. This refuses to send them into surfaces that are not text — file lists, tables, canvases.",
                state.setting.guard_focus,
            ),
            (
                ID_STOP_ON_RETURN,
                "Stop dictating when I press Return",
                "Submitting means you are done talking. Return still sends whatever you were typing into — this only ends the take.",
                state.setting.stop_on_return,
            ),
            (
                ID_ANCHOR_INPUT,
                "Keep dictation in the field it started in",
                "Switch window mid-sentence and dictation pauses rather than following you, then resumes where it began.",
                state.setting.anchor_input,
            ),
        ] {
            let box_ = unsafe { checkbox(window, title, id, on, MARGIN + 10, y, instance) };
            set_font(box_, state.font);
            item.push(box_);
            y += LINE;

            let note = unsafe { caption(window, detail, MARGIN + 10, y, FULL - 24, 2, instance) };
            set_font(note, state.font);
            item.push(note);
            y += LINE * 2 + GAP;
        }

        let heading = unsafe {
            label(
                window,
                "Stop a latched take on silence",
                MARGIN + 10,
                y,
                instance,
            )
        };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        state.latch = unsafe {
            child(
                window,
                w!("EDIT"),
                PCWSTR::null(),
                WINDOW_STYLE((ES_NUMBER | ES_AUTOHSCROLL) as u32) | WS_BORDER | WS_TABSTOP,
                MARGIN + 10,
                y,
                70,
                24,
                ID_LATCH,
                instance,
            )
        };
        set_font(state.latch, state.font);
        set_text(state.latch, &state.setting.latch_timeout_second.to_string());
        item.push(state.latch);

        let note = unsafe {
            caption(
                window,
                "Seconds. A take you tapped to latch ends by itself after this much quiet, so a microphone you forgot is not left open. 0 never ends one. Held takes are unaffected — they end when you let go.",
                MARGIN + 90,
                y,
                FULL - 104,
                3,
                instance,
            )
        };
        set_font(note, state.font);
        item.push(note);
        y += LINE * 3 + GAP;

        let heading = unsafe { label(window, "Typing speed", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        let slider = unsafe {
            child(
                window,
                w!("msctls_trackbar32"),
                PCWSTR::null(),
                WS_TABSTOP,
                MARGIN + 10,
                y,
                FULL - 24,
                30,
                ID_INTERVAL,
                instance,
            )
        };
        unsafe {
            SendMessageW(
                slider,
                TBM_SETRANGE,
                Some(WPARAM(1)),
                Some(LPARAM(((INTERVAL_CEILING << 16) | INTERVAL_FLOOR) as isize)),
            );
            SendMessageW(
                slider,
                TBM_SETTICFREQ,
                Some(WPARAM(INTERVAL_STEP as usize)),
                None,
            );
            SendMessageW(
                slider,
                TBM_SETPAGESIZE,
                None,
                Some(LPARAM(INTERVAL_STEP as isize)),
            );
            SendMessageW(
                slider,
                TBM_SETPOS,
                Some(WPARAM(1)),
                Some(LPARAM(state.setting.typing_interval as isize)),
            );
        }
        item.push(slider);
        y += 34;

        state.interval_note = unsafe {
            caption(
                window,
                &interval_line(state.setting.typing_interval),
                MARGIN + 10,
                y,
                FULL - 24,
                2,
                instance,
            )
        };
        set_font(state.interval_note, state.font);
        item.push(state.interval_note);
        y += LINE * 2 + GAP;

        let heading = unsafe { label(window, "Language", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        let combo = unsafe {
            child(
                window,
                w!("COMBOBOX"),
                PCWSTR::null(),
                WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
                MARGIN + 10,
                y,
                220,
                // A drop-list is as tall as its open list, not as its closed
                // face: this is the list's height, and the face is one line.
                240,
                ID_LANGUAGE,
                instance,
            )
        };
        set_font(combo, state.font);
        for (index, (code, name)) in AVAILABLE_LANGUAGE.iter().enumerate() {
            let text = HSTRING::from(*name);
            unsafe {
                SendMessageW(
                    combo,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(text.as_ptr() as isize)),
                )
            };
            if *code == state.setting.language {
                unsafe { SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index)), None) };
            }
        }
        item.push(combo);
        y += 30;

        let heading = unsafe { label(window, "Shortcut", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        state.hotkey_field = unsafe {
            child(
                window,
                w!("EDIT"),
                PCWSTR::null(),
                WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
                MARGIN + 10,
                y,
                180,
                24,
                ID_HOTKEY,
                instance,
            )
        };
        set_font(state.hotkey_field, state.font);
        set_text(state.hotkey_field, &state.setting.hotkey.to_string());
        item.push(state.hotkey_field);

        state.record = unsafe {
            child(
                window,
                w!("BUTTON"),
                w!("Record…"),
                WINDOW_STYLE(BS_PUSHBUTTON as u32) | WS_TABSTOP,
                MARGIN + 200,
                y,
                90,
                24,
                ID_RECORD,
                instance,
            )
        };
        set_font(state.record, state.font);
        item.push(state.record);
        y += 30;

        state.hotkey_note = unsafe {
            caption(
                window,
                "Click Record, then press the combination you want — a modifier plus a key, or a function key on its own. Escape cancels. Or type it here: Ctrl+Shift+KeyD, F9.",
                MARGIN + 10,
                y,
                FULL - 24,
                3,
                instance,
            )
        };
        set_font(state.hotkey_note, state.font);
        item.push(state.hotkey_note);

        state.page[0] = item;
    }

    unsafe fn vocabulary_page(window: HWND, state: &mut State, instance: HINSTANCE) {
        let mut y = PAGE_TOP;
        let mut item = Vec::new();

        let note = unsafe {
            caption(
                window,
                "One per line. Names, jargon and project words the recogniser would otherwise mangle. This is the single biggest accuracy win available.",
                MARGIN + 10,
                y,
                FULL - 24,
                2,
                instance,
            )
        };
        set_font(note, state.font);
        item.push(note);
        y += LINE * 2 + GAP;

        state.keyterm = unsafe {
            child(
                window,
                w!("EDIT"),
                PCWSTR::null(),
                WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32)
                    | WS_BORDER
                    | WS_VSCROLL
                    | WS_TABSTOP,
                MARGIN + 10,
                y,
                FULL - 24,
                240,
                ID_KEYTERM,
                instance,
            )
        };
        set_font(state.keyterm, state.font);
        set_text(state.keyterm, &state.setting.custom_keyterm.join("\r\n"));
        item.push(state.keyterm);
        y += 250;

        let box_ = unsafe {
            checkbox(
                window,
                "Include built-in developer terms",
                ID_BUILTIN_KEYTERM,
                state.setting.use_builtin_keyterm,
                MARGIN + 10,
                y,
                instance,
            )
        };
        set_font(box_, state.font);
        item.push(box_);
        y += LINE;

        let builtin = unsafe {
            caption(
                window,
                &BUILTIN_KEYTERM.join(", "),
                MARGIN + 10,
                y,
                FULL - 24,
                3,
                instance,
            )
        };
        set_font(builtin, state.font);
        item.push(builtin);
        y += LINE * 3;

        state.budget = unsafe {
            caption(
                window,
                &budget_line(&state.setting),
                MARGIN + 10,
                y,
                FULL - 24,
                2,
                instance,
            )
        };
        set_font(state.budget, state.font);
        item.push(state.budget);
        y += LINE * 2 + GAP;

        let heading = unsafe { label(window, "Corrections", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        let note = unsafe {
            caption(
                window,
                "One per line, as heard = wanted. Terms above bias what the recogniser hears; these fix what it decided when it settles on the wrong spelling every time.",
                MARGIN + 10,
                y,
                FULL - 24,
                2,
                instance,
            )
        };
        set_font(note, state.font);
        item.push(note);
        y += LINE * 2;

        state.replacement = unsafe {
            child(
                window,
                w!("EDIT"),
                PCWSTR::null(),
                WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32)
                    | WS_BORDER
                    | WS_VSCROLL
                    | WS_TABSTOP,
                MARGIN + 10,
                y,
                FULL - 24,
                90,
                ID_REPLACEMENT,
                instance,
            )
        };
        set_font(state.replacement, state.font);
        set_text(state.replacement, &replacement_text(&state.setting));
        item.push(state.replacement);

        state.page[1] = item;
    }

    unsafe fn general_page(window: HWND, state: &mut State, instance: HINSTANCE) {
        let mut y = PAGE_TOP;
        let mut item = Vec::new();

        let heading = unsafe { label(window, "Interface", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        for (id, title, detail, on) in [
            (
                ID_FLOATING,
                "Show floating mic button",
                "A round mic button that floats above every app. Drag to reposition; it never takes keyboard focus.",
                state.setting.show_floating_button,
            ),
            (
                ID_PLAY_SOUND,
                "Play a sound when dictation starts and stops",
                "A short tone at each end of a take, so a latched microphone is audible rather than only visible.",
                state.setting.play_sound,
            ),
        ] {
            let box_ = unsafe { checkbox(window, title, id, on, MARGIN + 10, y, instance) };
            set_font(box_, state.font);
            item.push(box_);
            y += LINE;

            let note = unsafe { caption(window, detail, MARGIN + 10, y, FULL - 24, 2, instance) };
            set_font(note, state.font);
            item.push(note);
            y += LINE * 2 + GAP;
        }

        let heading = unsafe { label(window, "Startup", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        let box_ = unsafe {
            checkbox(
                window,
                "Launch splaude at login",
                ID_LAUNCH,
                state.setting.launch_at_login,
                MARGIN + 10,
                y,
                instance,
            )
        };
        set_font(box_, state.font);
        item.push(box_);
        y += LINE + GAP;

        let heading = unsafe { label(window, "Diagnostics", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        let button = unsafe {
            child(
                window,
                w!("BUTTON"),
                w!("Reveal Log"),
                WINDOW_STYLE(BS_PUSHBUTTON as u32) | WS_TABSTOP,
                MARGIN + 10,
                y,
                110,
                26,
                ID_REVEAL_LOG,
                instance,
            )
        };
        set_font(button, state.font);
        item.push(button);
        y += 32;

        let path = unsafe {
            caption(
                window,
                &format!(
                    "{}\nSettings file: {}",
                    diagnostic::path().display(),
                    Setting::path().display()
                ),
                MARGIN + 10,
                y,
                FULL - 24,
                3,
                instance,
            )
        };
        set_font(path, state.font);
        item.push(path);

        state.page[2] = item;
    }

    unsafe fn status_page(window: HWND, state: &mut State, instance: HINSTANCE) {
        let mut y = PAGE_TOP;
        let mut item = Vec::new();

        let heading = unsafe { label(window, "Credential", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        // A read-only edit rather than a static, so the text can be selected
        // and copied — a credential path is exactly the kind of line someone
        // needs to paste into a terminal.
        state.credential = unsafe {
            child(
                window,
                w!("EDIT"),
                PCWSTR::null(),
                WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32) | WS_BORDER | WS_TABSTOP,
                MARGIN + 10,
                y,
                FULL - 24,
                90,
                0,
                instance,
            )
        };
        set_font(state.credential, state.font);
        set_text(state.credential, &credential_line());
        item.push(state.credential);
        y += 96;

        let button = unsafe {
            child(
                window,
                w!("BUTTON"),
                w!("Re-check"),
                WINDOW_STYLE(BS_PUSHBUTTON as u32) | WS_TABSTOP,
                MARGIN + 10,
                y,
                110,
                26,
                ID_RECHECK,
                instance,
            )
        };
        set_font(button, state.font);
        item.push(button);
        y += 34;

        let heading = unsafe { label(window, "Speech provider", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        let combo = unsafe {
            child(
                window,
                w!("COMBOBOX"),
                PCWSTR::null(),
                WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
                MARGIN + 10,
                y,
                220,
                120,
                ID_BACKEND,
                instance,
            )
        };
        set_font(combo, state.font);
        for (index, (code, name)) in BACKEND.iter().enumerate() {
            let text = HSTRING::from(*name);
            unsafe {
                SendMessageW(
                    combo,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(text.as_ptr() as isize)),
                )
            };
            if *code == state.setting.speech_backend {
                unsafe { SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index)), None) };
            }
        }
        item.push(combo);
        y += 30;

        // A password field, because it is a secret and a settings window is a
        // thing people open with somebody behind them. It is stored in plain
        // text in the settings file either way, which the caption says rather
        // than hides.
        state.api_key = unsafe {
            child(
                window,
                w!("EDIT"),
                PCWSTR::null(),
                WINDOW_STYLE((ES_PASSWORD | ES_AUTOHSCROLL) as u32) | WS_BORDER | WS_TABSTOP,
                MARGIN + 10,
                y,
                FULL - 24,
                24,
                ID_API_KEY,
                instance,
            )
        };
        set_font(state.api_key, state.font);
        set_text(state.api_key, &state.setting.api_key);
        item.push(state.api_key);
        y += 28;

        let note = unsafe {
            caption(
                window,
                "The default sends speech to Anthropic's internal endpoint using the Claude Code credential already on this machine, and needs no key. Deepgram needs one of your own, billed to you, and is here because the default endpoint is undocumented and could stop answering. The key is stored in the settings file as plain text.",
                MARGIN + 10,
                y,
                FULL - 24,
                4,
                instance,
            )
        };
        set_font(note, state.font);
        item.push(note);
        y += LINE * 4 + GAP;

        let heading = unsafe { label(window, "Microphone", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        // The Swift window has a Microphone permission row, and this is the
        // Windows shape of the same question. There is no per-app grant to
        // check the way macOS has one — what there is instead is a device, or
        // not: a session with no audio redirected, a headset that is
        // output-only, or a microphone switched off under Privacy all present
        // as *no input device*, which is the single most common reason
        // dictation does nothing here.
        let microphone = unsafe {
            caption(
                window,
                &microphone_line(),
                MARGIN + 10,
                y,
                FULL - 24,
                2,
                instance,
            )
        };
        set_font(microphone, state.font);
        state.microphone = microphone;
        item.push(microphone);
        y += LINE * 2;

        let button = unsafe {
            child(
                window,
                w!("BUTTON"),
                w!("Sound settings"),
                WINDOW_STYLE(BS_PUSHBUTTON as u32) | WS_TABSTOP,
                MARGIN + 10,
                y,
                130,
                26,
                ID_SOUND,
                instance,
            )
        };
        set_font(button, state.font);
        item.push(button);
        y += 34;

        let heading = unsafe { label(window, "Capability", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        let capability = Capability::detect();
        let mut lines = format!(
            "Global hotkey: {}\nType into apps: {}\nInspect focus: {}\nLaunch at login: {}",
            mark(capability.global_hotkey),
            mark(capability.inject_text),
            mark(capability.inspect_focus),
            mark(capability.autostart),
        );
        if let Some(note) = &capability.note {
            lines.push('\n');
            lines.push_str(note);
        }
        let shown = unsafe { caption(window, &lines, MARGIN + 10, y, FULL - 24, 5, instance) };
        set_font(shown, state.font);
        item.push(shown);
        y += LINE * 5;

        let heading = unsafe { label(window, "Connection", MARGIN + 10, y, instance) };
        set_font(heading, state.font);
        item.push(heading);
        y += LINE;

        state.connection = unsafe {
            caption(
                window,
                &connection_text(&state.setting),
                MARGIN + 10,
                y,
                FULL - 24,
                7,
                instance,
            )
        };
        let connection = state.connection;
        set_font(connection, state.font);
        item.push(connection);

        state.page[3] = item;
    }

    // MARK: - Window procedure

    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_COMMAND => {
                if let Some(state) = unsafe { state_of(window) } {
                    let id = wparam.0 & 0xFFFF;
                    let notification = ((wparam.0 >> 16) & 0xFFFF) as u32;
                    unsafe { command(window, state, id, notification) };
                }
                LRESULT(0)
            }

            // The trackbar reports through its parent. Committed on the end of
            // the gesture as well as during it: writing the file on every pixel
            // of a drag would be forty saves and forty reloads for one change
            // of mind.
            WM_HSCROLL => {
                if let Some(state) = unsafe { state_of(window) } {
                    let slider = HWND(lparam.0 as *mut c_void);
                    let position = unsafe { SendMessageW(slider, TBM_GETPOS, None, None) }.0 as i32;
                    let rounded = round_interval(position);
                    set_text(state.interval_note, &interval_line(rounded as u32));

                    let code = (wparam.0 & 0xFFFF) as i32;
                    let ended = code == SB_ENDSCROLL.0 || code == SB_THUMBPOSITION.0;
                    if ended && state.setting.typing_interval != rounded as u32 {
                        state.setting.typing_interval = rounded as u32;
                        commit(state);
                    }
                }
                LRESULT(0)
            }

            WM_NOTIFY => {
                let header = lparam.0 as *const NMHDR;
                // SAFETY: WM_NOTIFY's lparam is an NMHDR by contract, and the
                // structure lives for the duration of the message.
                if !header.is_null() && unsafe { (*header).code } == TCN_SELCHANGE {
                    if let Some(state) = unsafe { state_of(window) } {
                        let page = unsafe { SendMessageW(state.tab, TCM_GETCURSEL, None, None) }.0;
                        if page >= 0 {
                            show_page(state, page as usize);
                        }
                    }
                }
                LRESULT(0)
            }

            // Statics and checkbox labels would otherwise draw on white inside
            // a grey window. Only the captions are also greyed — a heading in
            // secondary text is not a heading, and the colour message is the
            // one place that can tell them apart, by the id above.
            WM_CTLCOLORSTATIC => {
                let dc = HDC(wparam.0 as *mut c_void);
                let control = HWND(lparam.0 as *mut c_void);
                // SAFETY: both handles belong to the control being drawn and
                // are valid for the length of this message.
                unsafe {
                    SetBkMode(dc, TRANSPARENT);
                    // Both branches set a colour, and the else is not
                    // redundant: the device context is reused across controls,
                    // so a heading that set nothing would inherit whatever the
                    // last caption asked for and be quietly greyed too.
                    if GetDlgCtrlID(control) == ID_CAPTION as i32 {
                        SetTextColor(dc, COLORREF(SECONDARY_TEXT));
                    } else {
                        SetTextColor(dc, COLORREF(GetSysColor(COLOR_BTNTEXT)));
                    }
                    LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize)
                }
            }

            // Edits and lists keep the window background they would have in a
            // dialog, which is not the grey the captions sit on.
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                // SAFETY: the brush is a system object, valid for the process.
                unsafe { LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize) }
            }

            // Everything a control might still be holding is written back on
            // the way out: a keyterm list edited and never tabbed away from is
            // otherwise lost at the moment the user thinks they are done.
            WM_CLOSE => {
                if let Some(state) = unsafe { state_of(window) } {
                    unsafe { harvest(state) };
                }
                let _ = unsafe { DestroyWindow(window) };
                LRESULT(0)
            }

            WM_NCDESTROY => {
                let held = unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
                if held != 0 {
                    // SAFETY: the pointer was made by `Box::into_raw` in `run`,
                    // has not been freed — this is the only place that frees it
                    // — and no further message can reach the procedure with it
                    // installed, because it was cleared above first.
                    let state = unsafe { Box::from_raw(held as *mut State) };
                    // SAFETY: the font was created by this window and nothing
                    // else holds it; the controls that used it are already gone.
                    unsafe {
                        let _ = DeleteObject(state.font.into());
                    }
                }
                WINDOW.store(0, Ordering::SeqCst);
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }

            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    /// One control said something. Only the ones that change a setting or run
    /// an action are named; everything else is a control talking to itself.
    unsafe fn command(window: HWND, state: &mut State, id: usize, notification: u32) {
        diagnostic::log(
            "trace",
            format!("command id={id} notification={notification}"),
        );
        match id {
            ID_LIVE_TYPING => {
                state.setting.live_typing = checked(window, ID_LIVE_TYPING);
                commit(state);
            }
            ID_GUARD_FOCUS => {
                state.setting.guard_focus = checked(window, ID_GUARD_FOCUS);
                commit(state);
            }
            ID_STOP_ON_RETURN => {
                state.setting.stop_on_return = checked(window, ID_STOP_ON_RETURN);
                commit(state);
            }
            ID_ANCHOR_INPUT => {
                state.setting.anchor_input = checked(window, ID_ANCHOR_INPUT);
                commit(state);
            }
            ID_BUILTIN_KEYTERM => {
                state.setting.use_builtin_keyterm = checked(window, ID_BUILTIN_KEYTERM);
                set_text(state.budget, &budget_line(&state.setting));
                commit(state);
            }
            ID_FLOATING => {
                state.setting.show_floating_button = checked(window, ID_FLOATING);
                commit(state);
            }
            ID_PLAY_SOUND => {
                state.setting.play_sound = checked(window, ID_PLAY_SOUND);
                commit(state);
            }
            ID_LAUNCH => {
                state.setting.launch_at_login = checked(window, ID_LAUNCH);
                commit(state);
            }

            ID_LANGUAGE if notification == CBN_SELCHANGE => {
                let combo = unsafe { GetDlgItem(Some(window), ID_LANGUAGE as i32) };
                if let Ok(combo) = combo {
                    let index = unsafe { SendMessageW(combo, CB_GETCURSEL, None, None) }.0;
                    if let Some((code, _)) = usize::try_from(index)
                        .ok()
                        .and_then(|index| AVAILABLE_LANGUAGE.get(index))
                    {
                        state.setting.language = (*code).to_string();
                        commit(state);
                    }
                }
            }

            // Committed when focus leaves rather than per keystroke: a partly
            // typed binding is not a binding, and a term list saved per letter
            // would be a reload per letter.
            ID_HOTKEY if notification == EN_KILLFOCUS => unsafe { take_hotkey(state) },
            ID_KEYTERM if notification == EN_KILLFOCUS => unsafe { take_keyterm(state) },
            ID_REPLACEMENT if notification == EN_KILLFOCUS => unsafe { take_replacement(state) },
            ID_LATCH if notification == EN_KILLFOCUS => unsafe { take_latch(state) },
            ID_API_KEY if notification == EN_KILLFOCUS => unsafe { take_api_key(state) },

            ID_BACKEND if notification == CBN_SELCHANGE => {
                let combo = unsafe { GetDlgItem(Some(window), ID_BACKEND as i32) };
                if let Ok(combo) = combo {
                    let index = unsafe { SendMessageW(combo, CB_GETCURSEL, None, None) }.0;
                    if let Some((code, _)) = usize::try_from(index)
                        .ok()
                        .and_then(|index| BACKEND.get(index))
                    {
                        state.setting.speech_backend = (*code).to_string();
                        commit(state);
                    }
                }
            }

            ID_RECORD => {
                state.arming = true;
                set_text(state.record, "Press a key…");
                set_text(
                    state.hotkey_note,
                    "Listening — press the combination you want. Escape cancels.",
                );
                // The pump reads keys itself while armed, but focus still has
                // to be somewhere inside this window for them to arrive at all.
                let _ = unsafe { SetFocus(Some(state.record)) };
            }

            ID_REVEAL_LOG => crate::tray::reveal_log(),
            // Everything on the status page that can go stale while the window
            // is open, refreshed together. The update check is the only one
            // that reaches the network, which is exactly why it belongs to a
            // click rather than to the page being drawn.
            ID_RECHECK => {
                set_text(state.credential, &credential_line());
                set_text(state.microphone, &microphone_line());
                set_text(state.connection, &connection_checked(&state.setting));
            }

            // The Settings app owns this, and there is no in-process fix to
            // offer — a device either exists or it does not. Same shape as the
            // Swift window's Fix button, which also only opens a pane.
            ID_SOUND => open_sound_setting(),

            _ => {}
        }
    }

    /// The recorder's one keystroke.
    ///
    /// Modifier keys pressed on their own are ignored rather than rejected —
    /// pressing `Ctrl` first and the letter second is how everyone types a
    /// chord, and treating the `Ctrl` as the binding would make that
    /// impossible.
    fn record_key(window: HWND, state: &mut State, virtual_key: u32) {
        if virtual_key == VK_ESCAPE.0 as u32 {
            unsafe { disarm(state) };
            set_text(state.hotkey_field, &state.setting.hotkey.to_string());
            return;
        }

        if is_modifier(virtual_key) {
            return;
        }

        let Some(code) = code_name(virtual_key) else {
            set_text(
                state.hotkey_note,
                "That key cannot be bound. Try a letter, a digit or a function key.",
            );
            return;
        };

        let mut text = String::new();
        // SAFETY: `GetKeyState` reads a thread-local table and takes no
        // pointer; the cast is the documented "high bit means down" test.
        unsafe {
            if GetKeyState(VK_CONTROL.0 as i32) < 0 {
                text.push_str("Ctrl+");
            }
            if GetKeyState(VK_MENU.0 as i32) < 0 {
                text.push_str("Alt+");
            }
            if GetKeyState(VK_SHIFT.0 as i32) < 0 {
                text.push_str("Shift+");
            }
            if GetKeyState(VK_LWIN.0 as i32) < 0 || GetKeyState(VK_RWIN.0 as i32) < 0 {
                text.push_str("Meta+");
            }
        }
        text.push_str(code);

        unsafe { disarm(state) };
        set_text(state.hotkey_field, &text);
        unsafe { take_hotkey(state) };
        let _ = window;
    }

    unsafe fn disarm(state: &mut State) {
        state.arming = false;
        set_text(state.record, "Record…");
    }

    /// Reads the hotkey field, and refuses anything the app could not honour.
    ///
    /// Two separate refusals, because they fail differently. A binding that
    /// does not parse is a typo. A binding that parses but is not
    /// [`Hotkey::is_safe`] — an unmodified printable key — would swallow that
    /// character everywhere on the machine for as long as splaude runs, and
    /// the user would have no way to type the field back to something else.
    unsafe fn take_hotkey(state: &mut State) {
        let typed = text_of(state.hotkey_field);
        let current = state.setting.hotkey.to_string();
        if typed.trim() == current {
            return;
        }

        match typed.trim().parse::<Hotkey>() {
            Ok(hotkey) if hotkey.is_safe() => {
                state.setting.hotkey = hotkey;
                set_text(state.hotkey_field, &hotkey.to_string());
                set_text(
                    state.hotkey_note,
                    &format!("Bound to {hotkey}. Hold it to talk, or tap it to latch."),
                );
                commit(state);
            }
            Ok(hotkey) => {
                set_text(
                    state.hotkey_note,
                    &format!(
                        "{hotkey} has no modifier, so it would swallow that key everywhere. \
                         Add Ctrl, Alt, Shift or Meta, or use a function key."
                    ),
                );
                set_text(state.hotkey_field, &current);
            }
            Err(why) => {
                set_text(state.hotkey_note, &format!("Not a binding: {why}"));
                set_text(state.hotkey_field, &current);
            }
        }
    }

    /// One term per line, blanks dropped — the same shape `commitKeyterm` has
    /// in the Swift build, and the same forgiveness about stray whitespace.
    unsafe fn take_keyterm(state: &mut State) {
        let typed = text_of(state.keyterm);
        let term: Vec<String> = typed
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();

        if term == state.setting.custom_keyterm {
            return;
        }

        state.setting.custom_keyterm = term;
        set_text(state.budget, &budget_line(&state.setting));
        commit(state);
    }

    /// Reads the corrections box: one rule per line, `heard = wanted`.
    ///
    /// A line with no `=` is skipped rather than rejected, because this box is
    /// also where somebody types a note to themselves, and refusing to save the
    /// whole file over it would cost them the rules that were fine.
    unsafe fn take_replacement(state: &mut State) {
        let typed = text_of(state.replacement);
        let mut rule = splaude_core::replacement::Rule::new();

        for line in typed.lines() {
            let Some((from, to)) = line.split_once('=') else {
                continue;
            };
            let from = from.trim();
            if from.is_empty() {
                continue;
            }
            rule.insert(from.to_string(), to.trim().to_string());
        }

        if rule == state.setting.replacement {
            return;
        }

        state.setting.replacement = rule;
        // Rewritten from what was parsed, so what is on screen is what was
        // saved — a line that was ignored visibly disappears rather than
        // sitting there looking like a rule.
        set_text(state.replacement, &replacement_text(&state.setting));
        commit(state);
    }

    /// Reads the latch timeout. Anything that is not a number is put back
    /// rather than saved as zero, which would silently turn the feature off.
    unsafe fn take_latch(state: &mut State) {
        let typed = text_of(state.latch);
        let current = state.setting.latch_timeout_second;

        match typed.trim().parse::<u32>() {
            Ok(second) if second == current => {}
            Ok(second) => {
                state.setting.latch_timeout_second = second;
                commit(state);
            }
            Err(_) => set_text(state.latch, &current.to_string()),
        }
    }

    unsafe fn take_api_key(state: &mut State) {
        let typed = text_of(state.api_key).trim().to_string();
        if typed == state.setting.api_key {
            return;
        }
        state.setting.api_key = typed;
        commit(state);
    }

    /// Whatever the controls hold that has not been committed yet.
    unsafe fn harvest(state: &mut State) {
        diagnostic::log(
            "trace",
            format!(
                "harvest: latch hwnd {:?} reads {:?}",
                state.latch.0 as usize,
                text_of(state.latch)
            ),
        );
        unsafe { take_hotkey(state) };
        unsafe { take_keyterm(state) };
        unsafe { take_replacement(state) };
        unsafe { take_latch(state) };
        unsafe { take_api_key(state) };
    }

    /// Writes the file and says so.
    ///
    /// The window never applies a setting itself — not the hotkey, not launch
    /// at login. It writes, and the event loop reloads, because that is the
    /// path that already knows how to move a live registration and how to
    /// reconcile the machine with the file. A window that applied its own
    /// changes would be a second implementation of every one of those rules.
    fn commit(state: &State) {
        match state.setting.save() {
            Ok(()) => (state.on_save)(),
            Err(error) => diagnostic::log(
                "setting",
                format!("could not save from the settings window: {error}"),
            ),
        }
    }

    // MARK: - Control helpers

    fn show_page(state: &State, page: usize) {
        for (index, control) in state.page.iter().enumerate() {
            let how = if index == page { SW_SHOW } else { SW_HIDE };
            for window in control {
                // SAFETY: every handle here was created by this window and is
                // destroyed with it, so none can be stale while this runs.
                unsafe {
                    let _ = ShowWindow(*window, how);
                }
            }
        }
    }

    unsafe fn state_of<'a>(window: HWND) -> Option<&'a mut State> {
        let held = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) };
        if held == 0 {
            return None;
        }
        // SAFETY: the pointer is installed once in `run` and cleared in
        // `WM_NCDESTROY` before it is freed, and every caller is on the window's
        // own thread — so no two live borrows can exist at once.
        Some(unsafe { &mut *(held as *mut State) })
    }

    /// How many device pixels a logical one is worth, as a percentage.
    ///
    /// Every number in this module is written at 100% — `MARGIN`, `LINE`, the
    /// control sizes — and scaled here, in the one place every control is
    /// created. That is the whole of the DPI story: the process declares itself
    /// system-aware in `main`, so Windows stops bitmap-stretching it, and the
    /// layout has to do the arithmetic Windows used to do badly.
    ///
    /// A percentage rather than a float because integer arithmetic on control
    /// coordinates cannot drift: `120 * 150 / 100` is exactly 180 on every run,
    /// where a float multiply can land a pixel either side and put a one-pixel
    /// gap between a checkbox and its label.
    fn dpi_percent() -> i32 {
        // SAFETY: takes no arguments and reads a process-wide value.
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() } as i32;
        percent_of(dpi)
    }

    /// The scale a DPI reading means, as a percentage.
    ///
    /// Separated from the call that reads it so the arithmetic can be checked
    /// at every scale factor a person actually has. The pixels themselves need
    /// a display set to 150% and a pair of eyes; this is the half that does not,
    /// and it is the half that can be wrong by a rounding rule.
    fn percent_of(dpi: i32) -> i32 {
        // A zero would be nonsense from the API and a division by it here; 96
        // is the value that means "no scaling".
        if dpi <= 0 {
            100
        } else {
            dpi * 100 / 96
        }
    }

    /// A logical coordinate in device pixels.
    fn scaled(value: i32) -> i32 {
        at_percent(value, dpi_percent())
    }

    fn at_percent(value: i32, percent: i32) -> i32 {
        value * percent / 100
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn child(
        parent: HWND,
        class: PCWSTR,
        text: PCWSTR,
        style: WINDOW_STYLE,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        id: usize,
        instance: HINSTANCE,
    ) -> HWND {
        // SAFETY: the class is a registered system class or the one this module
        // registered, the parent outlives every child, and the id is only ever
        // read back out of `WM_COMMAND`.
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                text,
                WS_CHILD | style,
                scaled(x),
                scaled(y),
                scaled(width),
                scaled(height),
                Some(parent),
                Some(HMENU(id as *mut c_void)),
                Some(instance),
                None,
            )
            .unwrap_or_default()
        }
    }

    unsafe fn checkbox(
        parent: HWND,
        title: &str,
        id: usize,
        on: bool,
        x: i32,
        y: i32,
        instance: HINSTANCE,
    ) -> HWND {
        let text = HSTRING::from(title);
        let window = unsafe {
            child(
                parent,
                w!("BUTTON"),
                PCWSTR(text.as_ptr()),
                WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
                x,
                y,
                FULL - 24,
                LINE,
                id,
                instance,
            )
        };
        unsafe {
            SendMessageW(
                window,
                BM_SETCHECK,
                Some(WPARAM(if on {
                    BST_CHECKED.0 as usize
                } else {
                    BST_UNCHECKED.0 as usize
                })),
                None,
            )
        };
        window
    }

    unsafe fn label(parent: HWND, title: &str, x: i32, y: i32, instance: HINSTANCE) -> HWND {
        let text = HSTRING::from(title);
        unsafe {
            child(
                parent,
                w!("STATIC"),
                PCWSTR(text.as_ptr()),
                WINDOW_STYLE(0),
                x,
                y,
                FULL - 24,
                LINE,
                0,
                instance,
            )
        }
    }

    unsafe fn caption(
        parent: HWND,
        body: &str,
        x: i32,
        y: i32,
        width: i32,
        line: i32,
        instance: HINSTANCE,
    ) -> HWND {
        let text = HSTRING::from(body);
        unsafe {
            child(
                parent,
                w!("STATIC"),
                PCWSTR(text.as_ptr()),
                WINDOW_STYLE(0),
                x,
                y,
                width,
                LINE * line,
                ID_CAPTION,
                instance,
            )
        }
    }

    fn checked(window: HWND, id: usize) -> bool {
        // SAFETY: the id belongs to a control of this window, and a missing one
        // answers `Err` rather than faulting.
        unsafe {
            GetDlgItem(Some(window), id as i32)
                .map(|control| {
                    SendMessageW(control, BM_GETCHECK, None, None).0 == BST_CHECKED.0 as isize
                })
                .unwrap_or(false)
        }
    }

    fn set_font(window: HWND, font: HFONT) {
        // SAFETY: the font outlives every control that uses it — it is freed in
        // `WM_NCDESTROY`, after the children are gone.
        unsafe {
            SendMessageW(
                window,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            )
        };
    }

    fn set_text(window: HWND, text: &str) {
        let text = HSTRING::from(text);
        // SAFETY: the string outlives the call, which copies it.
        unsafe {
            let _ = SetWindowTextW(window, &text);
        }
    }

    fn text_of(window: HWND) -> String {
        // SAFETY: the length is asked for first and the buffer is sized from
        // it, including the terminator the API always writes.
        unsafe {
            let length = GetWindowTextLengthW(window);
            if length <= 0 {
                return String::new();
            }
            let mut buffer = vec![0u16; length as usize + 1];
            let written = GetWindowTextW(window, &mut buffer);
            String::from_utf16_lossy(&buffer[..written as usize])
        }
    }

    /// The font the rest of the system's dialogs use.
    ///
    /// Without this every control comes up in the ancient bitmap system font,
    /// which is the single loudest way a hand-written Win32 window announces
    /// that nobody looked at it.
    fn message_font() -> HFONT {
        let mut metric = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        // SAFETY: the size is stamped as the API requires and the pointer is to
        // a live local of exactly that size.
        let read = unsafe {
            SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                metric.cbSize,
                Some(std::ptr::addr_of_mut!(metric).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        if read.is_err() {
            return HFONT::default();
        }
        // SAFETY: the descriptor is a live local and the call copies from it.
        unsafe { CreateFontIndirectW(&metric.lfMessageFont) }
    }

    #[cfg(test)]
    mod scaling {
        use super::*;

        /// 96 DPI is 100%, and every common Windows scale factor is a whole
        /// number of percent — so none of these should round at all.
        #[test]
        fn every_common_scale_factor_is_exact() {
            assert_eq!(percent_of(96), 100);
            assert_eq!(percent_of(120), 125);
            assert_eq!(percent_of(144), 150);
            assert_eq!(percent_of(168), 175);
            assert_eq!(percent_of(192), 200);
            assert_eq!(percent_of(240), 250);
        }

        /// A nonsensical reading must not divide by zero or collapse the
        /// window to nothing; it means "do not scale".
        #[test]
        fn a_useless_reading_means_no_scaling() {
            assert_eq!(percent_of(0), 100);
            assert_eq!(percent_of(-1), 100);
        }

        #[test]
        fn scaling_at_one_hundred_percent_changes_nothing() {
            for value in [0, 1, 14, 22, 560, 760] {
                assert_eq!(at_percent(value, 100), value);
            }
        }

        /// The numbers this module is actually written in, at the scale factor
        /// most likely to be met after 100%.
        #[test]
        fn the_layout_grows_with_the_display() {
            // MARGIN, LINE, and the window's own width.
            assert_eq!(at_percent(14, 150), 21);
            assert_eq!(at_percent(22, 150), 33);
            assert_eq!(at_percent(WIDTH, 150), 840);
            assert_eq!(at_percent(HEIGHT, 200), 1_520);
        }

        /// Integer arithmetic throughout, so two controls given the same
        /// logical coordinate land on the same device pixel — a float multiply
        /// can put them one pixel apart and leave a seam between a checkbox and
        /// its label.
        #[test]
        fn the_same_logical_coordinate_always_lands_on_the_same_pixel() {
            for percent in [100, 125, 150, 175, 200] {
                assert_eq!(
                    at_percent(MARGIN + 10, percent),
                    at_percent(MARGIN + 10, percent)
                );
            }
        }

        /// Ordering has to survive scaling, or controls overlap: whatever was
        /// below stays below.
        #[test]
        fn scaling_preserves_the_order_of_the_layout() {
            for percent in [100, 125, 150, 200] {
                let first = at_percent(PAGE_TOP, percent);
                let second = at_percent(PAGE_TOP + LINE, percent);
                assert!(second > first, "at {percent}%");
            }
        }
    }

    // MARK: - Text

    fn interval_line(micros: u32) -> String {
        format!(
            "{micros} µs between keystrokes. Lower is snappier; too low and some Electron apps drop characters."
        )
    }

    /// Both ceilings, because there are two and only one of them used to be
    /// shown. A list of short names can sit well inside the character budget
    /// and still be cut by the term count, and a budget line that mentioned
    /// only characters would say everything was fine while terms were being
    /// dropped.
    fn budget_line(setting: &Setting) -> String {
        let asked = setting.keyterm();
        let packed = pack_keyterm(&asked);
        let used = packed.chars().count();
        let kept = if packed.is_empty() {
            0
        } else {
            packed.split(',').count()
        };

        let dropped = asked.len().saturating_sub(kept);
        let note = if dropped == 0 {
            String::from("Everything fits.")
        } else {
            format!("{dropped} term(s) past the limit are not sent.")
        };

        format!(
            "Budget: {used} / {KEYTERM_BUDGET} characters, {kept} / {KEYTERM_TERM_CEILING} terms. {note}"
        )
    }

    /// Names the device, because "yes" is not actionable and the name is: a
    /// user who sees the wrong headset here knows exactly what to change.
    fn microphone_line() -> String {
        match splaude_platform::audio::default_input_name() {
            Some(name) => format!("Input device: {name}"),
            None => "No input device. Dictation cannot start until Windows has one — check Sound settings, and if this is a Remote Desktop session, that it is redirecting audio.".to_string(),
        }
    }

    fn open_sound_setting() {
        // `explorer.exe` is the documented way to hand a `ms-settings:` URI to
        // the Settings app, and it exits non-zero even when it worked, so only
        // failing to spawn at all is worth reporting.
        let spawned = std::process::Command::new("explorer.exe")
            .arg("ms-settings:sound")
            .spawn();
        if let Err(error) = spawned {
            diagnostic::log("setting", format!("could not open sound settings: {error}"));
        }
    }

    /// The corrections as the box shows them: one `heard = wanted` per line, in
    /// the map's own order, so the file and the window agree.
    fn replacement_text(setting: &Setting) -> String {
        setting
            .replacement
            .iter()
            .map(|(from, to)| format!("{from} = {to}"))
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    }

    /// What the connection actually is, for the provider actually selected.
    ///
    /// Read from the setting rather than fixed, because the sentence about not
    /// spending Claude tokens is true of only one of the two, and printing it
    /// under the other would be the window lying about where the audio goes.
    fn connection_line(setting: &Setting) -> String {
        match setting.speech_backend.trim() {
            "deepgram" => "Speech streams to Deepgram directly, on your own key and your own bill. Nothing goes to Anthropic on this setting."
                .to_string(),
            _ => "Speech streams to Anthropic's internal WebSocket, which runs Deepgram Nova-3 rather than a Claude model — so it does not spend Claude tokens. It is undocumented and can change without notice."
                .to_string(),
        }
    }

    fn credential_line() -> String {
        Store::file_only().describe()
    }

    /// The connection paragraph, as it reads before anybody has asked about
    /// updates.
    ///
    /// The window used to call [`update::check`] while building this page,
    /// which is a network request in the middle of window construction:
    /// nothing appeared on screen until GitHub answered, so opening Settings
    /// took seconds on a good connection and a timeout's worth on a bad one,
    /// with no window and no spinner to say why. That is this project's
    /// signature failure — something reasonable happening invisibly — and it
    /// was measured here rather than reasoned about: a test driving the window
    /// found the controls still being created four seconds after launch.
    ///
    /// The check belongs to *Re-check*, beside the two other things that button
    /// already refreshes. Opening the window now touches no network at all.
    fn connection_text(setting: &Setting) -> String {
        format!(
            "{}\n\nRate limit: {}\nUpdate: {}",
            connection_line(setting),
            quota::summary(),
            "click Re-check",
        )
    }

    /// The same paragraph with the update actually checked. Only ever called
    /// from the *Re-check* button, on a click, where waiting is expected.
    fn connection_checked(setting: &Setting) -> String {
        format!(
            "{}\n\nRate limit: {}\nUpdate: {}",
            connection_line(setting),
            quota::summary(),
            update_line(),
        )
    }

    fn update_line() -> String {
        update::check().line()
    }

    fn mark(yes: bool) -> &'static str {
        if yes {
            "yes"
        } else {
            "no"
        }
    }

    fn round_interval(position: i32) -> i32 {
        let stepped = ((position + INTERVAL_STEP / 2) / INTERVAL_STEP) * INTERVAL_STEP;
        stepped.clamp(INTERVAL_FLOOR, INTERVAL_CEILING)
    }

    /// The W3C `code` name for a virtual key, or `None` for one that has no
    /// business being a push-to-talk binding.
    ///
    /// Names rather than `keyboard_types::Code` values, because the parser this
    /// feeds is the same one that reads the settings file — so a name this
    /// table gets wrong fails as a refused binding the user can see, rather
    /// than as a value that compiles and means something else.
    fn code_name(virtual_key: u32) -> Option<&'static str> {
        const LETTER: [&str; 26] = [
            "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK",
            "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV",
            "KeyW", "KeyX", "KeyY", "KeyZ",
        ];
        const DIGIT: [&str; 10] = [
            "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7",
            "Digit8", "Digit9",
        ];
        const FUNCTION: [&str; 20] = [
            "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13",
            "F14", "F15", "F16", "F17", "F18", "F19", "F20",
        ];

        match virtual_key {
            0x41..=0x5A => Some(LETTER[(virtual_key - 0x41) as usize]),
            0x30..=0x39 => Some(DIGIT[(virtual_key - 0x30) as usize]),
            0x70..=0x83 => Some(FUNCTION[(virtual_key - 0x70) as usize]),
            0x20 => Some("Space"),
            0xBF => Some("Slash"),
            0xBE => Some("Period"),
            0xBC => Some("Comma"),
            0xBD => Some("Minus"),
            0xBB => Some("Equal"),
            0xBA => Some("Semicolon"),
            0xDE => Some("Quote"),
            0xDB => Some("BracketLeft"),
            0xDD => Some("BracketRight"),
            0xDC => Some("Backslash"),
            0xC0 => Some("Backquote"),
            _ => None,
        }
    }

    fn is_modifier(virtual_key: u32) -> bool {
        matches!(
            virtual_key,
            0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5
        )
    }

    #[cfg(test)]
    mod test {
        use super::*;

        #[test]
        fn the_slider_lands_on_hundreds() {
            assert_eq!(round_interval(1249), 1200);
            assert_eq!(round_interval(1250), 1300);
            assert_eq!(round_interval(0), INTERVAL_FLOOR);
            assert_eq!(round_interval(9999), INTERVAL_CEILING);
        }

        /// Every name this table produces has to be one the settings parser
        /// accepts, or the recorder would offer bindings that cannot be saved.
        #[test]
        fn every_recorded_name_parses_as_a_binding() {
            for key in 0x00u32..=0xFFu32 {
                let Some(name) = code_name(key) else {
                    continue;
                };
                let binding = format!("Ctrl+{name}");
                assert!(
                    binding.parse::<Hotkey>().is_ok(),
                    "{name} (vk {key:#x}) does not parse"
                );
            }
        }

        #[test]
        fn a_bare_function_key_is_bindable_and_a_bare_letter_is_not() {
            assert!("F9".parse::<Hotkey>().unwrap().is_safe());
            assert!(!"KeyD".parse::<Hotkey>().unwrap().is_safe());
        }

        #[test]
        fn a_modifier_alone_is_not_a_binding() {
            for key in [0x10u32, 0x11, 0x12, 0x5B] {
                assert!(is_modifier(key));
                assert!(code_name(key).is_none());
            }
        }

        #[test]
        fn the_letter_table_is_in_order() {
            assert_eq!(code_name(0x41), Some("KeyA"));
            assert_eq!(code_name(0x5A), Some("KeyZ"));
            assert_eq!(code_name(0x30), Some("Digit0"));
            assert_eq!(code_name(0x70), Some("F1"));
            assert_eq!(code_name(0x83), Some("F20"));
        }
    }
}

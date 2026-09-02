//! The floating mic button.
//!
//! A round mark that sits above every window, shows whether a take is on the
//! air, and starts or stops one when clicked. `FloatingMic.swift` is the shape
//! this follows, including the two properties that are easy to miss and are the
//! whole reason it can exist at all:
//!
//! **It never takes keyboard focus.** A dictation lands wherever focus is, so a
//! button that stole focus to be clicked would move the text into itself. The
//! Swift build uses a non-activating `NSPanel`; here it is `WS_EX_NOACTIVATE`
//! plus an `MA_NOACTIVATE` answer to [`WM_MOUSEACTIVATE`], which is the same
//! promise made twice — the style keeps the window out of the activation order,
//! and the reply keeps the click itself from activating it.
//!
//! **It is a shape, not a rectangle.** The mark has transparent corners, and a
//! square window with a picture of a round button in it would black them in
//! over whatever is behind. That is what [`UpdateLayeredWindow`] is for: the
//! window is composited from a premultiplied bitmap with its own alpha, so the
//! corners are genuinely not there — clicks included.
//!
//! Like the settings window it runs on a thread of its own with its own message
//! queue, for the same reason: `tao` owns the main thread and does not share it.
//! Everything the event loop wants to say arrives as a posted message, which is
//! the one thing that is safe to send at a window from another thread.

#[cfg(windows)]
pub use backend::{show, Handle};

#[cfg(not(windows))]
pub use fallback::{show, Handle};

/// No floating button off Windows. macOS has the Swift build's own, and Linux
/// has neither a tray to toggle it from nor, under Wayland, permission to place
/// a window above other clients.
#[cfg(not(windows))]
mod fallback {
    use anyhow::Result;
    use splaude_core::setting::Point;

    use crate::icon::Mood;

    #[derive(Debug)]
    pub struct Handle;

    impl Handle {
        pub fn set_mood(&self, _mood: Mood) {}
        pub fn close(&self) {}
    }

    pub fn show(
        _at: Option<Point>,
        _on_click: Box<dyn Fn() + Send>,
        _on_move: Box<dyn Fn(Point) + Send>,
    ) -> Result<Handle> {
        anyhow::bail!("the floating button is only implemented on Windows")
    }
}

#[cfg(windows)]
mod backend {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::Arc;

    use anyhow::{anyhow, Result};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{
        COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, MonitorFromRect,
        ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        BLENDFUNCTION, DIB_RGB_COLORS, MONITOR_DEFAULTTONULL,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
    use windows::Win32::UI::WindowsAndMessaging::*;

    use splaude_core::setting::Point;

    use crate::icon::{self, Mood};

    /// How wide the button is, in pixels.
    ///
    /// Bigger than the 32-pixel tray mark because this one is a target as well
    /// as an indicator: it has to be hittable without aiming, and it is drawn
    /// from the same renderer at whatever size is asked for, so nothing is
    /// scaled up.
    const EDGE: i32 = 56;

    /// How far the pointer may travel between press and release and still count
    /// as a click rather than a drag.
    ///
    /// Zero would make the button undraggable-looking — a hand on a mouse moves
    /// a pixel or two during a click, and every one of those would land as a
    /// move that saved a new position. Too large and a short deliberate drag
    /// would start a take instead.
    const DRAG_SLOP: i32 = 4;

    /// The button's size in device pixels, from its logical [`EDGE`].
    ///
    /// Scaled rather than fixed because the process declares itself DPI-aware
    /// at startup, so nothing stretches its windows any more. The mark is drawn
    /// by [`icon::render`] at whatever size is asked for and every proportion in
    /// it is a fraction of the canvas, so this is a genuinely sharper button at
    /// 150% rather than the same bitmap enlarged and blurred.
    fn edge() -> i32 {
        // SAFETY: takes no arguments and reads a process-wide value.
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() } as i32;
        if dpi <= 0 {
            EDGE
        } else {
            EDGE * dpi / 96
        }
    }

    /// How far from the corner the button sits when it has never been moved.
    const DEFAULT_INSET: i32 = 120;

    /// Set the mood. `wparam` is [`encode`]d, because a posted message carries
    /// integers and not a `Mood`.
    const WM_MOOD: u32 = WM_APP + 1;
    /// Close, from the thread that asked for the button in the first place.
    const WM_DISMISS: u32 = WM_APP + 2;

    const CLASS: PCWSTR = w!("splaude.floating.button");

    /// A handle on the button, usable from the event loop's thread.
    ///
    /// Everything it can do is a posted message, which is the only cross-thread
    /// operation on a window that Win32 promises is safe. Nothing here waits for
    /// an answer, so a button whose thread is busy painting cannot stall a take.
    #[derive(Debug)]
    pub struct Handle {
        window: Arc<AtomicIsize>,
    }

    impl Handle {
        pub fn set_mood(&self, mood: Mood) {
            self.post(WM_MOOD, WPARAM(encode(mood)));
        }

        pub fn close(&self) {
            self.post(WM_DISMISS, WPARAM(0));
        }

        fn post(&self, message: u32, wparam: WPARAM) {
            let held = self.window.load(Ordering::SeqCst);
            if held == 0 {
                return;
            }
            // SAFETY: the handle is stored while the window lives and zeroed by
            // the window procedure before it is destroyed. A post to a window
            // that has gone anyway fails; it does not fault.
            unsafe {
                let _ = PostMessageW(Some(HWND(held as *mut c_void)), message, wparam, LPARAM(0));
            }
        }
    }

    /// State the window procedure owns, hung off the window.
    struct State {
        mood: Mood,
        on_click: Box<dyn Fn() + Send>,
        on_move: Box<dyn Fn(Point) + Send>,
        /// Where the pointer was, in screen coordinates, when the button was
        /// pressed — and where the window was. A drag moves the window by the
        /// difference, which is what keeps the mark under the pointer instead
        /// of snapping its corner there.
        grabbed: Option<(POINT, POINT)>,
        dragged: bool,
        window: Arc<AtomicIsize>,
    }

    /// Puts the button on screen and returns a handle to it.
    ///
    /// `at` is where it was last left, or `None` for a button that has never
    /// been moved. `on_click` and `on_move` are called on the button's own
    /// thread, so both must be safe to run somewhere that is not the event
    /// loop — posting to the loop is exactly that.
    pub fn show(
        at: Option<Point>,
        on_click: Box<dyn Fn() + Send>,
        on_move: Box<dyn Fn(Point) + Send>,
    ) -> Result<Handle> {
        let window = Arc::new(AtomicIsize::new(0));
        let shared = Arc::clone(&window);

        let (ready, waiting) = std::sync::mpsc::channel::<Result<(), String>>();

        std::thread::Builder::new()
            .name("splaude-floating".into())
            .spawn(move || {
                match build(at, on_click, on_move, Arc::clone(&shared)) {
                    Ok(created) => {
                        let _ = ready.send(Ok(()));
                        // SAFETY: the window belongs to this thread, which is
                        // the only one allowed to pump it.
                        unsafe { pump(created) };
                    }
                    Err(error) => {
                        let _ = ready.send(Err(format!("{error:#}")));
                    }
                }
                shared.store(0, Ordering::SeqCst);
            })
            .map_err(|error| anyhow!("could not start the floating button: {error}"))?;

        // Waited for so a failure to create the window is an error the caller
        // can log, rather than a button that silently never appears.
        match waiting.recv() {
            Ok(Ok(())) => Ok(Handle { window }),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(_) => Err(anyhow!("the floating button ended before it appeared")),
        }
    }

    fn build(
        at: Option<Point>,
        on_click: Box<dyn Fn() + Send>,
        on_move: Box<dyn Fn(Point) + Send>,
        shared: Arc<AtomicIsize>,
    ) -> Result<HWND> {
        // SAFETY: every call is a plain Win32 constructor over values that
        // outlive it. The boxed `State` is leaked into the window's user data
        // and reclaimed in `WM_NCDESTROY`, which is the only place it is freed.
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None)?.into();

            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: CLASS,
                hCursor: LoadCursorW(None, IDC_HAND)?,
                ..Default::default()
            };
            RegisterClassW(&class);

            let place = at.and_then(on_a_display).unwrap_or_else(default_place);

            let window = CreateWindowExW(
                // LAYERED for the per-pixel alpha, TOPMOST so it is above the
                // app being dictated into, TOOLWINDOW to keep it out of the
                // taskbar and the Alt+Tab list — it is a control, not a window
                // anyone switches to — and NOACTIVATE so clicking it cannot
                // take focus away from the field the text has to land in.
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                CLASS,
                w!("splaude"),
                WS_POPUP,
                place.x,
                place.y,
                edge(),
                edge(),
                None,
                None,
                Some(instance),
                None,
            )?;

            let state = Box::new(State {
                mood: Mood::Idle,
                on_click,
                on_move,
                grabbed: None,
                dragged: false,
                window: Arc::clone(&shared),
            });
            SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(state) as isize);

            paint(window, Mood::Idle);
            shared.store(window.0 as isize, Ordering::SeqCst);

            // SW_SHOWNOACTIVATE rather than SW_SHOW: showing a window is
            // normally also activating it, and this one must never be active.
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);

            Ok(window)
        }
    }

    unsafe fn pump(window: HWND) {
        let mut message = MSG::default();
        // Bound to this window rather than the thread: the settings window may
        // be running on another thread of this same process, and a filter of
        // `None` here would be this loop eating messages that belong to it.
        while unsafe { GetMessageW(&mut message, Some(window), 0, 0) }.as_bool() {
            let _ = unsafe { TranslateMessage(&message) };
            unsafe { DispatchMessageW(&message) };
        }
    }

    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            // The second half of the no-focus promise. Without this a click
            // activates the window despite `WS_EX_NOACTIVATE`, and the field
            // the user was dictating into loses the caret.
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),

            WM_LBUTTONDOWN => {
                if let Some(state) = unsafe { state_of(window) } {
                    // SAFETY: both read state the window already owns.
                    unsafe {
                        let mut pointer = POINT::default();
                        let _ = GetCursorPos(&mut pointer);
                        let mut frame = Default::default();
                        let _ = GetWindowRect(window, &mut frame);
                        state.grabbed = Some((
                            pointer,
                            POINT {
                                x: frame.left,
                                y: frame.top,
                            },
                        ));
                        state.dragged = false;
                        SetCapture(window);
                    }
                }
                LRESULT(0)
            }

            WM_MOUSEMOVE => {
                if let Some(state) = unsafe { state_of(window) } {
                    if let Some((from, origin)) = state.grabbed {
                        // SAFETY: reads the cursor and moves a window this
                        // thread owns.
                        unsafe {
                            let mut pointer = POINT::default();
                            let _ = GetCursorPos(&mut pointer);
                            let dx = pointer.x - from.x;
                            let dy = pointer.y - from.y;
                            if dx.abs() > DRAG_SLOP || dy.abs() > DRAG_SLOP {
                                state.dragged = true;
                            }
                            if state.dragged {
                                let _ = SetWindowPos(
                                    window,
                                    None,
                                    origin.x + dx,
                                    origin.y + dy,
                                    0,
                                    0,
                                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                                );
                            }
                        }
                    }
                }
                LRESULT(0)
            }

            WM_LBUTTONUP => {
                if let Some(state) = unsafe { state_of(window) } {
                    // SAFETY: releases a capture this window took and reads its
                    // own frame.
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    let was_dragged = state.dragged;
                    state.grabbed = None;
                    state.dragged = false;

                    if was_dragged {
                        // SAFETY: as above.
                        let mut frame = Default::default();
                        let _ = unsafe { GetWindowRect(window, &mut frame) };
                        (state.on_move)(Point {
                            x: f64::from(frame.left),
                            y: f64::from(frame.top),
                        });
                    } else {
                        (state.on_click)();
                    }
                }
                LRESULT(0)
            }

            // Capture can be taken away — another window grabs it, the user
            // Alt+Tabs, a menu opens — and the release that would have ended
            // the drag then never arrives. Without this the button would go on
            // following the pointer with no button held down, which is the one
            // way a thing that floats above every window becomes something the
            // user is fighting.
            WM_CAPTURECHANGED => {
                if let Some(state) = unsafe { state_of(window) } {
                    state.grabbed = None;
                    state.dragged = false;
                }
                LRESULT(0)
            }

            WM_MOOD => {
                if let Some(state) = unsafe { state_of(window) } {
                    let mood = decode(wparam.0);
                    if state.mood != mood {
                        state.mood = mood;
                        unsafe { paint(window, mood) };
                    }
                }
                LRESULT(0)
            }

            WM_DISMISS => {
                let _ = unsafe { DestroyWindow(window) };
                LRESULT(0)
            }

            WM_NCDESTROY => {
                let held = unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
                if held != 0 {
                    // SAFETY: made by `Box::into_raw` in `build`, freed exactly
                    // here, and unreachable afterwards because the pointer was
                    // cleared before this ran.
                    let state = unsafe { Box::from_raw(held as *mut State) };
                    state.window.store(0, Ordering::SeqCst);
                }
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }

            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    /// Draws the mark into the window's own layer.
    ///
    /// Not `WM_PAINT`: a layered window built this way has no paint cycle at
    /// all. The bitmap *is* the window, so redrawing means handing the
    /// compositor a new one, and the corners stay transparent because the alpha
    /// travels with it.
    unsafe fn paint(window: HWND, mood: Mood) {
        let side = edge();
        let pixel = icon::render(mood, side as u32);

        // SAFETY: every handle below is created here and released here, in the
        // reverse order, and the bitmap bits are written through a pointer the
        // system just gave us for exactly `edge * edge * 4` bytes.
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return;
            }
            let memory = CreateCompatibleDC(Some(screen));

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: side,
                    // Negative for a top-down bitmap, so row zero is the top —
                    // the order the renderer produces.
                    biHeight: -side,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0);
            let Ok(bitmap) = bitmap else {
                let _ = DeleteDC(memory);
                ReleaseDC(None, screen);
                return;
            };

            // RGBA, straight, to BGRA, premultiplied — which is the only thing
            // `UpdateLayeredWindow` accepts. Skipping the multiply is the
            // classic way to get a button with a bright halo around it.
            let count = (side * side) as usize;
            let out = std::slice::from_raw_parts_mut(bits.cast::<u8>(), count * 4);
            for index in 0..count {
                let red = u32::from(pixel[index * 4]);
                let green = u32::from(pixel[index * 4 + 1]);
                let blue = u32::from(pixel[index * 4 + 2]);
                let alpha = u32::from(pixel[index * 4 + 3]);

                out[index * 4] = ((blue * alpha) / 255) as u8;
                out[index * 4 + 1] = ((green * alpha) / 255) as u8;
                out[index * 4 + 2] = ((red * alpha) / 255) as u8;
                out[index * 4 + 3] = alpha as u8;
            }

            let previous = SelectObject(memory, bitmap.into());

            let mut frame = Default::default();
            let _ = GetWindowRect(window, &mut frame);
            let place = POINT {
                x: frame.left,
                y: frame.top,
            };
            let size = windows::Win32::Foundation::SIZE { cx: side, cy: side };
            let source = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };

            let _ = UpdateLayeredWindow(
                window,
                Some(screen),
                Some(&place),
                Some(&size),
                Some(memory),
                Some(&source),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );

            SelectObject(memory, previous);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
        }
    }

    unsafe fn state_of<'a>(window: HWND) -> Option<&'a mut State> {
        let held = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) };
        if held == 0 {
            return None;
        }
        // SAFETY: installed once in `build`, cleared in `WM_NCDESTROY` before
        // it is freed, and only ever read on the window's own thread.
        Some(unsafe { &mut *(held as *mut State) })
    }

    /// The saved position, but only if some display still covers it.
    ///
    /// A button restored onto a monitor that has since been unplugged is a
    /// 56-pixel topmost window nobody can see, with no menu on it and no way
    /// back except hand-editing the settings file — and it is reached by the
    /// ordinary act of docking a laptop, dragging the button to the big screen,
    /// and undocking. The Swift build has guarded this since it shipped
    /// (`isOnScreen` in `FloatingMic.swift`); the port did not, and this is that
    /// guard.
    ///
    /// Asked of the whole button rather than of its top-left corner, so a button
    /// left straddling an edge counts as visible — which is what a person
    /// looking at it would say.
    fn on_a_display(point: Point) -> Option<POINT> {
        let place = POINT {
            x: point.x as i32,
            y: point.y as i32,
        };
        let frame = RECT {
            left: place.x,
            top: place.y,
            right: place.x + edge(),
            bottom: place.y + edge(),
        };

        // SAFETY: takes a rectangle by pointer and returns a monitor handle or
        // null; nothing is borrowed past the call. `MONITOR_DEFAULTTONULL` is
        // the whole point — the other flags would answer with the nearest
        // monitor, which is exactly the lie being guarded against.
        let monitor = unsafe { MonitorFromRect(&frame, MONITOR_DEFAULTTONULL) };
        (!monitor.is_invalid()).then_some(place)
    }

    /// Bottom right, a comfortable distance in from the corner — clear of the
    /// notification area, which is the one part of the screen a user is likely
    /// to be aiming at when splaude is on their mind.
    fn default_place() -> POINT {
        // SAFETY: reads two system metrics and takes no pointer.
        unsafe {
            POINT {
                x: GetSystemMetrics(SM_CXSCREEN) - DEFAULT_INSET,
                y: GetSystemMetrics(SM_CYSCREEN) - DEFAULT_INSET,
            }
        }
    }

    /// A [`Mood`] as a single integer, so it can ride in a posted message.
    fn encode(mood: Mood) -> usize {
        match mood {
            Mood::Idle => 0,
            Mood::Recording(step) => 1 + step as usize,
        }
    }

    fn decode(value: usize) -> Mood {
        match value {
            0 => Mood::Idle,
            other => Mood::Recording((other - 1).min(icon::STEP_COUNT as usize) as u8),
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;

        #[test]
        fn every_mood_survives_the_round_trip() {
            assert_eq!(decode(encode(Mood::Idle)), Mood::Idle);
            for step in 0..=icon::STEP_COUNT {
                let mood = Mood::Recording(step);
                assert_eq!(decode(encode(mood)), mood);
            }
        }

        /// A posted message carries whatever the sender put in it, and this
        /// decoder is the only thing standing between a bad value and an index
        /// out of range inside the renderer.
        #[test]
        fn an_impossible_step_is_clamped_rather_than_trusted() {
            assert_eq!(decode(usize::MAX), Mood::Recording(icon::STEP_COUNT));
        }

        #[test]
        fn idle_is_the_only_zero() {
            assert_eq!(encode(Mood::Idle), 0);
            assert_ne!(encode(Mood::Recording(0)), 0);
        }
    }
}

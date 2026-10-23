//! Window features GPUI has no API for: keeping a window above all others (the pin of a
//! standalone terminal) and passthrough (clicks fall through to what is behind). Windows only;
//! elsewhere the calls do nothing and the buttons are hidden.
#![allow(unsafe_code)]

use gpui_kit::Window;

/// Whether this platform can pin windows and let clicks through; the buttons are hidden
/// elsewhere.
pub const SUPPORTED: bool = cfg!(windows);

/// A native window, kept to change it from outside its own update (the passthrough title
/// strip moves the terminal window it is glued to). Only valid while that window is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeWindow(isize);

/// The native window behind `window`.
pub fn native(window: &Window) -> Option<NativeWindow> {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // GPUI's own `Window::window_handle` returns its handle type; this is the raw one.
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(raw) => Some(NativeWindow(raw.hwnd.get())),
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, GetWindowRect, HWND_NOTOPMOST, HWND_TOPMOST, LWA_ALPHA,
        SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow, WS_EX_APPWINDOW,
        WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
    };

    use super::NativeWindow;

    fn hwnd(window: NativeWindow) -> HWND {
        window.0 as HWND
    }

    /// Sets (`on`) or clears the extended style bits `bits`.
    fn set_ex_style(window: NativeWindow, bits: u32, on: bool) {
        // SAFETY: `window` is a live window of this process; only its style bits change.
        unsafe {
            let style = GetWindowLongPtrW(hwnd(window), GWL_EXSTYLE) as u32;
            let style = if on { style | bits } else { style & !bits };
            SetWindowLongPtrW(hwnd(window), GWL_EXSTYLE, style as isize);
        }
    }

    pub fn set_topmost(window: NativeWindow, on: bool) -> bool {
        let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
        // SAFETY: a live window of this process; only its z-order changes.
        unsafe {
            SetWindowPos(
                hwnd(window),
                after,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            ) != 0
        }
    }

    pub fn set_click_through(window: NativeWindow, on: bool) {
        // Layered + transparent is what lets the mouse fall through to other programs.
        set_ex_style(window, WS_EX_LAYERED | WS_EX_TRANSPARENT, on);
        if on {
            // A layered window stays invisible until its attributes are set; fully opaque here,
            // the content's own alpha still applies.
            // SAFETY: a live window of this process.
            unsafe { SetLayeredWindowAttributes(hwnd(window), 0, 255, LWA_ALPHA) };
        }
    }

    pub fn hide_from_taskbar(window: NativeWindow) {
        set_ex_style(window, WS_EX_APPWINDOW, false);
        set_ex_style(window, WS_EX_TOOLWINDOW, true);
        // The taskbar only notices the change when the window is shown again.
        // SAFETY: a live window of this process.
        unsafe {
            ShowWindow(hwnd(window), SW_HIDE);
            ShowWindow(hwnd(window), SW_SHOWNOACTIVATE);
        }
    }

    pub fn origin(window: NativeWindow) -> Option<(i32, i32)> {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: a live window of this process; `rect` is written by the call.
        (unsafe { GetWindowRect(hwnd(window), &mut rect) } != 0).then_some((rect.left, rect.top))
    }

    pub fn move_to(window: NativeWindow, (x, y): (i32, i32)) {
        // SAFETY: a live window of this process; only its position changes.
        unsafe {
            SetWindowPos(
                hwnd(window),
                std::ptr::null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// Keeps `window` above other windows (`on`), or lets it go behind them again. Returns false
/// when the platform refused or cannot do it.
pub fn set_topmost(window: NativeWindow, on: bool) -> bool {
    #[cfg(windows)]
    return win::set_topmost(window, on);
    #[cfg(not(windows))]
    {
        let _ = (window, on);
        false
    }
}

/// Lets every click through `window` to whatever is behind it (`on`), or takes clicks again.
pub fn set_click_through(window: NativeWindow, on: bool) {
    #[cfg(windows)]
    win::set_click_through(window, on);
    #[cfg(not(windows))]
    let _ = (window, on);
}

/// Keeps `window` off the taskbar and Alt+Tab (a helper window such as the title strip).
pub fn hide_from_taskbar(window: NativeWindow) {
    #[cfg(windows)]
    win::hide_from_taskbar(window);
    #[cfg(not(windows))]
    let _ = window;
}

/// The window's top-left corner in physical screen pixels.
pub fn origin(window: NativeWindow) -> Option<(i32, i32)> {
    #[cfg(windows)]
    return win::origin(window);
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

/// Moves the window's top-left corner to `at` (physical screen pixels).
pub fn move_to(window: NativeWindow, at: (i32, i32)) {
    #[cfg(windows)]
    win::move_to(window, at);
    #[cfg(not(windows))]
    let _ = (window, at);
}

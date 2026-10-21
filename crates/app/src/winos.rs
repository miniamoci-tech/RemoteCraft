//! The one place `unsafe` is allowed: thin Win32 calls to bring the host
//! window back from the system tray.
//!
//! eframe only runs the UI (and applies viewport commands) while the window
//! receives paint messages, which a hidden window does not. So once the window
//! is hidden to the tray, eframe can no longer show it again by itself. The
//! tray's event thread calls `restore_window` here, which asks Windows to show
//! and foreground the window directly, outside eframe's paint loop.
//!
//! Everything is a direct, documented call on our own window handle; no memory
//! is dereferenced. On non-Windows targets these are no-ops.
#![allow(unsafe_code)]

/// Show, un-minimize and foreground the window with the given HWND value.
/// A zero handle (not yet known) is ignored.
#[cfg(windows)]
pub fn restore_window(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };

    if hwnd == 0 {
        return;
    }
    let handle = hwnd as *mut core::ffi::c_void;
    // SAFETY: `handle` is our own eframe window's HWND, captured while the
    // window existed; these calls only read it as an opaque window handle and
    // ask the window manager to show/restore/foreground that window. A stale
    // handle makes them no-ops (they return an error we ignore), never UB.
    unsafe {
        ShowWindow(handle, SW_SHOW);
        ShowWindow(handle, SW_RESTORE);
        SetForegroundWindow(handle);
    }
}

#[cfg(not(windows))]
pub fn restore_window(_hwnd: isize) {}

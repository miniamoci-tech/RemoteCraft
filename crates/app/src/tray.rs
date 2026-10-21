//! System-tray support for the host window, so it can sit in the notification
//! area instead of the taskbar while it keeps sharing.
//!
//! Only Windows has a real implementation; elsewhere the window behaves
//! normally and every method is a no-op. A background thread drains the
//! tray's global event channel and wakes the egui event loop with
//! `request_repaint`, so a click on the tray menu is handled even while the
//! window is hidden (a hidden window stops repainting on its own).

/// What the user asked for via the tray.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    None,
    Show,
    Quit,
}

#[cfg(windows)]
pub use imp::Tray;

#[cfg(not(windows))]
pub use noop::Tray;

#[cfg(windows)]
mod imp {
    use std::sync::{Arc, Mutex};

    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    use super::TrayAction;

    pub struct Tray {
        // Kept alive for as long as the window runs; dropping it removes the
        // icon from the notification area.
        _tray: TrayIcon,
        pending: Arc<Mutex<Option<TrayAction>>>,
    }

    impl Tray {
        /// Create the tray icon. Must be called on the main thread. Returns
        /// `None` if the icon could not be created (the window then just stays
        /// in the taskbar).
        pub fn new(ctx: egui::Context) -> Option<Self> {
            let menu = Menu::new();
            let show = MenuItem::new("Show RemoteCraft", true, None);
            let quit = MenuItem::new("Quit (stop sharing)", true, None);
            menu.append(&show).ok()?;
            menu.append(&quit).ok()?;
            let show_id = show.id().clone();
            let quit_id = quit.id().clone();

            let mut builder = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("RemoteCraft — sharing this screen");
            if let Some(icon) = make_icon() {
                builder = builder.with_icon(icon);
            }
            let tray = builder.build().ok()?;

            let pending: Arc<Mutex<Option<TrayAction>>> = Arc::new(Mutex::new(None));
            spawn_pump(ctx, show_id, quit_id, pending.clone());

            Some(Self {
                _tray: tray,
                pending,
            })
        }

        /// Take the latest pending tray action (consumes it).
        pub fn poll(&self) -> TrayAction {
            self.pending
                .lock()
                .ok()
                .and_then(|mut p| p.take())
                .unwrap_or(TrayAction::None)
        }
    }

    /// Drain the global menu-event channel on a background thread and wake the
    /// UI. A single consumer avoids racing the UI thread for the same events.
    fn spawn_pump(
        ctx: egui::Context,
        show_id: MenuId,
        quit_id: MenuId,
        pending: Arc<Mutex<Option<TrayAction>>>,
    ) {
        std::thread::spawn(move || {
            let rx = MenuEvent::receiver();
            while let Ok(event) = rx.recv() {
                let action = if event.id == quit_id {
                    TrayAction::Quit
                } else if event.id == show_id {
                    TrayAction::Show
                } else {
                    continue;
                };
                if let Ok(mut p) = pending.lock() {
                    *p = Some(action);
                }
                ctx.request_repaint();
            }
        });
    }

    /// A small solid-colour icon (16x16 RGBA). Returns `None` if rejected.
    fn make_icon() -> Option<Icon> {
        let mut rgba = Vec::with_capacity(16 * 16 * 4);
        for _ in 0..16 * 16 {
            rgba.extend_from_slice(&[0x2f, 0x6f, 0xed, 0xff]);
        }
        Icon::from_rgba(rgba, 16, 16).ok()
    }
}

#[cfg(not(windows))]
mod noop {
    use super::TrayAction;

    pub struct Tray;

    impl Tray {
        pub fn new(_ctx: egui::Context) -> Option<Self> {
            None
        }

        pub fn poll(&self) -> TrayAction {
            TrayAction::None
        }
    }
}

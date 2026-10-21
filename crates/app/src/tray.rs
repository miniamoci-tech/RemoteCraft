//! System-tray support for the host window, so it can sit in the notification
//! area instead of the taskbar while it keeps sharing.
//!
//! Only Windows has a real implementation; elsewhere the window behaves
//! normally and every method is a no-op.
//!
//! A background thread drains the tray's global menu-event channel. Because a
//! hidden window stops running the UI update loop on its own, the thread acts
//! on the egui `Context` directly — it sends the viewport commands to show the
//! window and wakes the loop with `request_repaint` — instead of relying on
//! the next frame to poll. "Quit" is signalled through a shared flag the
//! window reads so it lets the close through instead of hiding again.

#[cfg(windows)]
pub use imp::Tray;

#[cfg(not(windows))]
pub use noop::Tray;

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    pub struct Tray {
        // Kept alive for as long as the window runs; dropping it removes the
        // icon from the notification area.
        _tray: TrayIcon,
        quit: Arc<AtomicBool>,
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

            let quit_flag = Arc::new(AtomicBool::new(false));
            let quit_for_thread = quit_flag.clone();
            std::thread::spawn(move || {
                let rx = MenuEvent::receiver();
                while let Ok(event) = rx.recv() {
                    if event.id == show_id {
                        restore(&ctx);
                    } else if event.id == quit_id {
                        quit_for_thread.store(true, Ordering::SeqCst);
                        // Make sure it's visible, then ask it to close.
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.request_repaint();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            });

            Some(Self {
                _tray: tray,
                quit: quit_flag,
            })
        }

        /// True once the user picked "Quit" from the tray menu.
        pub fn quit_requested(&self) -> bool {
            self.quit.load(Ordering::SeqCst)
        }
    }

    /// Bring the window back on screen from the tray.
    fn restore(ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
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
    pub struct Tray;

    impl Tray {
        pub fn new(_ctx: egui::Context) -> Option<Self> {
            None
        }

        pub fn quit_requested(&self) -> bool {
            false
        }
    }
}

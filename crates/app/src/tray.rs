//! System-tray support for the host window, so it can sit in the notification
//! area instead of the taskbar while it keeps sharing.
//!
//! Only Windows has a real implementation; elsewhere the window behaves
//! normally and every method is a no-op.
//!
//! A background thread drains the tray's global menu-event channel and records
//! the request in shared flags, then wakes the UI with `request_repaint`
//! (which runs a frame even while the window is hidden). The actual viewport
//! commands — showing the window, closing it — are issued from the UI thread
//! in `update`, because egui only reliably applies viewport commands from the
//! thread that owns the event loop.

#[cfg(windows)]
pub use imp::Tray;

#[cfg(not(windows))]
pub use noop::Tray;

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::Arc;

    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    pub struct Tray {
        // Kept alive for as long as the window runs; dropping it removes the
        // icon from the notification area.
        _tray: TrayIcon,
        show: Arc<AtomicBool>,
        quit: Arc<AtomicBool>,
    }

    impl Tray {
        /// Create the tray icon. Must be called on the main thread. `hwnd`
        /// holds the window handle (filled in once the window exists); the
        /// event thread uses it to restore the window directly, since eframe
        /// cannot repaint — and therefore cannot show — a hidden window.
        /// Returns `None` if the icon could not be created.
        pub fn new(ctx: egui::Context, hwnd: Arc<AtomicIsize>) -> Option<Self> {
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

            let show_flag = Arc::new(AtomicBool::new(false));
            let quit_flag = Arc::new(AtomicBool::new(false));
            let show_for_thread = show_flag.clone();
            let quit_for_thread = quit_flag.clone();
            std::thread::spawn(move || {
                let rx = MenuEvent::receiver();
                while let Ok(event) = rx.recv() {
                    if event.id == show_id {
                        crate::diag::log("tray: show event -> restore_window");
                        show_for_thread.store(true, Ordering::SeqCst);
                    } else if event.id == quit_id {
                        crate::diag::log("tray: quit event received");
                        quit_for_thread.store(true, Ordering::SeqCst);
                    } else {
                        continue;
                    }
                    // Restore the window directly via the OS: eframe can't
                    // repaint a hidden window, so this is what makes the UI
                    // loop run again (and then re-sync eframe's own state).
                    crate::winos::restore_window(hwnd.load(Ordering::SeqCst));
                    ctx.request_repaint();
                }
            });

            Some(Self {
                _tray: tray,
                show: show_flag,
                quit: quit_flag,
            })
        }

        /// True once (then cleared) when the user picked "Show" from the menu.
        pub fn take_show(&self) -> bool {
            self.show.swap(false, Ordering::SeqCst)
        }

        /// True once the user picked "Quit" from the tray menu.
        pub fn quit_requested(&self) -> bool {
            self.quit.load(Ordering::SeqCst)
        }
    }

    /// A 32x32 RGBA icon: a little blue monitor on a transparent background.
    /// Returns `None` if the pixels are somehow rejected.
    fn make_icon() -> Option<Icon> {
        const S: i32 = 32;
        let blue = [0x2f, 0x6f, 0xed];
        let screen = [0xe8, 0xf1, 0xff];
        let mut px = vec![0u8; (S * S * 4) as usize];

        let mut put = |x: i32, y: i32, c: [u8; 3]| {
            if (0..S).contains(&x) && (0..S).contains(&y) {
                let i = ((y * S + x) * 4) as usize;
                px[i] = c[0];
                px[i + 1] = c[1];
                px[i + 2] = c[2];
                px[i + 3] = 0xff;
            }
        };

        // Monitor body: a rounded rectangle.
        let (x0, y0, x1, y1, r) = (4, 5, 27, 22, 4);
        for y in y0..=y1 {
            for x in x0..=x1 {
                if rounded_contains(x, y, x0, y0, x1, y1, r) {
                    put(x, y, blue);
                }
            }
        }
        // Screen inset.
        for y in y0 + 3..=y1 - 3 {
            for x in x0 + 3..=x1 - 3 {
                put(x, y, screen);
            }
        }
        // Neck and base of the stand.
        for y in y1 + 1..=y1 + 3 {
            for x in 14..=17 {
                put(x, y, blue);
            }
        }
        for x in 9..=22 {
            put(x, y1 + 4, blue);
            put(x, y1 + 5, blue);
        }

        Icon::from_rgba(px, S as u32, S as u32).ok()
    }

    /// Whether (x,y) is inside the rectangle with corners rounded by radius r.
    fn rounded_contains(x: i32, y: i32, x0: i32, y0: i32, x1: i32, y1: i32, r: i32) -> bool {
        // Corner centres, r pixels in from each corner.
        let cx = if x < x0 + r {
            x0 + r
        } else if x > x1 - r {
            x1 - r
        } else {
            x
        };
        let cy = if y < y0 + r {
            y0 + r
        } else if y > y1 - r {
            y1 - r
        } else {
            y
        };
        let (dx, dy) = (x - cx, y - cy);
        dx * dx + dy * dy <= r * r
    }
}

#[cfg(not(windows))]
mod noop {
    use std::sync::atomic::AtomicIsize;
    use std::sync::Arc;

    pub struct Tray;

    impl Tray {
        pub fn new(_ctx: egui::Context, _hwnd: Arc<AtomicIsize>) -> Option<Self> {
            None
        }

        pub fn take_show(&self) -> bool {
            false
        }

        pub fn quit_requested(&self) -> bool {
            false
        }
    }
}

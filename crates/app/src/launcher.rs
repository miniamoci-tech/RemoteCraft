//! Graphical launcher.
//!
//! A single window shown when `remotecraft` is started with no arguments (for
//! example from the Start-menu / Finder icon). It lets a non-technical person
//! pick a role — share this screen, or view another computer — and fill in the
//! few fields, instead of using the command line.
//!
//! One event loop for the whole process: `RootApp` starts on the launcher
//! screen and, when the user clicks Start, *switches* in place to the viewer
//! or host screen. It never calls `eframe::run_native` a second time — doing
//! so aborts on macOS (AppKit tears down the first event loop's observers).

use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use remotecraft_net::random_password;

use crate::host::{self, HostConfig, HostStatus, SharedHostStatus};
use crate::viewer::{self, ViewerApp};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    View,
    Host,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostAccess {
    /// A one-time random password shown on screen (someone is at the host).
    Attended,
    /// A fixed password set in advance (no one need be at the host).
    Unattended,
}

/// What the launcher produced; the root app acts on it by switching screen.
enum Choice {
    View {
        relay: String,
        id: String,
        password: String,
    },
    Host(HostConfig),
}

/// Entry point: run the single-window app (launcher, then the chosen role).
pub fn run() -> anyhow::Result<()> {
    let settings = Settings::load();
    let runtime = tokio::runtime::Runtime::new()?;
    let handle = runtime.handle().clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([540.0, 480.0])
            .with_min_inner_size([460.0, 420.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "RemoteCraft",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(RootApp {
                screen: Screen::Launcher,
                launcher: LauncherApp::new(settings),
                handle,
            }))
        }),
    );
    // Dropping the runtime stops any background host/viewer task.
    drop(runtime);
    result.map_err(|e| anyhow::anyhow!("gui error: {e}"))
}

/// Which screen the single window is currently showing.
enum Screen {
    Launcher,
    Viewer(ViewerApp),
    Host(HostWindow),
}

/// The one eframe app for the whole process. It owns the current screen and
/// the tokio runtime handle used to start the viewer/host background work.
struct RootApp {
    screen: Screen,
    launcher: LauncherApp,
    handle: tokio::runtime::Handle,
}

impl RootApp {
    /// Switch from the launcher to the chosen role, in the same window.
    fn transition(&mut self, ctx: &egui::Context, choice: Choice) {
        match choice {
            Choice::View {
                relay,
                id,
                password,
            } => {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1360.0, 820.0)));
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("RemoteCraft — {id}")));
                let app = viewer::start_viewer(&self.handle, ctx.clone(), relay, id, password);
                self.screen = Screen::Viewer(app);
            }
            Choice::Host(config) => {
                let win = HostWindow::start(&self.handle, ctx.clone(), config);
                self.screen = Screen::Host(win);
            }
        }
    }
}

/// Open the host status window directly (CLI `host-window`), without the
/// launcher. Useful for an unattended/background host: it shows the ID and a
/// live status, and can minimize to the system tray. One event loop.
pub fn run_host_window(config: HostConfig) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let handle = runtime.handle().clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 400.0])
            .with_min_inner_size([420.0, 340.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "RemoteCraft — condivisione schermo",
        options,
        Box::new(move |cc| {
            Ok(Box::new(HostWindow::start(
                &handle,
                cc.egui_ctx.clone(),
                config,
            )))
        }),
    );
    drop(runtime);
    result.map_err(|e| anyhow::anyhow!("gui error: {e}"))
}

impl eframe::App for RootApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        match &mut self.screen {
            Screen::Launcher => {
                let mut choice = None;
                egui::CentralPanel::default().show(ctx, |ui| {
                    choice = self.launcher.ui(ui);
                });
                if let Some(choice) = choice {
                    self.transition(ctx, choice);
                }
            }
            Screen::Viewer(app) => app.update(ctx, frame),
            Screen::Host(win) => win.update(ctx, frame),
        }
    }
}

struct LauncherApp {
    mode: Mode,
    access: HostAccess,
    relay: String,
    id: String,
    /// Viewer: the host's password. Host/unattended: the fixed password to set.
    password: String,
    audio: bool,
    error: Option<String>,
}

impl LauncherApp {
    fn new(settings: Settings) -> Self {
        Self {
            mode: Mode::View,
            access: HostAccess::Attended,
            relay: settings.relay,
            id: settings.last_id,
            password: String::new(),
            audio: true,
            error: None,
        }
    }

    /// Validate the fields and, if good, remember the relay/id for next time
    /// and return the chosen role.
    fn validate(&mut self) -> Option<Choice> {
        let relay = self.relay.trim().to_string();
        if relay.is_empty() {
            self.error =
                Some("Inserisci l'indirizzo del relay (es. 192.168.1.50:21118).".to_string());
            return None;
        }

        let choice = match self.mode {
            Mode::View => {
                let id = self.id.trim().to_string();
                let password = self.password.clone();
                if id.is_empty() {
                    self.error =
                        Some("Inserisci l'ID mostrato sul computer che vuoi vedere.".to_string());
                    return None;
                }
                if password.is_empty() {
                    self.error =
                        Some("Inserisci la password mostrata su quel computer.".to_string());
                    return None;
                }
                Choice::View {
                    relay: relay.clone(),
                    id,
                    password,
                }
            }
            Mode::Host => {
                let (password, attended, desired_id) = match self.access {
                    HostAccess::Attended => (random_password(8), true, None),
                    HostAccess::Unattended => {
                        if self.password.is_empty() {
                            self.error = Some(
                                "Imposta una password fissa per l'accesso non vigilato."
                                    .to_string(),
                            );
                            return None;
                        }
                        // A fixed ID is optional but recommended so the ID stays
                        // the same across restarts. Empty = let the relay pick.
                        let id = self.id.trim();
                        let desired = if id.is_empty() {
                            None
                        } else if id.len() == 9 && id.bytes().all(|b| b.is_ascii_digit()) {
                            Some(id.to_string())
                        } else {
                            self.error = Some(
                                "L'ID fisso deve avere esattamente 9 cifre (oppure lascialo vuoto)."
                                    .to_string(),
                            );
                            return None;
                        };
                        (self.password.clone(), false, desired)
                    }
                };
                Choice::Host(HostConfig {
                    relay: relay.clone(),
                    password,
                    attended,
                    desired_id,
                    quality: 60,
                    fps: 10,
                    audio: self.audio,
                })
            }
        };

        Settings {
            relay,
            last_id: self.id.trim().to_string(),
        }
        .save();

        Some(choice)
    }

    /// Draw the launcher UI into `ui`; returns the chosen role when Start is
    /// clicked with valid fields.
    fn ui(&mut self, ui: &mut egui::Ui) -> Option<Choice> {
        let mut chosen = None;
        {
            ui.add_space(8.0);
            ui.heading("RemoteCraft");
            ui.label("Collega due computer tramite il tuo server relay.");
            ui.add_space(12.0);

            ui.label("Cosa vuoi fare?");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.mode, Mode::View, "👁  Vedi un altro computer");
                ui.selectable_value(&mut self.mode, Mode::Host, "🖥  Condividi questo schermo");
            });
            ui.add_space(12.0);

            egui::Grid::new("fields")
                .num_columns(2)
                .spacing([10.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Indirizzo relay");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.relay)
                            .hint_text("host:porta, es. 192.168.1.50:21118")
                            .desired_width(300.0),
                    );
                    ui.end_row();

                    match self.mode {
                        Mode::View => {
                            ui.label("ID computer");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.id)
                                    .hint_text("l'ID di 9 cifre mostrato sull'altro computer")
                                    .desired_width(300.0),
                            );
                            ui.end_row();

                            ui.label("Password");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.password)
                                    .password(true)
                                    .hint_text("la password mostrata sull'altro computer")
                                    .desired_width(300.0),
                            );
                            ui.end_row();
                        }
                        Mode::Host => {
                            ui.label("Accesso");
                            ui.vertical(|ui| {
                                ui.radio_value(
                                    &mut self.access,
                                    HostAccess::Attended,
                                    "Vigilato — mostra una password monouso ogni volta",
                                );
                                ui.radio_value(
                                    &mut self.access,
                                    HostAccess::Unattended,
                                    "Non vigilato — usa una password fissa che imposto io",
                                );
                            });
                            ui.end_row();

                            if self.access == HostAccess::Unattended {
                                ui.label("Password fissa");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.password)
                                        .password(true)
                                        .hint_text("scegli una password robusta")
                                        .desired_width(300.0),
                                );
                                ui.end_row();

                                ui.label("ID fisso");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.id)
                                        .hint_text("9 cifre, tienila sempre uguale (opzionale)")
                                        .desired_width(300.0),
                                );
                                ui.end_row();
                            }

                            ui.label("Audio");
                            ui.checkbox(
                                &mut self.audio,
                                "Invia l'audio di questo computer a chi si collega",
                            );
                            ui.end_row();
                        }
                    }
                });

            ui.add_space(14.0);
            let start_label = match self.mode {
                Mode::View => "Connetti",
                Mode::Host => "Avvia condivisione",
            };
            if ui
                .add_sized([160.0, 32.0], egui::Button::new(start_label))
                .clicked()
            {
                chosen = self.validate();
            }

            if let Some(err) = &self.error {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }

            ui.add_space(12.0);
            ui.separator();
            ui.label(
                egui::RichText::new(
                    "Il relay è il piccolo server che fai girare su un PC Linux o Windows \
                     in ufficio. Se non conosci l'indirizzo, chiedilo a chi l'ha configurato.",
                )
                .small()
                .weak(),
            );
        }
        chosen
    }
}

struct HostWindow {
    status: SharedHostStatus,
    relay: String,
    tray: Option<crate::tray::Tray>,
    tray_ready: bool,
    /// The native window handle (HWND on Windows), shared with the tray thread
    /// so it can restore the window while it is hidden. 0 until first known.
    hwnd: Arc<AtomicIsize>,
    /// Set when the user really wants to exit (tray "Quit" or in-window button),
    /// so the close request is allowed through instead of hiding to the tray.
    quitting: bool,
}

impl HostWindow {
    /// Start the host on `handle`, reporting into shared state, and return the
    /// window ready to be driven by the root app's event loop.
    fn start(handle: &tokio::runtime::Handle, _ctx: egui::Context, config: HostConfig) -> Self {
        let status: SharedHostStatus = Arc::new(Mutex::new(HostStatus {
            state: "Avvio…".to_string(),
            ..Default::default()
        }));
        host::spawn_gui(handle, config.clone(), status.clone());
        HostWindow {
            status,
            relay: config.relay,
            tray: None,
            tray_ready: false,
            hwnd: Arc::new(AtomicIsize::new(0)),
            quitting: false,
        }
    }
}

/// Read the native window handle value (HWND on Windows) from eframe's frame.
fn window_handle_value(frame: &eframe::Frame) -> Option<isize> {
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

impl eframe::App for HostWindow {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Keep the shared window handle current (cheap; only changes once).
        if let Some(h) = window_handle_value(frame) {
            self.hwnd.store(h, Ordering::SeqCst);
        }
        // Create the tray icon once, on the first frame (main thread), after
        // the handle is known so the tray thread can restore the window.
        if !self.tray_ready {
            self.tray_ready = true;
            self.tray = crate::tray::Tray::new(ctx.clone(), self.hwnd.clone());
        }
        self.handle_tray(ctx);

        let snapshot = self.status.lock().ok().map(|s| s.clone());

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("Stai condividendo questo schermo");
            ui.add_space(8.0);

            let Some(st) = snapshot else {
                ui.label("internal lock error");
                return;
            };

            if let Some(fatal) = &st.fatal {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("Impossibile avviare: {fatal}"),
                );
                return;
            }

            match &st.id {
                Some(id) => {
                    ui.label("Comunica questi dati a chi si collegherà:");
                    ui.add_space(6.0);
                    egui::Grid::new("host_fields")
                        .num_columns(2)
                        .spacing([10.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Indirizzo relay");
                            selectable_mono(ui, &self.relay);
                            ui.end_row();

                            ui.label("ID");
                            selectable_mono(ui, id);
                            ui.end_row();

                            ui.label("Password");
                            if st.attended {
                                selectable_mono(ui, &st.password);
                            } else {
                                ui.label(
                                    egui::RichText::new("la password fissa che hai impostato").italics(),
                                );
                            }
                            ui.end_row();
                        });

                    if st.attended {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Questa password monouso è valida finché questa finestra resta aperta.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                }
                None => {
                    ui.label("Connessione al relay…");
                }
            }

            ui.add_space(14.0);
            ui.separator();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                // Relay LED: red on fatal error, green once registered with the
                // relay (ID assigned), amber while still connecting.
                let (color, tip) = if st.fatal.is_some() {
                    (egui::Color32::from_rgb(220, 70, 70), "Errore relay")
                } else if st.id.is_some() {
                    (
                        egui::Color32::from_rgb(60, 180, 75),
                        "Connesso al relay",
                    )
                } else {
                    (
                        egui::Color32::from_rgb(230, 170, 0),
                        "Connessione al relay…",
                    )
                };
                ui.label(egui::RichText::new("⏺").color(color).size(14.0))
                    .on_hover_text(tip);
                ui.strong("Stato:");
                ui.label(&st.state);
            });
            if let Some((w, h)) = st.screen {
                ui.label(
                    egui::RichText::new(format!("Schermo {w}×{h}, audio {}", on_off(st.audio)))
                        .small()
                        .weak(),
                );
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if self.tray.is_some() && ui.button("Riduci a icona").clicked() {
                    hide_to_tray(ctx);
                }
                if ui.button("Esci (ferma la condivisione)").clicked() {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.label(
                egui::RichText::new(if self.tray.is_some() {
                    "Chiudendo la finestra (o con \"Riduci a icona\") RemoteCraft \
                     resta attivo come icona vicino all'orologio — clicca l'icona e \
                     scegli \"Mostra RemoteCraft\" per riaprirla. Usa Esci per fermare."
                } else {
                    "Chiudi questa finestra per fermare la condivisione."
                })
                .small()
                .weak(),
            );
        });

        ctx.request_repaint_after(Duration::from_millis(300));
    }
}

impl HostWindow {
    /// Act on the tray while a tray icon exists. Showing the window from the
    /// tray is done by the tray's own thread (so it works while hidden); here
    /// we only need to honour a "Quit" request and turn the window's close
    /// button into "hide to tray".
    fn handle_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else {
            return;
        };
        // The tray thread already restored the window via the OS; when this
        // frame then runs, re-sync eframe's own visible flag and take focus.
        if tray.take_show() {
            crate::diag::log("ui: re-sync after show");
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if tray.quit_requested() {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Closing the window hides it to the tray instead of quitting, unless
        // the user explicitly chose to quit.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            hide_to_tray(ctx);
        }
    }
}

fn hide_to_tray(ctx: &egui::Context) {
    crate::diag::log("ui: hiding to tray (Visible(false))");
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
}

fn selectable_mono(ui: &mut egui::Ui, text: &str) {
    let mut owned = text.to_string();
    // A read-only, selectable field so the value is easy to copy.
    ui.add(egui::TextEdit::singleline(&mut owned).font(egui::TextStyle::Monospace));
}

fn on_off(v: bool) -> &'static str {
    if v {
        "on"
    } else {
        "off"
    }
}

/// Relay/ID remembered between runs so they need not be retyped.
#[derive(Default)]
struct Settings {
    relay: String,
    last_id: String,
}

impl Settings {
    fn path() -> Option<PathBuf> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("RemoteCraft").join("launcher.conf"))
    }

    fn load() -> Self {
        let mut s = Settings::default();
        let Some(path) = Self::path() else {
            return s;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return s;
        };
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                match key.trim() {
                    "relay" => s.relay = value.trim().to_string(),
                    "id" => s.last_id = value.trim().to_string(),
                    _ => {}
                }
            }
        }
        s
    }

    fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Newlines are stripped to keep the key=value format intact.
        let relay = self.relay.replace(['\r', '\n'], "");
        let id = self.last_id.replace(['\r', '\n'], "");
        let _ = std::fs::write(&path, format!("relay={relay}\nid={id}\n"));
    }
}

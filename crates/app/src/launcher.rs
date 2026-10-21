//! Graphical launcher.
//!
//! A small window shown when `remotecraft` is started with no arguments (for
//! example from the Start-menu / Finder icon). It lets a non-technical person
//! pick a role — share this screen, or view another computer — and fill in the
//! few fields, instead of using the command line.
//!
//! The launcher only *collects* a choice. `eframe` owns the main-thread event
//! loop, so once the user clicks Start the launcher window closes, its event
//! loop returns, and `run` then opens the real window for the chosen role (the
//! viewer window, or a small host status window). This avoids nesting one
//! `eframe` app inside another.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use remotecraft_net::random_password;

use crate::host::{self, HostConfig, HostStatus, SharedHostStatus};
use crate::viewer;

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

/// What the launcher produced; acted on after its window closes.
enum Choice {
    View {
        relay: String,
        id: String,
        password: String,
    },
    Host(HostConfig),
}

/// Entry point: show the launcher, then run the chosen role.
pub fn run() -> anyhow::Result<()> {
    let settings = Settings::load();
    let choice: Arc<Mutex<Option<Choice>>> = Arc::new(Mutex::new(None));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([540.0, 480.0])
            .with_min_inner_size([460.0, 420.0]),
        ..Default::default()
    };

    let choice_for_app = choice.clone();
    let result = eframe::run_native(
        "RemoteCraft",
        options,
        Box::new(move |_cc| Ok(Box::new(LauncherApp::new(settings, choice_for_app)))),
    );
    result.map_err(|e| anyhow::anyhow!("gui error: {e}"))?;

    // The launcher window has closed; act on what the user chose (if anything).
    let chosen = choice.lock().ok().and_then(|mut c| c.take());
    match chosen {
        Some(Choice::View {
            relay,
            id,
            password,
        }) => viewer::run_with(relay, id, password),
        Some(Choice::Host(config)) => host_window(config),
        None => Ok(()),
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
    choice: Arc<Mutex<Option<Choice>>>,
}

impl LauncherApp {
    fn new(settings: Settings, choice: Arc<Mutex<Option<Choice>>>) -> Self {
        Self {
            mode: Mode::View,
            access: HostAccess::Attended,
            relay: settings.relay,
            id: settings.last_id,
            password: String::new(),
            audio: true,
            error: None,
            choice,
        }
    }

    /// Validate the fields and, if good, record the choice, remember the
    /// relay/id for next time, and ask the window to close.
    fn start(&mut self, ctx: &egui::Context) {
        let relay = self.relay.trim().to_string();
        if relay.is_empty() {
            self.error = Some("Enter the relay address (e.g. 192.168.1.50:21118).".to_string());
            return;
        }

        let choice = match self.mode {
            Mode::View => {
                let id = self.id.trim().to_string();
                let password = self.password.clone();
                if id.is_empty() {
                    self.error =
                        Some("Enter the ID shown on the computer you want to view.".to_string());
                    return;
                }
                if password.is_empty() {
                    self.error = Some("Enter the password shown on that computer.".to_string());
                    return;
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
                            self.error =
                                Some("Set a fixed password for unattended access.".to_string());
                            return;
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
                                "The fixed ID must be exactly 9 digits (or left empty)."
                                    .to_string(),
                            );
                            return;
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

        if let Ok(mut c) = self.choice.lock() {
            *c = Some(choice);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("RemoteCraft");
            ui.label("Connect two computers through your relay server.");
            ui.add_space(12.0);

            ui.label("What do you want to do?");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.mode, Mode::View, "👁  View another computer");
                ui.selectable_value(&mut self.mode, Mode::Host, "🖥  Share this screen");
            });
            ui.add_space(12.0);

            egui::Grid::new("fields")
                .num_columns(2)
                .spacing([10.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Relay address");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.relay)
                            .hint_text("host:port, e.g. 192.168.1.50:21118")
                            .desired_width(300.0),
                    );
                    ui.end_row();

                    match self.mode {
                        Mode::View => {
                            ui.label("Computer ID");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.id)
                                    .hint_text("the 9-digit ID shown on the other computer")
                                    .desired_width(300.0),
                            );
                            ui.end_row();

                            ui.label("Password");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.password)
                                    .password(true)
                                    .hint_text("the password shown on the other computer")
                                    .desired_width(300.0),
                            );
                            ui.end_row();
                        }
                        Mode::Host => {
                            ui.label("Access");
                            ui.vertical(|ui| {
                                ui.radio_value(
                                    &mut self.access,
                                    HostAccess::Attended,
                                    "Attended — show a one-time password each time",
                                );
                                ui.radio_value(
                                    &mut self.access,
                                    HostAccess::Unattended,
                                    "Unattended — use a fixed password I set",
                                );
                            });
                            ui.end_row();

                            if self.access == HostAccess::Unattended {
                                ui.label("Fixed password");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.password)
                                        .password(true)
                                        .hint_text("choose a strong password")
                                        .desired_width(300.0),
                                );
                                ui.end_row();

                                ui.label("Fixed ID");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.id)
                                        .hint_text("9 digits, keep it the same (optional)")
                                        .desired_width(300.0),
                                );
                                ui.end_row();
                            }

                            ui.label("Audio");
                            ui.checkbox(
                                &mut self.audio,
                                "Send this computer's sound to the viewer",
                            );
                            ui.end_row();
                        }
                    }
                });

            ui.add_space(14.0);
            let start_label = match self.mode {
                Mode::View => "Connect",
                Mode::Host => "Start sharing",
            };
            if ui
                .add_sized([160.0, 32.0], egui::Button::new(start_label))
                .clicked()
            {
                self.start(ctx);
            }

            if let Some(err) = &self.error {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }

            ui.add_space(12.0);
            ui.separator();
            ui.label(
                egui::RichText::new(
                    "The relay is the small server you run on a Linux or Windows PC in the \
                     office. Ask whoever set it up for its address if you don't know it.",
                )
                .small()
                .weak(),
            );
        });
    }
}

/// Run a small status window for the host role, started from the launcher.
///
/// It shows the ID and (for attended access) the one-time password to read out
/// to the person connecting, plus a live state line. The host itself runs on a
/// background tokio runtime and reports into shared state.
fn host_window(config: HostConfig) -> anyhow::Result<()> {
    let status: SharedHostStatus = Arc::new(Mutex::new(HostStatus {
        state: "Starting…".to_string(),
        ..Default::default()
    }));
    let runtime = tokio::runtime::Runtime::new()?;
    host::spawn_gui(runtime.handle(), config.clone(), status.clone());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 380.0])
            .with_min_inner_size([420.0, 320.0]),
        ..Default::default()
    };

    let app = HostWindow {
        status,
        relay: config.relay,
    };
    let result = eframe::run_native(
        "RemoteCraft — sharing this screen",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    );
    // Dropping the runtime stops the background host when the window closes.
    drop(runtime);
    result.map_err(|e| anyhow::anyhow!("gui error: {e}"))
}

struct HostWindow {
    status: SharedHostStatus,
    relay: String,
}

impl eframe::App for HostWindow {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let snapshot = self.status.lock().ok().map(|s| s.clone());

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("Sharing this screen");
            ui.add_space(8.0);

            let Some(st) = snapshot else {
                ui.label("internal lock error");
                return;
            };

            if let Some(fatal) = &st.fatal {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("Could not start: {fatal}"),
                );
                return;
            }

            match &st.id {
                Some(id) => {
                    ui.label("Give these to the person who will connect:");
                    ui.add_space(6.0);
                    egui::Grid::new("host_fields")
                        .num_columns(2)
                        .spacing([10.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Relay address");
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
                                    egui::RichText::new("the fixed password you set").italics(),
                                );
                            }
                            ui.end_row();
                        });

                    if st.attended {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "This one-time password is valid while this window stays open.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                }
                None => {
                    ui.label("Connecting to the relay…");
                }
            }

            ui.add_space(14.0);
            ui.separator();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.strong("Status:");
                ui.label(&st.state);
            });
            if let Some((w, h)) = st.screen {
                ui.label(
                    egui::RichText::new(format!("Screen {w}×{h}, audio {}", on_off(st.audio)))
                        .small()
                        .weak(),
                );
            }
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new("Close this window to stop sharing.")
                    .small()
                    .weak(),
            );
        });

        ctx.request_repaint_after(Duration::from_millis(300));
    }
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

//! Viewer role: a window showing the remote host's screen, forwarding this
//! window's mouse and keyboard input, and browsing/downloading remote files.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use remotecraft_core::{
    decode_frame, to_bytes, AppMessage, DirEntry, InputEvent, KeyName, MouseButton,
};
use remotecraft_net::{RelayClient, SecureChannel};
use tokio::sync::mpsc;

use crate::flag;

/// A remote directory the viewer is browsing.
#[derive(Default, Clone)]
struct Listing {
    path: String,
    parent: Option<String>,
    entries: Vec<DirEntry>,
}

/// Progress of the current/last file download.
#[derive(Clone)]
struct Download {
    name: String,
    received: u64,
    total: u64,
    done: bool,
    saved_path: Option<String>,
    error: Option<String>,
}

#[derive(Default)]
struct ViewerState {
    frame: Option<(usize, usize, Vec<u8>)>,
    version: u64,
    status: String,
    listing: Option<Listing>,
    download: Option<Download>,
}

fn set_status(shared: &Arc<Mutex<ViewerState>>, ctx: &egui::Context, text: &str) {
    if let Ok(mut s) = shared.lock() {
        s.status = text.to_string();
    }
    ctx.request_repaint();
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let relay = flag(args, "--relay").ok_or_else(|| anyhow::anyhow!("--relay <host:port> is required"))?;
    let target = flag(args, "--id").ok_or_else(|| anyhow::anyhow!("--id <id> is required"))?;
    let password = flag(args, "--password").ok_or_else(|| anyhow::anyhow!("--password <pw> is required"))?;

    let shared = Arc::new(Mutex::new(ViewerState {
        status: "Starting ...".to_string(),
        ..Default::default()
    }));

    // Outbound application messages: GUI/network -> host.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<AppMessage>();

    let runtime = tokio::runtime::Runtime::new()?;
    let handle = runtime.handle().clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1360.0, 820.0]),
        ..Default::default()
    };

    let title = format!("RemoteCraft — {target}");
    let shared_for_app = shared.clone();
    let out_rx = std::sync::Mutex::new(Some(out_rx));
    let out_tx_for_app = out_tx.clone();

    let result = eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let shared_net = shared.clone();
            let relay = relay.clone();
            let target = target.clone();
            let password = password.clone();
            let out_tx_net = out_tx.clone();
            let rx = out_rx.lock().ok().and_then(|mut g| g.take());
            if let Some(rx) = rx {
                handle.spawn(async move {
                    network(ctx, shared_net, relay, target, password, out_tx_net, rx).await;
                });
            }
            Ok(Box::new(ViewerApp {
                shared: shared_for_app,
                texture: None,
                shown_version: u64::MAX,
                out_tx: out_tx_for_app.clone(),
                show_files: false,
                fullscreen: false,
            }))
        }),
    );

    drop(runtime);
    result.map_err(|e| anyhow::anyhow!("gui error: {e}"))
}

struct ViewerApp {
    shared: Arc<Mutex<ViewerState>>,
    texture: Option<egui::TextureHandle>,
    shown_version: u64,
    out_tx: mpsc::UnboundedSender<AppMessage>,
    /// File panel starts hidden so the remote screen uses the whole window.
    show_files: bool,
    fullscreen: bool,
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (status, new_frame, listing, download) = match self.shared.lock() {
            Ok(s) => {
                let nf = if s.version != self.shown_version {
                    self.shown_version = s.version;
                    s.frame.clone()
                } else {
                    None
                };
                (s.status.clone(), nf, s.listing.clone(), s.download.clone())
            }
            Err(_) => ("internal lock error".to_string(), None, None, None),
        };

        if let Some((w, h, rgba)) = new_frame {
            if rgba.len() == w * h * 4 {
                let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                match &mut self.texture {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.texture =
                            Some(ctx.load_texture("remote-screen", image, egui::TextureOptions::LINEAR));
                    }
                }
            }
        }

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.toggle_value(&mut self.show_files, "📁 Files");
                if ui.toggle_value(&mut self.fullscreen, "⛶ Fullscreen").changed() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                }
                ui.separator();
                ui.label(&status);
            });
        });

        if self.show_files {
            egui::SidePanel::left("files")
                .default_width(320.0)
                .show(ctx, |ui| self.file_panel(ui, listing.as_ref(), download.as_ref()));
        }

        let mut image_rect = None;
        egui::CentralPanel::default().show(ctx, |ui| match &self.texture {
            Some(tex) => {
                let available = ui.available_size();
                let img = tex.size_vec2();
                let scale = (available.x / img.x).min(available.y / img.y).min(4.0);
                let size = img * scale.max(0.01);
                let resp = ui.add(egui::Image::new((tex.id(), size)));
                image_rect = Some(resp.rect);
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label("Waiting for the first frame ...");
                });
            }
        });

        if let Some(rect) = image_rect {
            self.forward_input(ctx, rect);
        }

        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

impl ViewerApp {
    fn file_panel(&self, ui: &mut egui::Ui, listing: Option<&Listing>, download: Option<&Download>) {
        ui.heading("Remote files");
        ui.separator();

        match listing {
            Some(l) => {
                ui.label(egui::RichText::new(&l.path).small().monospace());
                if let Some(parent) = &l.parent {
                    if ui.button("⬆  Up").clicked() {
                        self.request(AppMessage::ListDir { path: parent.clone() });
                    }
                }
                ui.separator();
                egui::ScrollArea::vertical().max_height(ui.available_height() - 120.0).show(ui, |ui| {
                    for entry in &l.entries {
                        ui.horizontal(|ui| {
                            if entry.is_dir {
                                if ui.button(format!("📁 {}", entry.name)).clicked() {
                                    self.request(AppMessage::ListDir { path: entry.path.clone() });
                                }
                            } else {
                                if ui.button("⤓").on_hover_text("Download").clicked() {
                                    self.request(AppMessage::GetFile { path: entry.path.clone() });
                                }
                                ui.label(format!("{}  ({})", entry.name, human_size(entry.size)));
                            }
                        });
                    }
                });
            }
            None => {
                ui.label("Connecting / loading file list ...");
            }
        }

        ui.separator();
        if let Some(d) = download {
            if let Some(err) = &d.error {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("Download failed: {err}"));
            } else if d.done {
                ui.label(format!("✅ {} downloaded", d.name));
                if let Some(p) = &d.saved_path {
                    ui.label(egui::RichText::new(p).small().monospace());
                }
            } else {
                let frac = if d.total > 0 { d.received as f32 / d.total as f32 } else { 0.0 };
                ui.label(format!(
                    "Downloading {} — {} / {}",
                    d.name,
                    human_size(d.received),
                    human_size(d.total)
                ));
                ui.add(egui::ProgressBar::new(frac).show_percentage());
            }
        }
    }

    fn request(&self, msg: AppMessage) {
        let _ = self.out_tx.send(msg);
    }

    fn forward_input(&self, ctx: &egui::Context, rect: egui::Rect) {
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            match event {
                egui::Event::PointerMoved(pos) => {
                    if let Some((x, y)) = normalize(pos, rect) {
                        self.send(InputEvent::MouseMove { x, y });
                    }
                }
                egui::Event::PointerButton { pos, button, pressed, .. } => {
                    if let Some((x, y)) = normalize(pos, rect) {
                        self.send(InputEvent::MouseMove { x, y });
                    }
                    if let Some(b) = map_button(button) {
                        self.send(InputEvent::MouseButton { button: b, pressed });
                    }
                }
                egui::Event::MouseWheel { delta, .. } => {
                    let dx = delta.x.clamp(-10.0, 10.0) as i32;
                    let dy = delta.y.clamp(-10.0, 10.0) as i32;
                    if dx != 0 || dy != 0 {
                        self.send(InputEvent::Scroll { dx, dy });
                    }
                }
                egui::Event::Text(text) => {
                    if !text.is_empty() {
                        self.send(InputEvent::Text(text));
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if let Some(name) = map_key(key) {
                        let is_char = matches!(name, KeyName::Char(_));
                        let shortcut = modifiers.ctrl || modifiers.alt || modifiers.mac_cmd;
                        if !is_char || shortcut {
                            self.send(InputEvent::KeyClick {
                                key: name,
                                ctrl: modifiers.ctrl,
                                alt: modifiers.alt,
                                shift: modifiers.shift,
                                meta: modifiers.mac_cmd,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn send(&self, event: InputEvent) {
        let _ = self.out_tx.send(AppMessage::Input(event));
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn normalize(pos: egui::Pos2, rect: egui::Rect) -> Option<(f32, f32)> {
    if rect.width() <= 0.0 || rect.height() <= 0.0 || !rect.contains(pos) {
        return None;
    }
    let x = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
    let y = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
    Some((x, y))
}

fn map_button(b: egui::PointerButton) -> Option<MouseButton> {
    match b {
        egui::PointerButton::Primary => Some(MouseButton::Left),
        egui::PointerButton::Secondary => Some(MouseButton::Right),
        egui::PointerButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

fn map_key(key: egui::Key) -> Option<KeyName> {
    use egui::Key;
    let named = match key {
        Key::Enter => KeyName::Enter,
        Key::Backspace => KeyName::Backspace,
        Key::Tab => KeyName::Tab,
        Key::Escape => KeyName::Escape,
        Key::Delete => KeyName::Delete,
        Key::Space => KeyName::Space,
        Key::ArrowUp => KeyName::ArrowUp,
        Key::ArrowDown => KeyName::ArrowDown,
        Key::ArrowLeft => KeyName::ArrowLeft,
        Key::ArrowRight => KeyName::ArrowRight,
        Key::Home => KeyName::Home,
        Key::End => KeyName::End,
        Key::PageUp => KeyName::PageUp,
        Key::PageDown => KeyName::PageDown,
        Key::F1 => KeyName::Function(1),
        Key::F2 => KeyName::Function(2),
        Key::F3 => KeyName::Function(3),
        Key::F4 => KeyName::Function(4),
        Key::F5 => KeyName::Function(5),
        Key::F6 => KeyName::Function(6),
        Key::F7 => KeyName::Function(7),
        Key::F8 => KeyName::Function(8),
        Key::F9 => KeyName::Function(9),
        Key::F10 => KeyName::Function(10),
        Key::F11 => KeyName::Function(11),
        Key::F12 => KeyName::Function(12),
        other => return letter_or_digit(other).map(KeyName::Char),
    };
    Some(named)
}

fn letter_or_digit(key: egui::Key) -> Option<char> {
    use egui::Key;
    let c = match key {
        Key::A => 'a', Key::B => 'b', Key::C => 'c', Key::D => 'd', Key::E => 'e',
        Key::F => 'f', Key::G => 'g', Key::H => 'h', Key::I => 'i', Key::J => 'j',
        Key::K => 'k', Key::L => 'l', Key::M => 'm', Key::N => 'n', Key::O => 'o',
        Key::P => 'p', Key::Q => 'q', Key::R => 'r', Key::S => 's', Key::T => 't',
        Key::U => 'u', Key::V => 'v', Key::W => 'w', Key::X => 'x', Key::Y => 'y',
        Key::Z => 'z',
        Key::Num0 => '0', Key::Num1 => '1', Key::Num2 => '2', Key::Num3 => '3',
        Key::Num4 => '4', Key::Num5 => '5', Key::Num6 => '6', Key::Num7 => '7',
        Key::Num8 => '8', Key::Num9 => '9',
        _ => return None,
    };
    Some(c)
}

/// Directory where downloads are saved on the viewer's machine.
fn download_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    home.join("RemoteCraft-Downloads")
}

async fn network(
    ctx: egui::Context,
    shared: Arc<Mutex<ViewerState>>,
    relay: String,
    target: String,
    password: String,
    out_tx: mpsc::UnboundedSender<AppMessage>,
    mut out_rx: mpsc::UnboundedReceiver<AppMessage>,
) {
    set_status(&shared, &ctx, "Connecting to relay ...");
    let client = match RelayClient::connect(&relay, None).await {
        Ok(c) => c,
        Err(e) => {
            set_status(&shared, &ctx, &format!("Relay connection failed: {e}"));
            return;
        }
    };

    set_status(&shared, &ctx, &format!("Connecting to host {target} ..."));
    if let Err(e) = client.connect_to(&target).await {
        set_status(&shared, &ctx, &format!("Could not reach host: {e}"));
        return;
    }

    set_status(&shared, &ctx, "Authenticating ...");
    let sc = match SecureChannel::handshake(&client, true, &password).await {
        Ok(sc) => sc,
        Err(e) => {
            set_status(&shared, &ctx, &format!("Authentication failed (wrong password?): {e}"));
            return;
        }
    };

    let client = Arc::new(client);

    // Outbound task: drain GUI/network messages and send them encrypted.
    let send_client = client.clone();
    let send_sc = sc.clone();
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            match to_bytes(&msg) {
                Ok(bytes) => {
                    if send_sc.send_message(&send_client, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Ask for the host's home directory listing to start.
    let _ = out_tx.send(AppMessage::ListDir { path: String::new() });

    set_status(&shared, &ctx, "Connected. Receiving screen ...");
    let mut frames = 0u64;
    let mut download: Option<std::fs::File> = None;

    loop {
        match sc.recv_message(&client).await {
            Ok(bytes) => match decode_frame::<AppMessage>(&bytes) {
                Ok(AppMessage::Frame { jpeg, .. }) => {
                    if let Ok(img) = image::load_from_memory(&jpeg) {
                        let rgba = img.to_rgba8();
                        let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                        if let Ok(mut s) = shared.lock() {
                            s.frame = Some((w, h, rgba.into_raw()));
                            s.version = s.version.wrapping_add(1);
                        }
                        frames += 1;
                        if frames == 1 || frames.is_multiple_of(50) {
                            tracing::info!(frames, "decoded remote frame");
                        }
                        ctx.request_repaint();
                    }
                }
                Ok(AppMessage::HostInfo { screen_width, screen_height }) => {
                    set_status(
                        &shared,
                        &ctx,
                        &format!("Connected. Remote screen {screen_width}x{screen_height}. Mouse, keyboard & files active."),
                    );
                }
                Ok(AppMessage::DirListing { path, parent, entries }) => {
                    if let Ok(mut s) = shared.lock() {
                        s.listing = Some(Listing { path, parent, entries });
                    }
                    ctx.request_repaint();
                }
                Ok(AppMessage::FileStart { name, size }) => {
                    download = begin_download(&shared, &ctx, &name, size);
                }
                Ok(AppMessage::FileChunk { data }) => {
                    if let Some(file) = download.as_mut() {
                        if file.write_all(&data).is_err() {
                            mark_download_error(&shared, &ctx, "write to disk failed");
                            download = None;
                        } else if let Ok(mut s) = shared.lock() {
                            if let Some(d) = s.download.as_mut() {
                                d.received += data.len() as u64;
                            }
                        }
                        ctx.request_repaint();
                    }
                }
                Ok(AppMessage::FileEnd) => {
                    if let Some(mut file) = download.take() {
                        let _ = file.flush();
                        if let Ok(mut s) = shared.lock() {
                            if let Some(d) = s.download.as_mut() {
                                d.done = true;
                            }
                        }
                        ctx.request_repaint();
                    }
                }
                Ok(AppMessage::FileError { message }) => {
                    mark_download_error(&shared, &ctx, &message);
                    download = None;
                }
                Ok(_) => {}
                Err(_) => {}
            },
            Err(e) => {
                set_status(&shared, &ctx, &format!("Disconnected: {e}"));
                return;
            }
        }
    }
}

/// Open the local destination file and record the download in shared state.
fn begin_download(
    shared: &Arc<Mutex<ViewerState>>,
    ctx: &egui::Context,
    name: &str,
    size: u64,
) -> Option<std::fs::File> {
    let dir = download_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        mark_download_error(shared, ctx, &format!("cannot create download folder: {e}"));
        return None;
    }
    let dest = dir.join(name);
    match std::fs::File::create(&dest) {
        Ok(file) => {
            if let Ok(mut s) = shared.lock() {
                s.download = Some(Download {
                    name: name.to_string(),
                    received: 0,
                    total: size,
                    done: false,
                    saved_path: Some(dest.to_string_lossy().to_string()),
                    error: None,
                });
            }
            ctx.request_repaint();
            Some(file)
        }
        Err(e) => {
            mark_download_error(shared, ctx, &format!("cannot create {}: {e}", dest.display()));
            None
        }
    }
}

fn mark_download_error(shared: &Arc<Mutex<ViewerState>>, ctx: &egui::Context, message: &str) {
    if let Ok(mut s) = shared.lock() {
        match s.download.as_mut() {
            Some(d) => d.error = Some(message.to_string()),
            None => {
                s.download = Some(Download {
                    name: String::new(),
                    received: 0,
                    total: 0,
                    done: false,
                    saved_path: None,
                    error: Some(message.to_string()),
                });
            }
        }
    }
    ctx.request_repaint();
}

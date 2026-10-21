//! Viewer role: a window showing the remote host's screen, forwarding this
//! window's mouse and keyboard input back to the host.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use remotecraft_core::{decode_frame, to_bytes, AppMessage, InputEvent, KeyName, MouseButton};
use remotecraft_net::{RelayClient, SecureChannel};
use tokio::sync::mpsc;

use crate::flag;

/// Decoded RGBA frame ready for the GUI, plus connection status text.
#[derive(Default)]
struct ViewerState {
    /// Latest decoded frame: (width, height, rgba bytes).
    frame: Option<(usize, usize, Vec<u8>)>,
    /// Incremented each time a new frame lands, so the GUI knows to re-upload.
    version: u64,
    status: String,
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

    // Input events flow GUI thread -> network task -> host.
    let (input_tx, input_rx) = mpsc::unbounded_channel::<InputEvent>();

    let runtime = tokio::runtime::Runtime::new()?;
    let handle = runtime.handle().clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 760.0]),
        ..Default::default()
    };

    let title = format!("RemoteCraft — {target}");
    let shared_for_app = shared.clone();
    let input_rx = std::sync::Mutex::new(Some(input_rx));

    let result = eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let shared_net = shared.clone();
            let relay = relay.clone();
            let target = target.clone();
            let password = password.clone();
            let rx = input_rx.lock().ok().and_then(|mut g| g.take());
            if let Some(rx) = rx {
                handle.spawn(async move {
                    network(ctx, shared_net, relay, target, password, rx).await;
                });
            }
            Ok(Box::new(ViewerApp {
                shared: shared_for_app,
                texture: None,
                shown_version: u64::MAX,
                input_tx: input_tx.clone(),
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
    input_tx: mpsc::UnboundedSender<InputEvent>,
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (status, new_frame) = match self.shared.lock() {
            Ok(s) => {
                let nf = if s.version != self.shown_version {
                    self.shown_version = s.version;
                    s.frame.clone()
                } else {
                    None
                };
                (s.status.clone(), nf)
            }
            Err(_) => ("internal lock error".to_string(), None),
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
                ui.label("Status:");
                ui.label(&status);
            });
        });

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

        // Poll for new frames even without input events.
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

impl ViewerApp {
    /// Translate this window's input events into remote input, mapping pointer
    /// positions to normalized coordinates within the displayed image.
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
                        // Plain character keys arrive as Text events; only
                        // forward a Char key press when it's part of a shortcut.
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
        let _ = self.input_tx.send(event);
    }
}

/// Map a pointer position to normalized (0..1) coordinates inside `rect`, or
/// `None` if it is outside the displayed image.
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

/// Map a letter/digit key to its character, for shortcuts like Ctrl+C.
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

async fn network(
    ctx: egui::Context,
    shared: Arc<Mutex<ViewerState>>,
    relay: String,
    target: String,
    password: String,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
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

    // Input-sending task: drain the GUI's events and forward them encrypted.
    let send_client = client.clone();
    let send_sc = sc.clone();
    tokio::spawn(async move {
        while let Some(ev) = input_rx.recv().await {
            match to_bytes(&AppMessage::Input(ev)) {
                Ok(bytes) => {
                    if send_sc.send_message(&send_client, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    set_status(&shared, &ctx, "Connected. Receiving screen ...");
    let mut frames = 0u64;
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
                        if frames == 1 || frames.is_multiple_of(25) {
                            tracing::info!(frames, w, h, "decoded remote frame");
                        }
                        ctx.request_repaint();
                    }
                }
                Ok(AppMessage::HostInfo { screen_width, screen_height }) => {
                    set_status(
                        &shared,
                        &ctx,
                        &format!("Connected. Remote screen {screen_width}x{screen_height}. Mouse & keyboard active."),
                    );
                }
                Ok(AppMessage::ViewerReady) | Ok(AppMessage::Input(_)) => {}
                Err(_) => {}
            },
            Err(e) => {
                set_status(&shared, &ctx, &format!("Disconnected: {e}"));
                return;
            }
        }
    }
}

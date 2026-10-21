//! Viewer role: open a window showing the remote host's screen.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use remotecraft_core::{decode_frame, AppMessage};
use remotecraft_net::{RelayClient, SecureChannel};

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

    let runtime = tokio::runtime::Runtime::new()?;
    let handle = runtime.handle().clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 760.0]),
        ..Default::default()
    };

    let title = format!("RemoteCraft — {target}");
    let shared_for_app = shared.clone();

    let result = eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let shared_net = shared.clone();
            let relay = relay.clone();
            let target = target.clone();
            let password = password.clone();
            handle.spawn(async move {
                network(ctx, shared_net, relay, target, password).await;
            });
            Ok(Box::new(ViewerApp {
                shared: shared_for_app,
                texture: None,
                shown_version: u64::MAX,
            }))
        }),
    );

    // Keep the runtime alive until the window closes.
    drop(runtime);
    result.map_err(|e| anyhow::anyhow!("gui error: {e}"))
}

struct ViewerApp {
    shared: Arc<Mutex<ViewerState>>,
    texture: Option<egui::TextureHandle>,
    shown_version: u64,
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

        egui::CentralPanel::default().show(ctx, |ui| {
            match &self.texture {
                Some(tex) => {
                    let available = ui.available_size();
                    let img = tex.size_vec2();
                    // Scale to fit while preserving aspect ratio.
                    let scale = (available.x / img.x).min(available.y / img.y).min(4.0);
                    let size = img * scale.max(0.01);
                    ui.centered_and_justified(|ui| {
                        ui.add(egui::Image::new((tex.id(), size)));
                    });
                }
                None => {
                    ui.centered_and_justified(|ui| {
                        ui.label("Waiting for the first frame ...");
                    });
                }
            }
        });

        // Poll for new frames even without input events.
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

async fn network(
    ctx: egui::Context,
    shared: Arc<Mutex<ViewerState>>,
    relay: String,
    target: String,
    password: String,
) {
    set_status(&shared, &ctx, "Connecting to relay ...");
    let mut client = match RelayClient::connect(&relay, None).await {
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
    let mut sc = match SecureChannel::handshake(&mut client, true, &password).await {
        Ok(sc) => sc,
        Err(e) => {
            set_status(&shared, &ctx, &format!("Authentication failed (wrong password?): {e}"));
            return;
        }
    };

    set_status(&shared, &ctx, "Connected. Receiving screen ...");
    let mut frames = 0u64;
    loop {
        match sc.recv_message(&mut client).await {
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
                        &format!("Connected. Remote screen {screen_width}x{screen_height}."),
                    );
                }
                Ok(AppMessage::ViewerReady) => {}
                Err(_) => {}
            },
            Err(e) => {
                set_status(&shared, &ctx, &format!("Disconnected: {e}"));
                return;
            }
        }
    }
}

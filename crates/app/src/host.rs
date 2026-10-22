//! Host role: capture this screen, stream it to an authenticated viewer, apply
//! the viewer's mouse/keyboard input, and serve file-transfer requests.
//!
//! All outbound messages (screen frames and file-transfer replies) go through
//! one sender task, so the chunks of two logical messages never interleave on
//! the wire. The (!Send) screen capturer and input injector each live on their
//! own dedicated thread.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use remotecraft_capture::{list_monitors, Capturer, Frame, MonitorGeom};
use remotecraft_core::{decode_frame, to_bytes, AppMessage, InputEvent, MonitorInfo};
use remotecraft_input::InputInjector;
use remotecraft_net::{random_password, RelayClient, SecureChannel};
use tokio::sync::mpsc;

use crate::flag;

enum CaptureMsg {
    /// Capture is bound to monitor `index` (sent on start and on each switch).
    Ready {
        index: u32,
        width: u32,
        height: u32,
    },
    Frame(Frame),
    Error(String),
}

/// Convert capture-layer monitor geometry into the protocol's monitor list.
fn to_monitor_infos(geoms: &[MonitorGeom]) -> Vec<MonitorInfo> {
    geoms
        .iter()
        .map(|g| MonitorInfo {
            index: g.index,
            width: g.width,
            height: g.height,
            is_primary: g.is_primary,
        })
        .collect()
}

/// How to start a host.
#[derive(Clone)]
pub struct HostConfig {
    pub relay: String,
    pub password: String,
    /// True when the password was auto-generated (attended, one-time).
    pub attended: bool,
    /// Ask the relay for this fixed 9-digit ID (unattended access, so the ID
    /// stays the same across restarts). `None` lets the relay assign one.
    pub desired_id: Option<String>,
    pub quality: u8,
    pub fps: u32,
    pub audio: bool,
}

/// Live host status, read by the GUI launcher.
#[derive(Default, Clone)]
pub struct HostStatus {
    pub id: Option<String>,
    pub password: String,
    pub attended: bool,
    pub audio: bool,
    pub screen: Option<(u32, u32)>,
    /// A short human-readable line describing the current state.
    pub state: String,
    /// Set on an unrecoverable error (capture unavailable, relay unreachable).
    pub fatal: Option<String>,
}

pub type SharedHostStatus = Arc<Mutex<HostStatus>>;

/// Where host progress is reported: the console (CLI) or shared state (GUI).
enum Reporter {
    Console,
    Shared(SharedHostStatus),
}

impl Reporter {
    fn ready(&self, id: &str, cfg: &HostConfig, screen: (u32, u32)) {
        match self {
            Reporter::Console => {
                println!("Screen: {}x{}", screen.0, screen.1);
                println!("\n  RemoteCraft host ready.");
                println!("  ID:        {id}");
                if cfg.attended {
                    println!(
                        "  Password:  {}   (one-time, attended access)",
                        cfg.password
                    );
                } else {
                    println!("  Password:  (fixed, unattended access)");
                }
                if cfg.audio {
                    println!("  Audio:     remote playback will be streamed to the viewer.");
                }
                println!("\nOn the viewer run:");
                println!(
                    "  remotecraft view --relay {} --id {id} --password <password>\n",
                    cfg.relay
                );
                println!("Waiting for a viewer (Ctrl+C to stop) ...");
            }
            Reporter::Shared(s) => {
                if let Ok(mut st) = s.lock() {
                    st.id = Some(id.to_string());
                    st.password = cfg.password.clone();
                    st.attended = cfg.attended;
                    st.audio = cfg.audio;
                    st.screen = Some(screen);
                    st.state = "In attesa che qualcuno si colleghi…".to_string();
                }
            }
        }
    }

    fn state(&self, msg: &str) {
        match self {
            Reporter::Console => println!("{msg}"),
            Reporter::Shared(s) => {
                if let Ok(mut st) = s.lock() {
                    st.state = msg.to_string();
                }
            }
        }
    }

    fn fatal(&self, msg: &str) {
        match self {
            Reporter::Console => eprintln!("error: {msg}"),
            Reporter::Shared(s) => {
                if let Ok(mut st) = s.lock() {
                    st.fatal = Some(msg.to_string());
                }
            }
        }
    }
}

/// Parse the host flags shared by `host` and `host-window` into a config.
pub fn parse_config(args: &[String]) -> anyhow::Result<HostConfig> {
    let relay =
        flag(args, "--relay").ok_or_else(|| anyhow::anyhow!("--relay <host:port> is required"))?;
    let (password, attended) = match flag(args, "--password") {
        Some(p) => (p, false),
        None => (random_password(8), true),
    };
    let quality: u8 = flag(args, "--quality")
        .and_then(|q| q.parse().ok())
        .unwrap_or(60);
    let fps: u32 = flag(args, "--fps")
        .and_then(|f| f.parse().ok())
        .unwrap_or(10)
        .clamp(1, 60);
    let audio = args.iter().any(|a| a == "--audio");
    let desired_id = flag(args, "--id");
    if let Some(id) = &desired_id {
        if id.len() != 9 || !id.bytes().all(|b| b.is_ascii_digit()) {
            anyhow::bail!("--id must be exactly 9 digits (e.g. 100200300)");
        }
    }

    Ok(HostConfig {
        relay,
        password,
        attended,
        desired_id,
        quality,
        fps,
        audio,
    })
}

/// CLI entry: `remotecraft host --relay ... [--password ...] [...]`.
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let config = parse_config(args)?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move { host_loop(config, Reporter::Console).await });
    Ok(())
}

/// GUI entry: start the host on `handle` and report into `status`.
pub fn spawn_gui(handle: &tokio::runtime::Handle, config: HostConfig, status: SharedHostStatus) {
    handle.spawn(async move { host_loop(config, Reporter::Shared(status)).await });
}

async fn host_loop(config: HostConfig, reporter: Reporter) {
    let active = Arc::new(AtomicBool::new(false));
    // The monitor the viewer is watching/controlling; changed via SelectMonitor.
    let selected = Arc::new(AtomicU32::new(0));
    let (frame_tx, mut frame_rx) = mpsc::channel::<CaptureMsg>(2);

    // Enumerate the host's monitors once; used for the viewer's picker and to
    // map input onto the right monitor. Empty/err falls back to a single one.
    let geoms = list_monitors().unwrap_or_default();

    let interval = Duration::from_millis(1000 / config.fps.max(1) as u64);
    let active_thread = active.clone();
    let selected_thread = selected.clone();
    let quality = config.quality;
    std::thread::spawn(move || {
        capture_thread(quality, interval, active_thread, selected_thread, frame_tx)
    });

    let (width, height) = match frame_rx.recv().await {
        Some(CaptureMsg::Ready { width, height, .. }) => (width, height),
        Some(CaptureMsg::Error(e)) => {
            reporter.fatal(&format!("cattura schermo non disponibile: {e}"));
            return;
        }
        _ => {
            reporter.fatal("la cattura si è interrotta prima di essere pronta");
            return;
        }
    };

    let client = match RelayClient::connect(&config.relay, config.desired_id.clone()).await {
        Ok(c) => Arc::new(c),
        Err(e) => {
            reporter.fatal(&format!("relay {} non raggiungibile: {e}", config.relay));
            return;
        }
    };
    let id = client.id().to_string();
    reporter.ready(&id, &config, (width, height));

    loop {
        let (_session, peer) = match client.wait_for_peer().await {
            Ok(v) => v,
            Err(e) => {
                reporter.state(&format!("In attesa di nuovo ({e})…"));
                continue;
            }
        };
        reporter.state(&format!("{peer} si sta collegando — verifica password…"));

        // Bound the handshake so a silent or incompatible peer can't leave the
        // host stuck on "checking password" forever (local timeout; no wire
        // change, so it stays compatible with older viewers).
        let handshake = SecureChannel::handshake(&client, false, &config.password);
        let sc = match tokio::time::timeout(Duration::from_secs(20), handshake).await {
            Ok(Ok(sc)) => sc,
            Ok(Err(e)) => {
                reporter.state(&format!(
                    "Qualcuno ha provato con la password errata ({e}). In attesa…"
                ));
                continue;
            }
            Err(_) => {
                reporter.state(
                    "Una connessione non si è completata (password errata o versione \
                     diversa?). In attesa che qualcuno si colleghi…",
                );
                continue;
            }
        };
        reporter.state("Connesso — condivido schermo, input e file.");

        // Each new viewer starts on the primary monitor.
        selected.store(0, Ordering::Relaxed);
        let reason = run_session(
            &client,
            &sc,
            &mut frame_rx,
            &active,
            &selected,
            &geoms,
            config.audio,
        )
        .await;
        active.store(false, Ordering::Relaxed);
        reporter.state(&format!(
            "Disconnesso ({reason}). In attesa che qualcuno si colleghi…"
        ));
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    client: &Arc<RelayClient>,
    sc: &SecureChannel,
    frame_rx: &mut mpsc::Receiver<CaptureMsg>,
    active: &Arc<AtomicBool>,
    selected: &Arc<AtomicU32>,
    geoms: &[MonitorGeom],
    audio: bool,
) -> String {
    // One outbound sender task serializes all messages onto the wire, so the
    // chunks of a frame and a file never interleave.
    let (out_tx, mut out_rx) = mpsc::channel::<AppMessage>(16);
    let send_sc = sc.clone();
    let send_client = client.clone();
    let sender = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let bytes = match to_bytes(&msg) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if send_sc.send_message(&send_client, &bytes).await.is_err() {
                break;
            }
        }
    });

    let monitors = to_monitor_infos(geoms);
    let active_geom = geoms.first().copied();
    let (width, height) = active_geom.map_or((1, 1), |g| (g.width, g.height));

    if out_tx
        .send(AppMessage::HostInfo {
            screen_width: width,
            screen_height: height,
            monitors: monitors.clone(),
            active: 0,
            host_version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .await
        .is_err()
    {
        sender.abort();
        return "viewer gone before first frame".to_string();
    }

    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(256);
    let geoms_for_input = geoms.to_vec();
    let selected_for_input = selected.clone();
    std::thread::spawn(move || injection_thread(geoms_for_input, selected_for_input, input_rx));

    if audio {
        start_audio(out_tx.clone());
    }

    active.store(true, Ordering::Relaxed);

    let reason = tokio::select! {
        r = produce_frames(frame_rx, &out_tx, &monitors) => r,
        r = handle_incoming(client, sc, &out_tx, &input_tx, selected, monitors.len()) => r,
    };
    sender.abort();
    reason
}

async fn produce_frames(
    frame_rx: &mut mpsc::Receiver<CaptureMsg>,
    out_tx: &mpsc::Sender<AppMessage>,
    monitors: &[MonitorInfo],
) -> String {
    loop {
        let msg = match frame_rx.recv().await {
            Some(m) => m,
            None => return "capture thread stopped".to_string(),
        };
        let frame = match msg {
            CaptureMsg::Frame(f) => f,
            CaptureMsg::Error(e) => return format!("capture error: {e}"),
            // A (re)bind: tell the viewer which monitor is now active and its
            // size, so it updates the picker highlight and the aspect ratio.
            CaptureMsg::Ready {
                index,
                width,
                height,
            } => {
                if out_tx
                    .send(AppMessage::HostInfo {
                        screen_width: width,
                        screen_height: height,
                        monitors: monitors.to_vec(),
                        active: index,
                        host_version: env!("CARGO_PKG_VERSION").to_string(),
                    })
                    .await
                    .is_err()
                {
                    return "sender stopped".to_string();
                }
                continue;
            }
        };
        if out_tx
            .send(AppMessage::Frame {
                width: frame.width,
                height: frame.height,
                jpeg: frame.jpeg,
            })
            .await
            .is_err()
        {
            return "sender stopped".to_string();
        }
    }
}

async fn handle_incoming(
    client: &Arc<RelayClient>,
    sc: &SecureChannel,
    out_tx: &mpsc::Sender<AppMessage>,
    input_tx: &mpsc::Sender<InputEvent>,
    selected: &Arc<AtomicU32>,
    monitor_count: usize,
) -> String {
    // Current incoming upload, if any (viewer -> host, M12). Writes land in the
    // host's uploads folder via the shared safe-name creator.
    let mut upload: Option<(std::fs::File, String)> = None;
    loop {
        let bytes = match sc.recv_message(client).await {
            Ok(b) => b,
            Err(e) => return format!("{e}"),
        };
        match decode_frame::<AppMessage>(&bytes) {
            Ok(AppMessage::Input(ev)) => {
                let _ = input_tx.try_send(ev);
            }
            Ok(AppMessage::SelectMonitor { index }) => {
                if (index as usize) < monitor_count {
                    selected.store(index, Ordering::Relaxed);
                }
            }
            Ok(AppMessage::ListDir { path }) => {
                if out_tx
                    .send(remotecraft_files::list_dir(&path))
                    .await
                    .is_err()
                {
                    return "sender stopped".to_string();
                }
            }
            Ok(AppMessage::GetFile { path }) => {
                if remotecraft_files::stream_file(&path, out_tx).await.is_err() {
                    return "sender stopped".to_string();
                }
            }
            Ok(AppMessage::UploadStart { name, size: _ }) => {
                upload = None;
                match remotecraft_files::create_download(&uploads_dir(), &name) {
                    Ok((dest, file)) => {
                        upload = Some((file, dest.to_string_lossy().to_string()));
                    }
                    Err(e) => {
                        let _ = out_tx
                            .send(AppMessage::UploadResult {
                                name,
                                ok: false,
                                message: format!("host refused the upload: {e}"),
                            })
                            .await;
                    }
                }
            }
            Ok(AppMessage::UploadChunk { data }) => {
                if let Some((file, path)) = upload.as_mut() {
                    use std::io::Write;
                    if let Err(e) = file.write_all(&data) {
                        let msg = format!("write failed: {e}");
                        let name = path.clone();
                        upload = None;
                        let _ = out_tx
                            .send(AppMessage::UploadResult {
                                name,
                                ok: false,
                                message: msg,
                            })
                            .await;
                    }
                }
            }
            Ok(AppMessage::UploadEnd) => {
                if let Some((mut file, path)) = upload.take() {
                    use std::io::Write;
                    let _ = file.flush();
                    if out_tx
                        .send(AppMessage::UploadResult {
                            name: path.clone(),
                            ok: true,
                            message: path,
                        })
                        .await
                        .is_err()
                    {
                        return "sender stopped".to_string();
                    }
                }
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

/// Folder where the host saves files uploaded by the viewer (M12).
fn uploads_dir() -> std::path::PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    home.join("RemoteCraft-Uploads")
}

/// Start loopback audio capture and forward PCM blocks to the outbound queue.
/// Capture runs on its own thread; a bridge thread moves batches onto the
/// async sender. If capture is unavailable, logs and continues without audio.
fn start_audio(out_tx: mpsc::Sender<AppMessage>) {
    let (atx, arx) = std::sync::mpsc::channel::<(remotecraft_audio::AudioFormat, Vec<i16>)>();
    if let Err(e) = remotecraft_audio::spawn_capture(atx) {
        println!("[audio] disabled: {e}");
        return;
    }
    std::thread::spawn(move || {
        while let Ok((fmt, pcm)) = arx.recv() {
            if pcm.is_empty() {
                continue;
            }
            let msg = AppMessage::Audio {
                sample_rate: fmt.sample_rate,
                channels: fmt.channels,
                pcm,
            };
            if out_tx.blocking_send(msg).is_err() {
                break; // session ended
            }
        }
    });
}

fn injection_thread(
    geoms: Vec<MonitorGeom>,
    selected: Arc<AtomicU32>,
    mut rx: mpsc::Receiver<InputEvent>,
) {
    // Start bound to the primary monitor's rectangle.
    let bounds = |idx: u32| geoms.iter().find(|g| g.index == idx).or(geoms.first());
    let first = bounds(selected.load(Ordering::Relaxed));
    let mut injector = match first {
        Some(g) => match InputInjector::with_bounds(g.x, g.y, g.width, g.height) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[input] disabled: {e}");
                return;
            }
        },
        None => match InputInjector::new(1, 1) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[input] disabled: {e}");
                return;
            }
        },
    };
    let mut current = selected.load(Ordering::Relaxed);
    while let Some(ev) = rx.blocking_recv() {
        // Re-aim at the active monitor if the viewer switched.
        let want = selected.load(Ordering::Relaxed);
        if want != current {
            if let Some(g) = bounds(want) {
                injector.set_bounds(g.x, g.y, g.width, g.height);
            }
            current = want;
        }
        if let Err(e) = injector.apply(&ev) {
            tracing::warn!(error = %e, "input injection failed");
        }
    }
}

fn capture_thread(
    quality: u8,
    interval: Duration,
    active: Arc<AtomicBool>,
    selected: Arc<AtomicU32>,
    tx: mpsc::Sender<CaptureMsg>,
) {
    let mut current = selected.load(Ordering::Relaxed);
    let mut capturer = match Capturer::for_index(current, quality) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.blocking_send(CaptureMsg::Error(e.to_string()));
            return;
        }
    };
    if tx
        .blocking_send(CaptureMsg::Ready {
            index: capturer.geom().index,
            width: capturer.width(),
            height: capturer.height(),
        })
        .is_err()
    {
        return;
    }

    loop {
        if !active.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(150));
            continue;
        }
        // Switch monitor if the viewer asked for a different one.
        let want = selected.load(Ordering::Relaxed);
        if want != current {
            match Capturer::for_index(want, quality) {
                Ok(c) => {
                    capturer = c;
                    current = want;
                    let ready = CaptureMsg::Ready {
                        index: capturer.geom().index,
                        width: capturer.width(),
                        height: capturer.height(),
                    };
                    if tx.blocking_send(ready).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not switch monitor");
                    current = want; // don't retry the same bad index every frame
                }
            }
        }
        let start = std::time::Instant::now();
        match capturer.capture() {
            Ok(frame) => match tx.try_send(CaptureMsg::Frame(frame)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            },
            Err(e) => {
                let _ = tx.blocking_send(CaptureMsg::Error(e.to_string()));
                return;
            }
        }
        let elapsed = start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

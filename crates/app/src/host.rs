//! Host role: capture this screen, stream it to an authenticated viewer, apply
//! the viewer's mouse/keyboard input, and serve file-transfer requests.
//!
//! All outbound messages (screen frames and file-transfer replies) go through
//! one sender task, so the chunks of two logical messages never interleave on
//! the wire. The (!Send) screen capturer and input injector each live on their
//! own dedicated thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use remotecraft_capture::{Capturer, Frame};
use remotecraft_core::{decode_frame, to_bytes, AppMessage, InputEvent};
use remotecraft_input::InputInjector;
use remotecraft_net::{random_password, RelayClient, SecureChannel};
use tokio::sync::mpsc;

use crate::flag;

enum CaptureMsg {
    Ready { width: u32, height: u32 },
    Frame(Frame),
    Error(String),
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
                    st.state = "Waiting for someone to connect…".to_string();
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

/// CLI entry: `remotecraft host --relay ... [--password ...] [...]`.
pub fn run(args: &[String]) -> anyhow::Result<()> {
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

    let config = HostConfig {
        relay,
        password,
        attended,
        desired_id,
        quality,
        fps,
        audio,
    };
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
    let (frame_tx, mut frame_rx) = mpsc::channel::<CaptureMsg>(2);

    let interval = Duration::from_millis(1000 / config.fps.max(1) as u64);
    let active_thread = active.clone();
    let quality = config.quality;
    std::thread::spawn(move || capture_thread(quality, interval, active_thread, frame_tx));

    let (width, height) = match frame_rx.recv().await {
        Some(CaptureMsg::Ready { width, height }) => (width, height),
        Some(CaptureMsg::Error(e)) => {
            reporter.fatal(&format!("screen capture unavailable: {e}"));
            return;
        }
        _ => {
            reporter.fatal("capture stopped before reporting readiness");
            return;
        }
    };

    let client = match RelayClient::connect(&config.relay, config.desired_id.clone()).await {
        Ok(c) => Arc::new(c),
        Err(e) => {
            reporter.fatal(&format!("cannot reach relay {}: {e}", config.relay));
            return;
        }
    };
    let id = client.id().to_string();
    reporter.ready(&id, &config, (width, height));

    loop {
        let (_session, peer) = match client.wait_for_peer().await {
            Ok(v) => v,
            Err(e) => {
                reporter.state(&format!("Waiting again ({e})…"));
                continue;
            }
        };
        reporter.state(&format!("{peer} is connecting — checking password…"));

        let sc = match SecureChannel::handshake(&client, false, &config.password).await {
            Ok(sc) => sc,
            Err(e) => {
                reporter.state(&format!(
                    "Someone tried with the wrong password ({e}). Waiting…"
                ));
                continue;
            }
        };
        reporter.state("Connected — sharing screen, input and files.");

        let reason = run_session(
            &client,
            &sc,
            &mut frame_rx,
            &active,
            width,
            height,
            config.audio,
        )
        .await;
        active.store(false, Ordering::Relaxed);
        reporter.state(&format!(
            "Disconnected ({reason}). Waiting for someone to connect…"
        ));
    }
}

async fn run_session(
    client: &Arc<RelayClient>,
    sc: &SecureChannel,
    frame_rx: &mut mpsc::Receiver<CaptureMsg>,
    active: &Arc<AtomicBool>,
    width: u32,
    height: u32,
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

    if out_tx
        .send(AppMessage::HostInfo {
            screen_width: width,
            screen_height: height,
        })
        .await
        .is_err()
    {
        sender.abort();
        return "viewer gone before first frame".to_string();
    }

    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(256);
    std::thread::spawn(move || injection_thread(width, height, input_rx));

    if audio {
        start_audio(out_tx.clone());
    }

    active.store(true, Ordering::Relaxed);

    let reason = tokio::select! {
        r = produce_frames(frame_rx, &out_tx) => r,
        r = handle_incoming(client, sc, &out_tx, &input_tx) => r,
    };
    sender.abort();
    reason
}

async fn produce_frames(
    frame_rx: &mut mpsc::Receiver<CaptureMsg>,
    out_tx: &mpsc::Sender<AppMessage>,
) -> String {
    loop {
        let msg = match frame_rx.recv().await {
            Some(m) => m,
            None => return "capture thread stopped".to_string(),
        };
        let frame = match msg {
            CaptureMsg::Frame(f) => f,
            CaptureMsg::Error(e) => return format!("capture error: {e}"),
            CaptureMsg::Ready { .. } => continue,
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
) -> String {
    loop {
        let bytes = match sc.recv_message(client).await {
            Ok(b) => b,
            Err(e) => return format!("{e}"),
        };
        match decode_frame::<AppMessage>(&bytes) {
            Ok(AppMessage::Input(ev)) => {
                let _ = input_tx.try_send(ev);
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
            Ok(_) => {}
            Err(_) => {}
        }
    }
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

fn injection_thread(width: u32, height: u32, mut rx: mpsc::Receiver<InputEvent>) {
    let mut injector = match InputInjector::new(width, height) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[input] disabled: {e}");
            return;
        }
    };
    while let Some(ev) = rx.blocking_recv() {
        if let Err(e) = injector.apply(&ev) {
            tracing::warn!(error = %e, "input injection failed");
        }
    }
}

fn capture_thread(
    quality: u8,
    interval: Duration,
    active: Arc<AtomicBool>,
    tx: mpsc::Sender<CaptureMsg>,
) {
    let capturer = match Capturer::primary(quality) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.blocking_send(CaptureMsg::Error(e.to_string()));
            return;
        }
    };
    if tx
        .blocking_send(CaptureMsg::Ready {
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

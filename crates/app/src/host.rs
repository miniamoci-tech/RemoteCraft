//! Host role: capture this screen, stream it to an authenticated viewer, and
//! apply the viewer's mouse/keyboard input.
//!
//! Three threads/tasks cooperate per session:
//! - a capture thread owns the (!Send) screen capturer and pushes JPEG frames;
//! - an injection thread owns the (!Send) input injector and applies events;
//! - async tasks move frames out and input in over one shared, cloneable
//!   [`SecureChannel`] / [`RelayClient`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use remotecraft_capture::{Capturer, Frame};
use remotecraft_core::{decode_frame, to_bytes, AppMessage, InputEvent};
use remotecraft_input::InputInjector;
use remotecraft_net::{random_password, RelayClient, SecureChannel};
use tokio::sync::mpsc;

use crate::flag;

/// Messages from the capture thread to the async side.
enum CaptureMsg {
    Ready { width: u32, height: u32 },
    Frame(Frame),
    Error(String),
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let relay = flag(args, "--relay").ok_or_else(|| anyhow::anyhow!("--relay <host:port> is required"))?;
    let (password, attended) = match flag(args, "--password") {
        Some(p) => (p, false),
        None => (random_password(8), true),
    };
    let quality: u8 = flag(args, "--quality").and_then(|q| q.parse().ok()).unwrap_or(60);
    let fps: u32 = flag(args, "--fps").and_then(|f| f.parse().ok()).unwrap_or(10).clamp(1, 60);

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move { host_loop(&relay, &password, attended, quality, fps).await })
}

async fn host_loop(
    relay: &str,
    password: &str,
    attended: bool,
    quality: u8,
    fps: u32,
) -> anyhow::Result<()> {
    let active = Arc::new(AtomicBool::new(false));
    let (frame_tx, mut frame_rx) = mpsc::channel::<CaptureMsg>(2);

    let interval = Duration::from_millis(1000 / fps as u64);
    let active_thread = active.clone();
    std::thread::spawn(move || capture_thread(quality, interval, active_thread, frame_tx));

    let (width, height) = match frame_rx.recv().await {
        Some(CaptureMsg::Ready { width, height }) => (width, height),
        Some(CaptureMsg::Error(e)) => anyhow::bail!("screen capture unavailable: {e}"),
        _ => anyhow::bail!("capture thread stopped before reporting readiness"),
    };
    println!("Screen: {width}x{height}");

    let client = Arc::new(RelayClient::connect(relay, None).await?);
    let id = client.id().to_string();

    println!("\n  RemoteCraft host ready.");
    println!("  ID:        {id}");
    if attended {
        println!("  Password:  {password}   (one-time, attended access)");
    } else {
        println!("  Password:  (fixed, unattended access)");
    }
    println!("\nOn the viewer run:");
    println!("  remotecraft view --relay {relay} --id {id} --password <password>\n");
    println!("Waiting for a viewer (Ctrl+C to stop) ...");

    loop {
        let (session, peer) = match client.wait_for_peer().await {
            Ok(v) => v,
            Err(e) => {
                println!("[-] {e}. Waiting again ...");
                continue;
            }
        };
        println!("[+] Viewer {peer} connected (session {session}). Authenticating ...");

        let sc = match SecureChannel::handshake(&client, false, password).await {
            Ok(sc) => sc,
            Err(e) => {
                println!("[x] Authentication failed: {e}. Waiting again ...");
                continue;
            }
        };
        println!("[+] Authenticated. Streaming at {fps} fps, quality {quality}; input enabled.");

        let reason = run_session(&client, &sc, &mut frame_rx, &active, width, height).await;
        active.store(false, Ordering::Relaxed);
        println!("[-] Session ended: {reason}. Waiting for a new viewer ...");
    }
}

/// Run one viewer session: send HostInfo, then stream frames and apply input
/// concurrently until either direction fails.
async fn run_session(
    client: &Arc<RelayClient>,
    sc: &SecureChannel,
    frame_rx: &mut mpsc::Receiver<CaptureMsg>,
    active: &Arc<AtomicBool>,
    width: u32,
    height: u32,
) -> String {
    let info = AppMessage::HostInfo {
        screen_width: width,
        screen_height: height,
    };
    match to_bytes(&info) {
        Ok(bytes) => {
            if let Err(e) = sc.send_message(client, &bytes).await {
                return format!("viewer gone before first frame: {e}");
            }
        }
        Err(e) => return format!("encode HostInfo: {e}"),
    }

    // Input injection thread.
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(256);
    std::thread::spawn(move || injection_thread(width, height, input_rx));

    active.store(true, Ordering::Relaxed);

    // Frames out and input in run concurrently; first to finish ends the session.
    tokio::select! {
        r = stream_frames(client, sc, frame_rx) => r,
        r = recv_input(client, sc, &input_tx) => r,
    }
}

async fn stream_frames(
    client: &Arc<RelayClient>,
    sc: &SecureChannel,
    frame_rx: &mut mpsc::Receiver<CaptureMsg>,
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
        let app = AppMessage::Frame {
            width: frame.width,
            height: frame.height,
            jpeg: frame.jpeg,
        };
        let bytes = match to_bytes(&app) {
            Ok(b) => b,
            Err(e) => return format!("encode frame: {e}"),
        };
        if let Err(e) = sc.send_message(client, &bytes).await {
            return format!("{e}");
        }
    }
}

async fn recv_input(
    client: &Arc<RelayClient>,
    sc: &SecureChannel,
    input_tx: &mpsc::Sender<InputEvent>,
) -> String {
    loop {
        let bytes = match sc.recv_message(client).await {
            Ok(b) => b,
            Err(e) => return format!("{e}"),
        };
        match decode_frame::<AppMessage>(&bytes) {
            Ok(AppMessage::Input(ev)) => {
                // Drop if the injector is backed up rather than stalling recv.
                let _ = input_tx.try_send(ev);
            }
            Ok(_) => {} // viewer shouldn't send other messages
            Err(_) => {}
        }
    }
}

/// Owns the (!Send) input injector; applies events until the channel closes.
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

/// Owns the (!Send) screen capturer; captures while `active`.
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

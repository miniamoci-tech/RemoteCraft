//! Host role: capture this screen and stream it to an authenticated viewer.
//!
//! `xcap::Monitor` is not `Send`, so the capturer is created and used only on
//! one dedicated OS thread. That thread pushes finished (Send) JPEG frames to
//! the async side over a bounded channel; when the viewer is slow, frames are
//! dropped rather than queued, so latency never grows without bound.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use remotecraft_capture::{Capturer, Frame};
use remotecraft_core::{to_bytes, AppMessage};
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
    // Only capture while a viewer is connected.
    let active = Arc::new(AtomicBool::new(false));
    let (frame_tx, mut frame_rx) = mpsc::channel::<CaptureMsg>(2);

    // Dedicated capture thread owns the (non-Send) capturer.
    let interval = Duration::from_millis(1000 / fps as u64);
    let active_thread = active.clone();
    std::thread::spawn(move || capture_thread(quality, interval, active_thread, frame_tx));

    // Wait for the capture thread's readiness (or failure).
    let (width, height) = match frame_rx.recv().await {
        Some(CaptureMsg::Ready { width, height }) => (width, height),
        Some(CaptureMsg::Error(e)) => anyhow::bail!("screen capture unavailable: {e}"),
        _ => anyhow::bail!("capture thread stopped before reporting readiness"),
    };
    println!("Screen: {width}x{height}");

    let mut client = RelayClient::connect(relay, None).await?;
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

        let mut sc = match SecureChannel::handshake(&mut client, false, password).await {
            Ok(sc) => sc,
            Err(e) => {
                println!("[x] Authentication failed: {e}. Waiting again ...");
                continue;
            }
        };
        println!("[+] Authenticated. Streaming at {fps} fps, quality {quality} ...");

        // Announce screen size, then stream frames until the viewer leaves.
        let info = AppMessage::HostInfo {
            screen_width: width,
            screen_height: height,
        };
        let ok = match to_bytes(&info) {
            Ok(bytes) => sc.send_message(&client, &bytes).await.is_ok(),
            Err(_) => false,
        };
        if !ok {
            println!("[-] viewer gone before first frame. Waiting again ...");
            continue;
        }

        active.store(true, Ordering::Relaxed);
        let reason = stream_session(&mut client, &mut sc, &mut frame_rx).await;
        active.store(false, Ordering::Relaxed);
        println!("[-] Session ended: {reason}. Waiting for a new viewer ...");
    }
}

/// Stream frames to the viewer until an error. Returns a human reason.
async fn stream_session(
    client: &mut RelayClient,
    sc: &mut SecureChannel,
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
            Err(e) => return format!("encode error: {e}"),
        };
        if let Err(e) = sc.send_message(client, &bytes).await {
            return format!("{e}");
        }
    }
}

/// Runs on its own thread; owns the capturer.
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
            Ok(frame) => {
                // Drop the frame if the channel is full (viewer is slow).
                match tx.try_send(CaptureMsg::Frame(frame)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
            Err(e) => {
                let _ = tx.blocking_send(CaptureMsg::Error(e.to_string()));
                return;
            }
        }
        // Keep the target frame interval.
        let elapsed = start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

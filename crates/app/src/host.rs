//! Host role: capture this screen, stream it to an authenticated viewer, apply
//! the viewer's mouse/keyboard input, and serve file-transfer requests.
//!
//! All outbound messages (screen frames and file-transfer replies) go through
//! one sender task, so the chunks of two logical messages never interleave on
//! the wire. The (!Send) screen capturer and input injector each live on their
//! own dedicated thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
        println!("[+] Authenticated. Screen + input + file transfer active.");

        let reason = run_session(&client, &sc, &mut frame_rx, &active, width, height).await;
        active.store(false, Ordering::Relaxed);
        println!("[-] Session ended: {reason}. Waiting for a new viewer ...");
    }
}

async fn run_session(
    client: &Arc<RelayClient>,
    sc: &SecureChannel,
    frame_rx: &mut mpsc::Receiver<CaptureMsg>,
    active: &Arc<AtomicBool>,
    width: u32,
    height: u32,
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
        .send(AppMessage::HostInfo { screen_width: width, screen_height: height })
        .await
        .is_err()
    {
        sender.abort();
        return "viewer gone before first frame".to_string();
    }

    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(256);
    std::thread::spawn(move || injection_thread(width, height, input_rx));

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
                if out_tx.send(remotecraft_files::list_dir(&path)).await.is_err() {
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

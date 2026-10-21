//! Full headless pipeline: capture real screen frames on a host task, send
//! them as encrypted `AppMessage::Frame`s through a relay, and decode them on
//! a viewer task — the same path the GUI viewer uses, minus the window.
//!
//! If no screen is available (e.g. headless CI), the test reports that and
//! passes rather than failing.

use remotecraft_capture::Capturer;
use remotecraft_core::{decode_frame, to_bytes, AppMessage};
use remotecraft_net::{RelayClient, SecureChannel};
use remotecraft_relay::Relay;
use tokio::net::TcpListener;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_encode_relay_decode_roundtrip() {
    // Skip gracefully where there is no display.
    if Capturer::primary(50).is_err() {
        eprintln!("no screen available; skipping capture pipeline test");
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(Relay::new().serve(listener));
    let relay = format!("127.0.0.1:{}", addr.port());

    let mut host = RelayClient::connect(&relay, None).await.expect("host connect");
    let host_id = host.id().to_string();

    // Host task: capture 3 frames and send them encrypted.
    let host_task = tokio::spawn(async move {
        host.wait_for_peer().await.expect("peer");
        let mut sc = SecureChannel::handshake(&mut host, false, "pw").await.expect("hs");
        for _ in 0..3 {
            // Build, use and drop the (!Send) capturer entirely before any
            // await, so the task future stays Send. The resulting bytes are.
            let bytes = {
                let capturer = Capturer::primary(50).expect("capturer");
                let frame = capturer.capture().expect("capture");
                let msg = AppMessage::Frame {
                    width: frame.width,
                    height: frame.height,
                    jpeg: frame.jpeg,
                };
                to_bytes(&msg).expect("encode")
            };
            sc.send_message(&host, &bytes).await.expect("send frame");
        }
    });

    // Viewer task: pair, authenticate, decode 3 frames.
    let mut viewer = RelayClient::connect(&relay, None).await.expect("viewer connect");
    viewer.connect_to(&host_id).await.expect("pair");
    let mut sc = SecureChannel::handshake(&mut viewer, true, "pw").await.expect("hs");

    let mut decoded = 0;
    for _ in 0..3 {
        let bytes = sc.recv_message(&mut viewer).await.expect("recv frame");
        match decode_frame::<AppMessage>(&bytes).expect("decode app msg") {
            AppMessage::Frame { width, height, jpeg } => {
                assert!(width > 0 && height > 0, "frame has real dimensions");
                let img = image::load_from_memory(&jpeg).expect("decode jpeg");
                assert_eq!(img.width(), width);
                assert_eq!(img.height(), height);
                decoded += 1;
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }
    assert_eq!(decoded, 3, "viewer decoded all three frames");

    host_task.await.expect("host task");
}

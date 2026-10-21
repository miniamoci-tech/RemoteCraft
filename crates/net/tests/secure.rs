//! End-to-end: two clients register with a real relay, pair, run the Noise
//! handshake with a password, and exchange encrypted payloads. A wrong
//! password must make the handshake fail.

use std::time::Duration;

use remotecraft_net::{RelayClient, SecureChannel};
use remotecraft_relay::Relay;
use tokio::net::TcpListener;

async fn start_relay() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(Relay::new().serve(listener));
    format!("127.0.0.1:{}", addr.port())
}

#[tokio::test]
async fn correct_password_establishes_encrypted_channel() {
    let relay = start_relay().await;

    // Host registers and waits.
    let host = RelayClient::connect(&relay, None).await.expect("host connect");
    let host_id = host.id().to_string();

    let host_task = tokio::spawn(async move {
        host.wait_for_peer().await.expect("peer");
        let sc = SecureChannel::handshake(&host, false, "correct horse")
            .await
            .expect("host handshake");
        // Receive one encrypted message, echo it back encrypted.
        let msg = sc.recv(&host).await.expect("recv");
        sc.send(&host, &msg).await.expect("send");
    });

    // Viewer connects.
    let viewer = RelayClient::connect(&relay, None).await.expect("viewer connect");
    viewer.connect_to(&host_id).await.expect("pair");
    let sc = SecureChannel::handshake(&viewer, true, "correct horse")
        .await
        .expect("viewer handshake");

    sc.send(&viewer, b"secret payload").await.expect("send");
    let echoed = sc.recv(&viewer).await.expect("recv echo");
    assert_eq!(echoed, b"secret payload");

    host_task.await.expect("host task");
}

#[tokio::test]
async fn large_message_is_chunked_and_reassembled() {
    let relay = start_relay().await;

    let host = RelayClient::connect(&relay, None).await.expect("host connect");
    let host_id = host.id().to_string();

    // A payload several times larger than one Noise message.
    let payload: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();

    let host_task = tokio::spawn(async move {
        host.wait_for_peer().await.expect("peer");
        let sc = SecureChannel::handshake(&host, false, "pw").await.expect("hs");
        let got = sc.recv_message(&host).await.expect("recv big");
        assert_eq!(got, expected, "reassembled message must match");
        // Echo back something small to release the viewer.
        sc.send_message(&host, b"ok").await.expect("ack");
    });

    let viewer = RelayClient::connect(&relay, None).await.expect("viewer connect");
    viewer.connect_to(&host_id).await.expect("pair");
    let sc = SecureChannel::handshake(&viewer, true, "pw").await.expect("hs");
    sc.send_message(&viewer, &payload).await.expect("send big");
    let ack = sc.recv_message(&viewer).await.expect("ack");
    assert_eq!(ack, b"ok");

    host_task.await.expect("host task");
}

#[tokio::test]
async fn wrong_password_fails_handshake() {
    let relay = start_relay().await;

    let host = RelayClient::connect(&relay, None).await.expect("host connect");
    let host_id = host.id().to_string();

    let host_task = tokio::spawn(async move {
        host.wait_for_peer().await.expect("peer");
        // Host uses the real password; viewer will use a different one.
        SecureChannel::handshake(&host, false, "real-password").await
    });

    let viewer = RelayClient::connect(&relay, None).await.expect("viewer connect");
    viewer.connect_to(&host_id).await.expect("pair");

    // Viewer uses the wrong password. Bound with a timeout so a hang fails.
    let viewer_result = tokio::time::timeout(
        Duration::from_secs(5),
        SecureChannel::handshake(&viewer, true, "wrong-password"),
    )
    .await;

    let host_result = tokio::time::timeout(Duration::from_secs(5), host_task)
        .await
        .expect("host task timed out")
        .expect("host task join");

    // The host must reject the mismatched handshake.
    assert!(host_result.is_err(), "host should reject wrong password");
    // The viewer either errors or times out; both mean no channel was formed.
    let viewer_ok = matches!(viewer_result, Ok(Ok(_)));
    assert!(!viewer_ok, "viewer must not establish a channel with wrong password");
}

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
    let host = RelayClient::connect(&relay, None)
        .await
        .expect("host connect");
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
    let viewer = RelayClient::connect(&relay, None)
        .await
        .expect("viewer connect");
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

    let host = RelayClient::connect(&relay, None)
        .await
        .expect("host connect");
    let host_id = host.id().to_string();

    // A payload several times larger than one Noise message.
    let payload: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();

    let host_task = tokio::spawn(async move {
        host.wait_for_peer().await.expect("peer");
        let sc = SecureChannel::handshake(&host, false, "pw")
            .await
            .expect("hs");
        let got = sc.recv_message(&host).await.expect("recv big");
        assert_eq!(got, expected, "reassembled message must match");
        // Echo back something small to release the viewer.
        sc.send_message(&host, b"ok").await.expect("ack");
    });

    let viewer = RelayClient::connect(&relay, None)
        .await
        .expect("viewer connect");
    viewer.connect_to(&host_id).await.expect("pair");
    let sc = SecureChannel::handshake(&viewer, true, "pw")
        .await
        .expect("hs");
    sc.send_message(&viewer, &payload).await.expect("send big");
    let ack = sc.recv_message(&viewer).await.expect("ack");
    assert_eq!(ack, b"ok");

    host_task.await.expect("host task");
}

#[tokio::test]
async fn wrong_password_fails_handshake() {
    let relay = start_relay().await;

    let host = RelayClient::connect(&relay, None)
        .await
        .expect("host connect");
    let host_id = host.id().to_string();

    let host_task = tokio::spawn(async move {
        host.wait_for_peer().await.expect("peer");
        // Host uses the real password; viewer will use a different one.
        SecureChannel::handshake(&host, false, "real-password").await
    });

    let viewer = RelayClient::connect(&relay, None)
        .await
        .expect("viewer connect");
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
    assert!(
        !viewer_ok,
        "viewer must not establish a channel with wrong password"
    );
}

async fn paired(relay: &str) -> (RelayClient, RelayClient, u64) {
    let host = RelayClient::connect(relay, None).await.expect("host");
    let viewer = RelayClient::connect(relay, None).await.expect("viewer");
    let session = viewer.connect_to(host.id()).await.expect("pair");
    assert_eq!(host.wait_for_peer().await.expect("incoming").0, session);
    (host, viewer, session)
}

#[tokio::test]
async fn encrypted_receive_wakes_when_viewer_disconnects() {
    let relay = start_relay().await;
    let (host, viewer, _) = paired(&relay).await;
    let (host_sc, viewer_sc) = tokio::join!(
        SecureChannel::handshake(&host, false, "pw"),
        SecureChannel::handshake(&viewer, true, "pw")
    );
    let host_sc = host_sc.expect("host handshake");
    viewer_sc.expect("viewer handshake");
    drop(viewer);
    let result = tokio::time::timeout(Duration::from_secs(2), host_sc.recv_message(&host))
        .await
        .expect("disconnect must wake receive");
    assert!(matches!(result, Err(remotecraft_net::NetError::PeerGone)));
}

#[tokio::test]
async fn failed_authentication_releases_host_and_discards_old_data() {
    let relay = start_relay().await;
    let (host, bad, _) = paired(&relay).await;
    bad.send(b"not a Noise handshake".to_vec())
        .await
        .expect("bad handshake");
    bad.send(b"leftover old session bytes".to_vec())
        .await
        .expect("leftover");
    assert!(SecureChannel::handshake(&host, false, "pw").await.is_err());
    assert!(tokio::time::timeout(Duration::from_secs(2), bad.recv())
        .await
        .expect("closed")
        .is_err());
    let viewer = RelayClient::connect(&relay, None)
        .await
        .expect("new viewer");
    viewer.connect_to(host.id()).await.expect("host released");
    tokio::time::timeout(Duration::from_secs(2), host.wait_for_peer())
        .await
        .expect("new session")
        .expect("incoming");
    let (host_sc, viewer_sc) = tokio::join!(
        SecureChannel::handshake(&host, false, "pw"),
        SecureChannel::handshake(&viewer, true, "pw")
    );
    let host_sc = host_sc.expect("new host handshake");
    let viewer_sc = viewer_sc.expect("new viewer handshake");
    viewer_sc
        .send_message(&viewer, b"new session")
        .await
        .expect("send");
    assert_eq!(
        host_sc.recv_message(&host).await.expect("recv"),
        b"new session"
    );
}

async fn silent_handshake_times_out(initiator: bool) {
    let relay = start_relay().await;
    let (host, viewer, _) = paired(&relay).await;
    let (client, idle) = if initiator {
        (&viewer, &host)
    } else {
        (&host, &viewer)
    };
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let result = SecureChannel::handshake(client, initiator, "pw").await;
    let elapsed = started.elapsed();
    tokio::time::resume();
    assert!(matches!(
        result,
        Err(remotecraft_net::NetError::HandshakeTimeout)
    ));
    assert!(elapsed >= remotecraft_net::HANDSHAKE_TIMEOUT);
    assert!(elapsed <= remotecraft_net::HANDSHAKE_TIMEOUT + Duration::from_millis(10));
    tokio::time::timeout(Duration::from_secs(2), async {
        while idle.recv().await.is_ok() {}
    })
    .await
    .expect("timeout must close session");
    let next = RelayClient::connect(&relay, None)
        .await
        .expect("next viewer");
    next.connect_to(host.id()).await.expect("host must be free");
    tokio::time::timeout(Duration::from_secs(2), host.wait_for_peer())
        .await
        .expect("incoming")
        .expect("new session");
    let (a, b) = tokio::join!(
        SecureChannel::handshake(&host, false, "pw"),
        SecureChannel::handshake(&next, true, "pw")
    );
    a.expect("host must authenticate again");
    b.expect("viewer must authenticate again");
}

#[tokio::test]
async fn silent_viewer_times_out_and_host_accepts_another_viewer() {
    silent_handshake_times_out(false).await;
}

#[tokio::test]
async fn silent_host_times_out_and_releases_session() {
    silent_handshake_times_out(true).await;
}

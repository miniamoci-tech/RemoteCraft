//! End-to-end test: two in-process peers register with a real relay bound to
//! an ephemeral port, pair, and relay opaque bytes both ways.

use remotecraft_core::{
    checked_frame_len, decode_frame, encode_frame, FromRelay, ToRelay, PROTOCOL_VERSION,
};
use remotecraft_relay::Relay;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn read_msg(stream: &mut TcpStream) -> FromRelay {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.expect("len");
    let len = checked_frame_len(u32::from_be_bytes(len_buf)).expect("len ok");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.expect("body");
    decode_frame(&body).expect("decode")
}

async fn send_msg(stream: &mut TcpStream, msg: &ToRelay) {
    let frame = encode_frame(msg).expect("encode");
    stream.write_all(&frame).await.expect("write");
}

async fn register(addr: std::net::SocketAddr) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    send_msg(
        &mut stream,
        &ToRelay::Register {
            protocol_version: PROTOCOL_VERSION,
            desired_id: None,
        },
    )
    .await;
    match read_msg(&mut stream).await {
        FromRelay::Registered { id } => (stream, id),
        other => panic!("expected Registered, got {other:?}"),
    }
}

#[tokio::test]
async fn two_peers_pair_and_relay_bytes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(Relay::new().serve(listener));

    // Host and viewer both register.
    let (mut host, host_id) = register(addr).await;
    let (mut viewer, _viewer_id) = register(addr).await;

    // Viewer connects to the host's ID.
    send_msg(&mut viewer, &ToRelay::Connect { target: host_id.clone() }).await;

    // Both sides should see SessionOpened with the same session number.
    let viewer_open = read_msg(&mut viewer).await;
    let host_open = read_msg(&mut host).await;
    let (vs, hs) = match (viewer_open, host_open) {
        (
            FromRelay::SessionOpened { session: vs, .. },
            FromRelay::SessionOpened { session: hs, .. },
        ) => (vs, hs),
        other => panic!("expected SessionOpened on both sides, got {other:?}"),
    };
    assert_eq!(vs, hs, "both peers share one session id");

    // Viewer -> host.
    send_msg(&mut viewer, &ToRelay::Data(b"hello host".to_vec())).await;
    match read_msg(&mut host).await {
        FromRelay::Data(bytes) => assert_eq!(bytes, b"hello host"),
        other => panic!("host expected Data, got {other:?}"),
    }

    // Host -> viewer.
    send_msg(&mut host, &ToRelay::Data(b"hello viewer".to_vec())).await;
    match read_msg(&mut viewer).await {
        FromRelay::Data(bytes) => assert_eq!(bytes, b"hello viewer"),
        other => panic!("viewer expected Data, got {other:?}"),
    }
}

#[tokio::test]
async fn connecting_to_unknown_id_is_refused() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(Relay::new().serve(listener));

    let (mut viewer, _) = register(addr).await;
    send_msg(&mut viewer, &ToRelay::Connect { target: "000000000".to_string() }).await;
    match read_msg(&mut viewer).await {
        FromRelay::SessionRefused { target, .. } => assert_eq!(target, "000000000"),
        other => panic!("expected SessionRefused, got {other:?}"),
    }
}

#[tokio::test]
async fn partner_is_told_when_peer_disconnects() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(Relay::new().serve(listener));

    let (mut host, host_id) = register(addr).await;
    let (mut viewer, _) = register(addr).await;
    send_msg(&mut viewer, &ToRelay::Connect { target: host_id }).await;
    let _ = read_msg(&mut viewer).await; // SessionOpened
    let _ = read_msg(&mut host).await; // SessionOpened

    // Viewer drops.
    drop(viewer);

    match read_msg(&mut host).await {
        FromRelay::SessionClosed { .. } => {}
        other => panic!("host expected SessionClosed, got {other:?}"),
    }
}

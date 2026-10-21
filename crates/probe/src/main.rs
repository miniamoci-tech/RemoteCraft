//! remotecraft-probe — a minimal connectivity tester for the relay.
//!
//! It is NOT the real app: it only proves that two machines can register with
//! a relay, be paired by numeric ID, and exchange bytes through it.
//!
//! On the machine to be reached ("office PC"):
//!     remotecraft-probe host --relay <RELAY_ADDR>
//! It prints an ID and waits.
//!
//! On the other machine ("home PC"):
//!     remotecraft-probe connect --relay <RELAY_ADDR> --id <THAT_ID>
//! It sends a test message; the host echoes it back and the round-trip is
//! printed on both sides.
//!
//! <RELAY_ADDR> is host:port, e.g. 192.168.1.50:21118 or relay.example.com:21118.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::time::{SystemTime, UNIX_EPOCH};

use remotecraft_core::{
    checked_frame_len, decode_frame, encode_frame, FromRelay, ToRelay, PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str);
    let relay = flag(&args, "--relay");
    let target = flag(&args, "--id");

    match mode {
        Some("host") => {
            let relay = relay.ok_or_else(|| anyhow::anyhow!("--relay <host:port> is required"))?;
            run_host(&relay).await
        }
        Some("connect") => {
            let relay = relay.ok_or_else(|| anyhow::anyhow!("--relay <host:port> is required"))?;
            let target = target.ok_or_else(|| anyhow::anyhow!("--id <target id> is required"))?;
            run_connect(&relay, &target).await
        }
        _ => {
            eprintln!(
                "usage:\n  remotecraft-probe host --relay <host:port>\n  remotecraft-probe connect --relay <host:port> --id <target-id>"
            );
            std::process::exit(2);
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

async fn register(relay: &str) -> anyhow::Result<(OwnedReadHalf, OwnedWriteHalf, String)> {
    println!("Connecting to relay {relay} ...");
    let stream = TcpStream::connect(relay).await?;
    let (mut reader, mut writer) = stream.into_split();
    write_msg(
        &mut writer,
        &ToRelay::Register {
            protocol_version: PROTOCOL_VERSION,
            desired_id: None,
        },
    )
    .await?;
    match read_msg(&mut reader).await? {
        FromRelay::Registered { id } => Ok((reader, writer, id)),
        FromRelay::Error { message } => anyhow::bail!("relay error: {message}"),
        other => anyhow::bail!("unexpected reply during register: {other:?}"),
    }
}

async fn run_host(relay: &str) -> anyhow::Result<()> {
    let (mut reader, mut writer, id) = register(relay).await?;
    println!("\n  Relay OK. This machine's ID is:  {id}\n");
    println!("On the other PC run:");
    println!("  remotecraft-probe connect --relay {relay} --id {id}\n");
    println!("Waiting for a connection (Ctrl+C to stop) ...");

    loop {
        match read_msg(&mut reader).await? {
            FromRelay::SessionOpened { session, peer } => {
                println!("[+] Peer {peer} connected (session {session}).");
            }
            FromRelay::Data(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                println!("[recv] {text}");
                // Echo it back through the relay.
                write_msg(&mut writer, &ToRelay::Data(bytes)).await?;
                println!("[echo] sent back to peer");
            }
            FromRelay::SessionClosed { session } => {
                println!("[-] Session {session} closed. Still waiting for a new connection ...");
            }
            FromRelay::Pong => {}
            other => println!("[?] {other:?}"),
        }
    }
}

async fn run_connect(relay: &str, target: &str) -> anyhow::Result<()> {
    let (mut reader, mut writer, id) = register(relay).await?;
    println!("Relay OK. My ID is {id}. Connecting to {target} ...");
    write_msg(&mut writer, &ToRelay::Connect { target: target.to_string() }).await?;

    loop {
        match read_msg(&mut reader).await? {
            FromRelay::SessionOpened { session, peer } => {
                println!("[+] Paired with {peer} (session {session}).");
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let msg = format!("ping from {id} at unix {now}");
                println!("[send] {msg}");
                write_msg(&mut writer, &ToRelay::Data(msg.into_bytes())).await?;
            }
            FromRelay::Data(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                println!("[recv echo] {text}");
                println!("\n  Round-trip through the relay succeeded. Relay works.\n");
                return Ok(());
            }
            FromRelay::SessionRefused { target, reason } => {
                anyhow::bail!("connection to {target} refused: {reason}");
            }
            FromRelay::SessionClosed { .. } => {
                anyhow::bail!("the host closed the session before replying");
            }
            FromRelay::Error { message } => anyhow::bail!("relay error: {message}"),
            FromRelay::Pong => {}
            FromRelay::Registered { .. } => {}
        }
    }
}

async fn write_msg(writer: &mut OwnedWriteHalf, msg: &ToRelay) -> anyhow::Result<()> {
    let frame = encode_frame(msg)?;
    writer.write_all(&frame).await?;
    Ok(())
}

async fn read_msg(reader: &mut OwnedReadHalf) -> anyhow::Result<FromRelay> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = checked_frame_len(u32::from_be_bytes(len_buf))?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(decode_frame(&body)?)
}

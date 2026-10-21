//! remotecraft-probe — connectivity + secure-channel tester for the relay.
//!
//! It is NOT the real app: it proves that two machines can register with a
//! relay, be paired by numeric ID, authenticate with a password, and exchange
//! end-to-end encrypted bytes through the relay.
//!
//! On the machine to be reached ("office PC"):
//!     remotecraft-probe host --relay <RELAY_ADDR> [--password <PW>]
//! Without --password it prints a fresh random one (attended access). With
//! --password it uses a fixed one (unattended access). It then prints its ID
//! and waits.
//!
//! On the other machine ("home PC"):
//!     remotecraft-probe connect --relay <RELAY_ADDR> --id <ID> --password <PW>
//! It authenticates and sends an encrypted test message; the host echoes it
//! back and both sides print the result.
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

use remotecraft_net::{random_password, RelayClient, SecureChannel};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str);
    let relay = flag(&args, "--relay");
    let target = flag(&args, "--id");
    let password = flag(&args, "--password");

    match mode {
        Some("host") => {
            let relay = relay.ok_or_else(|| anyhow::anyhow!("--relay <host:port> is required"))?;
            run_host(&relay, password).await
        }
        Some("connect") => {
            let relay = relay.ok_or_else(|| anyhow::anyhow!("--relay <host:port> is required"))?;
            let target = target.ok_or_else(|| anyhow::anyhow!("--id <target id> is required"))?;
            let password =
                password.ok_or_else(|| anyhow::anyhow!("--password <password> is required"))?;
            run_connect(&relay, &target, &password).await
        }
        _ => {
            eprintln!(
                "usage:\n  remotecraft-probe host --relay <host:port> [--password <pw>]\n  remotecraft-probe connect --relay <host:port> --id <id> --password <pw>"
            );
            std::process::exit(2);
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

async fn run_host(relay: &str, password: Option<String>) -> anyhow::Result<()> {
    let (password, attended) = match password {
        Some(p) => (p, false),
        None => (random_password(8), true),
    };

    println!("Connecting to relay {relay} ...");
    let client = RelayClient::connect(relay, None).await?;
    let id = client.id().to_string();

    println!("\n  Relay OK. This machine's ID is:  {id}");
    if attended {
        println!("  One-time password (attended access):  {password}");
    } else {
        println!("  Using the fixed password you supplied (unattended access).");
    }
    println!("\nOn the other PC run:");
    println!("  remotecraft-probe connect --relay {relay} --id {id} --password {password}\n");
    println!("Waiting for a connection (Ctrl+C to stop) ...");

    loop {
        let (session, peer) = match client.wait_for_peer().await {
            Ok(v) => v,
            Err(e) => {
                println!("[-] {e}. Waiting again ...");
                continue;
            }
        };
        println!("[+] Peer {peer} connected (session {session}). Authenticating ...");

        let sc = match SecureChannel::handshake(&client, false, &password).await {
            Ok(sc) => sc,
            Err(e) => {
                println!("[x] Authentication failed: {e}");
                println!("    (wrong password, or not a RemoteCraft peer) Waiting again ...");
                continue;
            }
        };
        println!("[+] Authenticated and encrypted. Waiting for a message ...");

        match sc.recv(&client).await {
            Ok(msg) => {
                let text = String::from_utf8_lossy(&msg);
                println!("[recv] {text}");
                if let Err(e) = sc.send(&client, &msg).await {
                    println!("[x] echo failed: {e}");
                } else {
                    println!("[echo] encrypted reply sent to peer");
                }
            }
            Err(e) => println!("[x] recv failed: {e}"),
        }
        println!("Session done. Waiting for a new connection ...");
    }
}

async fn run_connect(relay: &str, target: &str, password: &str) -> anyhow::Result<()> {
    println!("Connecting to relay {relay} ...");
    let client = RelayClient::connect(relay, None).await?;
    let id = client.id().to_string();
    println!("Relay OK. My ID is {id}. Connecting to {target} ...");

    client.connect_to(target).await?;
    println!("[+] Paired. Authenticating with password ...");

    let sc = SecureChannel::handshake(&client, true, password).await?;
    println!("[+] Authenticated and encrypted.");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let msg = format!("encrypted ping from {id} at unix {now}");
    println!("[send] {msg}");
    sc.send(&client, msg.as_bytes()).await?;

    let reply = sc.recv(&client).await?;
    let text = String::from_utf8_lossy(&reply);
    println!("[recv echo] {text}");
    println!("\n  Encrypted round-trip through the relay succeeded. Auth + crypto work.\n");
    Ok(())
}

//! RemoteCraft relay server binary.
//!
//! Usage:
//!   remotecraft-relay [--bind 0.0.0.0:21118]
//!
//! The bind address can also be set with `REMOTECRAFT_RELAY_BIND`.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use remotecraft_relay::Relay;
use tokio::net::TcpListener;
use tracing::info;

/// Default listening address. Port 21118 is RemoteCraft's rendezvous port.
const DEFAULT_BIND: &str = "0.0.0.0:21118";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let bind = parse_bind();
    let listener = TcpListener::bind(&bind).await?;
    info!(bind = %bind, "RemoteCraft relay listening");

    Relay::new().serve(listener).await?;
    Ok(())
}

fn parse_bind() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--bind" {
            if let Some(value) = args.next() {
                return value;
            }
        }
    }
    std::env::var("REMOTECRAFT_RELAY_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string())
}

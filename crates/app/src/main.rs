//! RemoteCraft desktop app.
//!
//! Two roles, one binary:
//!   remotecraft host --relay <addr> [--password <pw>] [--quality N] [--fps N]
//!   remotecraft view --relay <addr> --id <id> --password <pw>
//!
//! `host` shares this screen: it registers with the relay, shows its ID and a
//! password (random one-time if none is given, fixed if supplied), waits for a
//! viewer, authenticates, and streams JPEG screen frames.
//!
//! `view` opens a window showing the remote screen. (Mouse/keyboard control
//! and audio are later milestones.)

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod host;
mod viewer;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str);

    match mode {
        Some("host") => host::run(&args),
        Some("view") => viewer::run(&args),
        _ => {
            eprintln!(
                "usage:\n  remotecraft host --relay <host:port> [--password <pw>] [--quality 1-100] [--fps N]\n  remotecraft view --relay <host:port> --id <id> --password <pw>"
            );
            std::process::exit(2);
        }
    }
}

/// Read the value following `name` in the argument list.
pub(crate) fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

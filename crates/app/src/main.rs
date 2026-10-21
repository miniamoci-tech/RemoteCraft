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
//! `view` opens a window showing the remote screen, forwards mouse/keyboard
//! input, plays remote audio, and browses/downloads remote files.
//!
//! With no arguments (e.g. launched from its desktop icon) it opens a small
//! graphical launcher so a non-technical user can pick a role and fill in the
//! fields without the command line.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod host;
mod launcher;
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
        // No arguments: open the graphical launcher.
        None => launcher::run(),
        Some("--help") | Some("-h") | Some("help") => {
            print_usage();
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage:\n  remotecraft                 open the graphical launcher\n  remotecraft host --relay <host:port> [--password <pw>] [--quality 1-100] [--fps N] [--audio]\n  remotecraft view --relay <host:port> --id <id> --password <pw>"
    );
}

/// Read the value following `name` in the argument list.
pub(crate) fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

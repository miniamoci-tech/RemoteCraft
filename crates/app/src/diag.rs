//! Tiny opt-in diagnostic log, enabled only when the environment variable
//! `REMOTECRAFT_TRAYLOG` is set. It appends lines to that variable's path so a
//! tray/visibility problem on Windows can be diagnosed without a console.
//! Off by default, so normal runs write nothing.

use std::io::Write;

/// Append a line to the diagnostic log, if `REMOTECRAFT_TRAYLOG` is set.
pub fn log(msg: &str) {
    let Some(path) = std::env::var_os("REMOTECRAFT_TRAYLOG") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "{millis} {msg}");
    }
}

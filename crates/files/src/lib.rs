//! Host-side file transfer: list a directory and stream a file as a sequence
//! of [`AppMessage`]s. Kept separate from the app binary so it can be tested
//! headlessly against real temporary files.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::path::{Path, PathBuf};

use remotecraft_core::{AppMessage, DirEntry};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

/// Bytes per file-transfer chunk.
pub const FILE_CHUNK: usize = 256 * 1024;

/// The outbound channel closed (the viewer session ended) while streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderClosed;

/// Build a [`AppMessage::DirListing`] for `path`. An empty path lists the
/// host's home directory. Any error becomes [`AppMessage::FileError`].
pub fn list_dir(path: &str) -> AppMessage {
    let dir = if path.is_empty() {
        home_dir()
    } else {
        PathBuf::from(path)
    };

    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => {
            return AppMessage::FileError {
                message: format!("cannot open {}: {e}", dir.display()),
            }
        }
    };

    let mut entries: Vec<DirEntry> = Vec::new();
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        let full = item.path().to_string_lossy().to_string();
        let (is_dir, size) = match item.metadata() {
            Ok(m) => (m.is_dir(), m.len()),
            Err(_) => (false, 0),
        };
        entries.push(DirEntry {
            name,
            path: full,
            is_dir,
            size,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    AppMessage::DirListing {
        path: dir.to_string_lossy().to_string(),
        parent: dir.parent().map(|p| p.to_string_lossy().to_string()),
        entries,
    }
}

/// Stream `path` to `out` as `FileStart`, `FileChunk`*, `FileEnd`. A bad path
/// emits a single `FileError`. Returns `Err(())` only if `out` closed (the
/// session ended) so the caller can stop.
pub async fn stream_file(path: &str, out: &mpsc::Sender<AppMessage>) -> Result<(), SenderClosed> {
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) if m.is_file() => m,
        Ok(_) => return emit_error(out, format!("{path} is not a file")).await,
        Err(e) => return emit_error(out, format!("cannot open {path}: {e}")).await,
    };

    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download.bin".to_string());

    out.send(AppMessage::FileStart {
        name,
        size: meta.len(),
    })
    .await
    .map_err(|_| SenderClosed)?;

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => return emit_error(out, format!("open failed: {e}")).await,
    };

    let mut buf = vec![0u8; FILE_CHUNK];
    loop {
        match file.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                out.send(AppMessage::FileChunk {
                    data: buf[..n].to_vec(),
                })
                .await
                .map_err(|_| SenderClosed)?;
            }
            Err(e) => return emit_error(out, format!("read failed: {e}")).await,
        }
    }

    out.send(AppMessage::FileEnd)
        .await
        .map_err(|_| SenderClosed)
}

async fn emit_error(out: &mpsc::Sender<AppMessage>, message: String) -> Result<(), SenderClosed> {
    out.send(AppMessage::FileError { message })
        .await
        .map_err(|_| SenderClosed)
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_dir_sorts_dirs_first_and_reports_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("zsub")).expect("subdir");
        std::fs::write(dir.path().join("a.txt"), b"hello").expect("file");

        match list_dir(&dir.path().to_string_lossy()) {
            AppMessage::DirListing {
                entries, parent, ..
            } => {
                assert!(parent.is_some());
                assert_eq!(entries.len(), 2);
                // Directory sorts before the file.
                assert_eq!(entries[0].name, "zsub");
                assert!(entries[0].is_dir);
                assert_eq!(entries[1].name, "a.txt");
                assert!(!entries[1].is_dir);
                assert_eq!(entries[1].size, 5);
            }
            other => panic!("expected DirListing, got {other:?}"),
        }
    }

    #[test]
    fn list_dir_missing_path_is_error() {
        match list_dir("/no/such/path/remotecraft-xyz") {
            AppMessage::FileError { .. } => {}
            other => panic!("expected FileError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_file_emits_start_chunks_end_matching_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audio.bin");
        // Larger than one chunk so we get multiple FileChunks.
        let contents: Vec<u8> = (0..(FILE_CHUNK as u32 * 2 + 123))
            .map(|i| i as u8)
            .collect();
        std::fs::write(&path, &contents).expect("write");

        let (tx, mut rx) = mpsc::channel::<AppMessage>(8);
        let path_s = path.to_string_lossy().to_string();
        let producer = tokio::spawn(async move { stream_file(&path_s, &tx).await });

        let mut got = Vec::new();
        let mut saw_start = false;
        let mut saw_end = false;
        while let Some(msg) = rx.recv().await {
            match msg {
                AppMessage::FileStart { size, .. } => {
                    assert_eq!(size, contents.len() as u64);
                    saw_start = true;
                }
                AppMessage::FileChunk { data } => got.extend_from_slice(&data),
                AppMessage::FileEnd => {
                    saw_end = true;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        producer.await.expect("producer").expect("stream ok");
        assert!(saw_start && saw_end);
        assert_eq!(got, contents, "reassembled file matches the original");
    }
}

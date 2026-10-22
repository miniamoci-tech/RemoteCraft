//! File transfer: list host directories, stream files as [`AppMessage`]s and
//! safely create viewer downloads. Kept separate from the app binary so it
//! can be tested headlessly against real temporary files.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use remotecraft_core::{AppMessage, DirEntry};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

/// Bytes per file-transfer chunk.
pub const FILE_CHUNK: usize = 256 * 1024;

/// The outbound channel closed (the viewer session ended) while streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderClosed;

/// Create a new viewer download without trusting the remote filename or
/// overwriting an existing file (including a symlink).
///
/// Reject both Unix and Windows path syntax regardless of the viewer's OS,
/// including Windows alternate data streams and device names.
pub fn create_download(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    let mut components = Path::new(name).components();
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || name
            .chars()
            .any(|c| c.is_control() || "/\\:<>\"|?*".contains(c))
        || name.ends_with(['.', ' '])
        || device
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid download filename",
        ));
    }
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)?;
    Ok((dest, file))
}

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
    fn download_rejects_paths_and_windows_special_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in [
            "",
            ".",
            "..",
            "../outside.txt",
            "..\\outside.txt",
            "/tmp/outside.txt",
            "C:\\outside.txt",
            "C:outside.txt",
            "\\\\server\\share\\file",
            "sub/file",
            "sub\\file",
            "./file",
            "file/",
            "file:stream",
            "NUL",
            "con.txt",
            "AUX",
            "COM1.log",
            "LPT9",
            "COM¹",
            "file.",
            "file ",
            "bad\0name",
        ] {
            let err = create_download(dir.path(), name).expect_err(name);
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        }
        assert_eq!(std::fs::read_dir(dir.path()).expect("read_dir").count(), 0);
    }

    #[test]
    fn download_creates_file_and_preserves_existing_contents() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let downloads = dir.path().join("downloads");
        let (dest, mut file) = create_download(&downloads, "recording è.txt").expect("create");
        file.write_all(b"keep this").expect("write");
        drop(file);
        assert_eq!(dest, downloads.join("recording è.txt"));
        let err = create_download(&downloads, "recording è.txt").expect_err("must not replace");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(dest).expect("read"), b"keep this");
    }

    #[cfg(unix)]
    #[test]
    fn download_does_not_follow_existing_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("outside.txt");
        std::fs::write(&target, b"keep this").expect("write");
        let downloads = dir.path().join("downloads");
        std::fs::create_dir(&downloads).expect("mkdir");
        std::os::unix::fs::symlink(&target, downloads.join("file.txt")).expect("symlink");
        assert!(create_download(&downloads, "file.txt").is_err());
        assert_eq!(std::fs::read(target).expect("read"), b"keep this");
    }

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

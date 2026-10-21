//! Screen capture for the host, with JPEG encoding of frames.
//!
//! Wraps the cross-platform `xcap` crate. We pull the raw RGBA bytes out of
//! xcap's image type and re-encode with our own `image` crate, so a version
//! mismatch between the two can never break the build.
//!
//! On macOS the OS gates screen capture behind the "Screen Recording"
//! permission; the first capture there will fail until the user grants it in
//! System Settings. Capture errors are returned, never panicked.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, RgbaImage};
use xcap::Monitor;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no monitor found to capture")]
    NoMonitor,
    #[error("screen capture failed (on macOS, grant Screen Recording permission): {0}")]
    Capture(String),
    #[error("captured frame had an invalid size {width}x{height} for {len} bytes")]
    BadSize { width: u32, height: u32, len: usize },
    #[error("jpeg encoding failed: {0}")]
    Encode(String),
}

/// A captured, JPEG-encoded screen frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}

/// A capture source bound to one monitor.
#[derive(Clone)]
pub struct Capturer {
    monitor: Monitor,
    quality: u8,
    width: u32,
    height: u32,
}

impl Capturer {
    /// Bind to the primary monitor (or the first one if none is marked
    /// primary). `quality` is the JPEG quality 1..=100.
    pub fn primary(quality: u8) -> Result<Self, CaptureError> {
        let monitors = Monitor::all().map_err(|e| CaptureError::Capture(e.to_string()))?;
        // `is_primary()` returns a Result in xcap 0.9; treat an error as "not
        // primary" so a single misbehaving monitor can't abort selection.
        let monitor = monitors
            .iter()
            .find(|m| m.is_primary().unwrap_or(false))
            .cloned()
            .or_else(|| monitors.into_iter().next())
            .ok_or(CaptureError::NoMonitor)?;
        // xcap 0.9 returns these as Result; read them once at bind time.
        let width = monitor
            .width()
            .map_err(|e| CaptureError::Capture(e.to_string()))?;
        let height = monitor
            .height()
            .map_err(|e| CaptureError::Capture(e.to_string()))?;
        Ok(Self {
            monitor,
            quality: quality.clamp(1, 100),
            width,
            height,
        })
    }

    /// Capture one frame and encode it as JPEG.
    pub fn capture(&self) -> Result<Frame, CaptureError> {
        let xi = self
            .monitor
            .capture_image()
            .map_err(|e| CaptureError::Capture(e.to_string()))?;
        let width = xi.width();
        let height = xi.height();
        // Pull raw RGBA bytes; this is independent of xcap's `image` version.
        let raw: Vec<u8> = xi.into_raw();
        encode_jpeg(width, height, raw, self.quality)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

/// Encode raw RGBA8 pixels as a JPEG `Frame`.
fn encode_jpeg(width: u32, height: u32, raw: Vec<u8>, quality: u8) -> Result<Frame, CaptureError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4));
    if expected != Some(raw.len()) {
        return Err(CaptureError::BadSize {
            width,
            height,
            len: raw.len(),
        });
    }
    let rgba = RgbaImage::from_raw(width, height, raw).ok_or(CaptureError::BadSize {
        width,
        height,
        len: 0,
    })?;
    // JPEG is RGB; drop the alpha channel.
    let rgb = DynamicImage::ImageRgba8(rgba).to_rgb8();
    let mut jpeg = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut jpeg, quality);
    enc.encode_image(&rgb)
        .map_err(|e| CaptureError::Encode(e.to_string()))?;
    Ok(Frame {
        width,
        height,
        jpeg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_synthetic_frame() {
        // 2x2 RGBA image (16 bytes) encodes to a non-empty JPEG.
        let raw = vec![255u8; 2 * 2 * 4];
        let frame = encode_jpeg(2, 2, raw, 80).expect("encode");
        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 2);
        assert!(!frame.jpeg.is_empty());
        // JPEG magic bytes.
        assert_eq!(&frame.jpeg[0..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn mismatched_size_is_rejected() {
        let raw = vec![0u8; 10]; // not width*height*4
        let err = encode_jpeg(4, 4, raw, 80).expect_err("should reject");
        assert!(matches!(err, CaptureError::BadSize { .. }));
    }
}

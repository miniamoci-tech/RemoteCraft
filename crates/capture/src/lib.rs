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

/// Geometry of one monitor, for the viewer's monitor picker and for mapping
/// input to the right place in the virtual desktop.
#[derive(Debug, Clone, Copy)]
pub struct MonitorGeom {
    pub index: u32,
    /// Top-left position in the virtual desktop (can be negative).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

/// List all monitors with their geometry, ordered with the primary first so a
/// stable index can address each one. Monitors whose size can't be read are
/// skipped rather than failing the whole call.
pub fn list_monitors() -> Result<Vec<MonitorGeom>, CaptureError> {
    let monitors = Monitor::all().map_err(|e| CaptureError::Capture(e.to_string()))?;
    let mut geoms: Vec<(Monitor, MonitorGeom)> = Vec::new();
    for m in monitors {
        let (Ok(width), Ok(height)) = (m.width(), m.height()) else {
            continue;
        };
        let geom = MonitorGeom {
            index: 0, // assigned after sorting
            x: m.x().unwrap_or(0),
            y: m.y().unwrap_or(0),
            width,
            height,
            is_primary: m.is_primary().unwrap_or(false),
        };
        geoms.push((m, geom));
    }
    if geoms.is_empty() {
        return Err(CaptureError::NoMonitor);
    }
    // Primary first, then by horizontal then vertical position, for a stable,
    // intuitive order ("1" is the primary).
    geoms.sort_by_key(|(_, g)| (!g.is_primary, g.x, g.y));
    Ok(geoms
        .into_iter()
        .enumerate()
        .map(|(i, (_, mut g))| {
            g.index = i as u32;
            g
        })
        .collect())
}

/// A capture source bound to one monitor.
#[derive(Clone)]
pub struct Capturer {
    monitor: Monitor,
    quality: u8,
    geom: MonitorGeom,
}

impl Capturer {
    /// Bind to the primary monitor (index 0 in [`list_monitors`]).
    pub fn primary(quality: u8) -> Result<Self, CaptureError> {
        Self::for_index(0, quality)
    }

    /// Bind to the monitor at `index` in [`list_monitors`] order. Falls back to
    /// the first monitor if the index is out of range.
    pub fn for_index(index: u32, quality: u8) -> Result<Self, CaptureError> {
        let geoms = list_monitors()?;
        let geom = geoms
            .iter()
            .find(|g| g.index == index)
            .copied()
            .or_else(|| geoms.first().copied())
            .ok_or(CaptureError::NoMonitor)?;

        // Re-resolve the matching xcap Monitor by position (xcap gives no stable
        // id). Match on x/y, which `list_monitors` read from the same monitors.
        let monitors = Monitor::all().map_err(|e| CaptureError::Capture(e.to_string()))?;
        let monitor = monitors
            .into_iter()
            .find(|m| m.x().unwrap_or(0) == geom.x && m.y().unwrap_or(0) == geom.y)
            .ok_or(CaptureError::NoMonitor)?;
        Ok(Self {
            monitor,
            quality: quality.clamp(1, 100),
            geom,
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
        self.geom.width
    }

    pub fn height(&self) -> u32 {
        self.geom.height
    }

    /// Geometry (index, position, size) of the bound monitor.
    pub fn geom(&self) -> MonitorGeom {
        self.geom
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

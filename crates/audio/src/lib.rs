//! Remote audio for RemoteCraft (M5).
//!
//! - Capture: the host's *playback* output (what you hear), via WASAPI
//!   loopback on Windows. Other platforms return [`AudioError::Unsupported`]
//!   for now (macOS system-audio capture needs extra components).
//! - Playback: the viewer plays received PCM through its default output
//!   device, via the cross-platform `cpal` crate.
//!
//! Audio is carried as interleaved 16-bit signed PCM with its sample rate and
//! channel count, so no codec is required. The viewer opens its output at the
//! host's rate/channels; if the device cannot, audio is skipped with an error
//! rather than played at the wrong speed.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// PCM format descriptor shared by capture and playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("remote audio is not yet supported on this operating system")]
    Unsupported,
    #[error("no audio output device available")]
    NoOutputDevice,
    #[error("the output device does not support {0} Hz / {1} channels (16-bit)")]
    UnsupportedConfig(u32, u16),
    #[error("audio capture failed: {0}")]
    Capture(String),
    #[error("audio playback failed: {0}")]
    Playback(String),
}

/// Keep at most this many samples buffered for playback (~1s at 48k stereo),
/// so audio never drifts far behind the screen; excess is dropped.
const MAX_BUFFERED_SAMPLES: usize = 48_000 * 2;

/// Plays received PCM through the default output device. The cpal stream is
/// not `Send`, so an `AudioPlayer` must be created and kept on one thread.
pub struct AudioPlayer {
    buffer: Arc<Mutex<VecDeque<i16>>>,
    _stream: cpal::Stream,
}

impl AudioPlayer {
    /// Open the default output device for `format`.
    pub fn new(format: AudioFormat) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(AudioError::NoOutputDevice)?;

        let config = cpal::StreamConfig {
            channels: format.channels,
            sample_rate: cpal::SampleRate(format.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let buffer = Arc::new(Mutex::new(VecDeque::<i16>::new()));
        let cb_buffer = buffer.clone();
        let err_fn = |e| tracing::warn!(error = %e, "audio output stream error");

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let mut buf = match cb_buffer.lock() {
                        Ok(b) => b,
                        Err(p) => p.into_inner(),
                    };
                    for sample in data.iter_mut() {
                        *sample = buf.pop_front().unwrap_or(0);
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| AudioError::UnsupportedConfig(format.sample_rate, format.channels).wrap(e))?;

        stream.play().map_err(|e| AudioError::Playback(e.to_string()))?;
        Ok(Self { buffer, _stream: stream })
    }

    /// Queue PCM samples for playback, dropping the oldest if too far behind.
    pub fn push(&self, samples: &[i16]) {
        let mut buf = match self.buffer.lock() {
            Ok(b) => b,
            Err(p) => p.into_inner(),
        };
        buf.extend(samples.iter().copied());
        while buf.len() > MAX_BUFFERED_SAMPLES {
            buf.pop_front();
        }
    }
}

// Small helper so the UnsupportedConfig error can carry the cpal cause in logs
// without changing the public variant shape.
trait WrapErr {
    fn wrap<E: std::fmt::Display>(self, cause: E) -> AudioError;
}
impl WrapErr for AudioError {
    fn wrap<E: std::fmt::Display>(self, cause: E) -> AudioError {
        tracing::warn!(cause = %cause, "audio output config rejected");
        self
    }
}

/// Convert a float sample in [-1.0, 1.0] to 16-bit PCM.
pub fn f32_to_i16(f: f32) -> i16 {
    let clamped = f.clamp(-1.0, 1.0);
    (clamped * i16::MAX as f32) as i16
}

/// Interpret a little-endian byte buffer of 16-bit samples as `i16`s.
pub fn bytes_to_i16(bytes: &[u8]) -> Vec<i16> {
    let (pairs, _rest) = bytes.as_chunks::<2>();
    pairs.iter().map(|c| i16::from_le_bytes(*c)).collect()
}

/// Interpret a little-endian byte buffer of 32-bit floats as 16-bit PCM.
pub fn f32_bytes_to_i16(bytes: &[u8]) -> Vec<i16> {
    let (quads, _rest) = bytes.as_chunks::<4>();
    quads
        .iter()
        .map(|c| f32_to_i16(f32::from_le_bytes(*c)))
        .collect()
}

#[cfg(windows)]
mod capture_impl {
    use super::{AudioError, AudioFormat};
    use std::collections::VecDeque;
    use std::sync::mpsc::Sender;

    /// Spawn a thread that captures the host's playback output via WASAPI
    /// loopback and sends `(format, samples)` batches until `tx` is dropped.
    pub fn spawn(tx: Sender<(AudioFormat, Vec<i16>)>) -> Result<(), AudioError> {
        std::thread::spawn(move || {
            if let Err(e) = run(&tx) {
                tracing::warn!(error = %e, "audio capture stopped");
            }
        });
        Ok(())
    }

    fn run(tx: &Sender<(AudioFormat, Vec<i16>)>) -> Result<(), AudioError> {
        use wasapi::*;

        let _ = initialize_mta();
        let device = get_default_device(&Direction::Render)
            .map_err(|e| AudioError::Capture(format!("default render device: {e}")))?;
        let mut audio_client = device
            .get_iaudioclient()
            .map_err(|e| AudioError::Capture(format!("audio client: {e}")))?;

        // Use the device's own mix format for shared-mode loopback.
        let format = audio_client
            .get_mixformat()
            .map_err(|e| AudioError::Capture(format!("mix format: {e}")))?;
        let sample_rate = format.get_samplespersec();
        let channels = format.get_nchannels();
        let bits = format.get_bitspersample();
        let is_float = matches!(format.get_subformat(), Ok(SampleType::Float));

        let (_def, min_time) = audio_client
            .get_periods()
            .map_err(|e| AudioError::Capture(format!("periods: {e}")))?;
        audio_client
            .initialize_client(&format, min_time, &Direction::Capture, &ShareMode::Shared, true)
            .map_err(|e| AudioError::Capture(format!("initialize loopback: {e}")))?;

        let h_event = audio_client
            .set_get_eventhandle()
            .map_err(|e| AudioError::Capture(format!("event handle: {e}")))?;
        let capture_client = audio_client
            .get_audiocaptureclient()
            .map_err(|e| AudioError::Capture(format!("capture client: {e}")))?;
        audio_client
            .start_stream()
            .map_err(|e| AudioError::Capture(format!("start: {e}")))?;

        let fmt = AudioFormat { sample_rate, channels };
        let mut queue: VecDeque<u8> = VecDeque::new();

        loop {
            capture_client
                .read_from_device_to_deque(&mut queue)
                .map_err(|e| AudioError::Capture(format!("read: {e}")))?;

            if !queue.is_empty() {
                let bytes: Vec<u8> = queue.drain(..).collect();
                let samples = if is_float && bits == 32 {
                    super::f32_bytes_to_i16(&bytes)
                } else if bits == 16 {
                    super::bytes_to_i16(&bytes)
                } else {
                    // Unknown format; stop rather than emit noise.
                    return Err(AudioError::Capture(format!(
                        "unsupported capture format: {bits}-bit float={is_float}"
                    )));
                };
                if tx.send((fmt, samples)).is_err() {
                    return Ok(()); // viewer gone
                }
            }

            if h_event.wait_for_event(1000).is_err() {
                // Timeout/stop; keep going unless the channel closed.
                if tx.send((fmt, Vec::new())).is_err() {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod capture_impl {
    use super::{AudioError, AudioFormat};
    use std::sync::mpsc::Sender;

    pub fn spawn(_tx: Sender<(AudioFormat, Vec<i16>)>) -> Result<(), AudioError> {
        Err(AudioError::Unsupported)
    }
}

/// Start capturing the host's playback output on a background thread.
/// `(format, samples)` batches are sent until the receiver is dropped.
pub fn spawn_capture(
    tx: std::sync::mpsc::Sender<(AudioFormat, Vec<i16>)>,
) -> Result<(), AudioError> {
    capture_impl::spawn(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_conversion_clamps_and_scales() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(2.0), i16::MAX); // clamped
        assert_eq!(f32_to_i16(-1.0), -i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -i16::MAX); // clamped
    }

    #[test]
    fn byte_reinterpretation() {
        // Two i16 samples: 1 and -1, little-endian.
        let bytes = [0x01, 0x00, 0xff, 0xff];
        assert_eq!(bytes_to_i16(&bytes), vec![1, -1]);
        // Odd trailing byte is ignored (chunks_exact).
        let bytes = [0x01, 0x00, 0x7f];
        assert_eq!(bytes_to_i16(&bytes), vec![1]);
    }

    #[test]
    fn f32_bytes_roundtrip() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        bytes.extend_from_slice(&0.0f32.to_le_bytes());
        assert_eq!(f32_bytes_to_i16(&bytes), vec![i16::MAX, 0]);
    }
}

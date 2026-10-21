//! Input injection on the host: apply [`InputEvent`]s from the viewer to the
//! real mouse and keyboard, via the cross-platform `enigo` crate.
//!
//! `enigo`'s handle is not `Send`, so [`InputInjector`] is created and used on
//! one dedicated thread (see the host). On macOS, input injection needs the
//! "Accessibility" permission; without it `enigo` returns an error, which is
//! propagated rather than panicked.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use remotecraft_core::{InputEvent, KeyName, MouseButton};

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("could not initialize input injection (on macOS, grant Accessibility permission): {0}")]
    Init(String),
    #[error("input injection failed: {0}")]
    Inject(String),
}

/// Applies input events to the host's mouse and keyboard.
pub struct InputInjector {
    enigo: Enigo,
    screen_w: i32,
    screen_h: i32,
}

impl InputInjector {
    /// Create an injector. `screen_w`/`screen_h` are the host screen size used
    /// to turn normalized pointer coordinates into absolute pixels.
    pub fn new(screen_w: u32, screen_h: u32) -> Result<Self, InputError> {
        let enigo = Enigo::new(&Settings::default()).map_err(|e| InputError::Init(e.to_string()))?;
        Ok(Self {
            enigo,
            screen_w: screen_w.max(1) as i32,
            screen_h: screen_h.max(1) as i32,
        })
    }

    /// Apply one input event.
    pub fn apply(&mut self, event: &InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMove { x, y } => {
                let (px, py) = normalized_to_pixels(*x, *y, self.screen_w, self.screen_h);
                self.enigo
                    .move_mouse(px, py, Coordinate::Abs)
                    .map_err(|e| InputError::Inject(e.to_string()))
            }
            InputEvent::MouseButton { button, pressed } => {
                let dir = if *pressed { Direction::Press } else { Direction::Release };
                self.enigo
                    .button(map_button(*button), dir)
                    .map_err(|e| InputError::Inject(e.to_string()))
            }
            InputEvent::Scroll { dx, dy } => {
                // Positive dy = scroll up; enigo treats positive as down, so negate.
                if *dy != 0 {
                    self.enigo
                        .scroll(-*dy, Axis::Vertical)
                        .map_err(|e| InputError::Inject(e.to_string()))?;
                }
                if *dx != 0 {
                    self.enigo
                        .scroll(*dx, Axis::Horizontal)
                        .map_err(|e| InputError::Inject(e.to_string()))?;
                }
                Ok(())
            }
            InputEvent::Text(text) => self
                .enigo
                .text(text)
                .map_err(|e| InputError::Inject(e.to_string())),
            InputEvent::KeyClick { key, ctrl, alt, shift, meta } => {
                self.key_click(*key, *ctrl, *alt, *shift, *meta)
            }
        }
    }

    fn key_click(
        &mut self,
        key: KeyName,
        ctrl: bool,
        alt: bool,
        shift: bool,
        meta: bool,
    ) -> Result<(), InputError> {
        // Press held modifiers, click the key, then always release modifiers.
        let mods = [
            (ctrl, Key::Control),
            (alt, Key::Alt),
            (shift, Key::Shift),
            (meta, Key::Meta),
        ];
        for (on, k) in mods {
            if on {
                self.enigo
                    .key(k, Direction::Press)
                    .map_err(|e| InputError::Inject(e.to_string()))?;
            }
        }
        let click = self.enigo.key(map_key(key), Direction::Click);
        // Release in reverse order, ignoring errors so a stuck modifier can't
        // be left pressed on top of an injection failure.
        for (on, k) in mods.iter().rev() {
            if *on {
                let _ = self.enigo.key(*k, Direction::Release);
            }
        }
        click.map_err(|e| InputError::Inject(e.to_string()))
    }
}

fn map_button(b: MouseButton) -> Button {
    match b {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}

fn map_key(k: KeyName) -> Key {
    match k {
        KeyName::Enter => Key::Return,
        KeyName::Backspace => Key::Backspace,
        KeyName::Tab => Key::Tab,
        KeyName::Escape => Key::Escape,
        KeyName::Delete => Key::Delete,
        KeyName::Space => Key::Space,
        KeyName::ArrowUp => Key::UpArrow,
        KeyName::ArrowDown => Key::DownArrow,
        KeyName::ArrowLeft => Key::LeftArrow,
        KeyName::ArrowRight => Key::RightArrow,
        KeyName::Home => Key::Home,
        KeyName::End => Key::End,
        KeyName::PageUp => Key::PageUp,
        KeyName::PageDown => Key::PageDown,
        KeyName::Char(c) => Key::Unicode(c),
        KeyName::Function(n) => match n {
            1 => Key::F1,
            2 => Key::F2,
            3 => Key::F3,
            4 => Key::F4,
            5 => Key::F5,
            6 => Key::F6,
            7 => Key::F7,
            8 => Key::F8,
            9 => Key::F9,
            10 => Key::F10,
            11 => Key::F11,
            _ => Key::F12,
        },
    }
}

/// Map a normalized (0.0..=1.0) position to absolute pixels, clamped to the
/// screen. Pure function so it can be tested without a display.
fn normalized_to_pixels(x: f32, y: f32, w: i32, h: i32) -> (i32, i32) {
    let cx = x.clamp(0.0, 1.0);
    let cy = y.clamp(0.0, 1.0);
    let px = (cx * w as f32).round() as i32;
    let py = (cy * h as f32).round() as i32;
    (px.clamp(0, w - 1), py.clamp(0, h - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_mapping() {
        assert_eq!(normalized_to_pixels(0.0, 0.0, 1920, 1080), (0, 0));
        assert_eq!(normalized_to_pixels(1.0, 1.0, 1920, 1080), (1919, 1079));
        assert_eq!(normalized_to_pixels(0.5, 0.5, 1920, 1080), (960, 540));
        // Out-of-range input is clamped, never panics or goes negative.
        assert_eq!(normalized_to_pixels(-1.0, 2.0, 800, 600), (0, 599));
    }
}

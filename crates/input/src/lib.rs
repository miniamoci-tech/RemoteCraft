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

use enigo::{Axis, Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use remotecraft_core::{InputEvent, KeyName, MouseButton};

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error(
        "could not initialize input injection (on macOS, grant Accessibility permission): {0}"
    )]
    Init(String),
    #[error("input injection failed: {0}")]
    Inject(String),
}

/// Applies input events to the host's mouse and keyboard, mapped to one
/// monitor's rectangle in the virtual desktop.
pub struct InputInjector {
    enigo: Enigo,
    /// Top-left of the target monitor in the virtual desktop (can be negative).
    origin_x: i32,
    origin_y: i32,
    screen_w: i32,
    screen_h: i32,
}

impl InputInjector {
    /// Create an injector for a monitor at the virtual-desktop origin (0, 0)
    /// with the given size (the common single-monitor / primary case).
    pub fn new(screen_w: u32, screen_h: u32) -> Result<Self, InputError> {
        Self::with_bounds(0, 0, screen_w, screen_h)
    }

    /// Create an injector for a monitor whose top-left is at (`origin_x`,
    /// `origin_y`) in the virtual desktop and whose size is `screen_w` x
    /// `screen_h`. Normalized pointer coordinates map into that rectangle.
    pub fn with_bounds(
        origin_x: i32,
        origin_y: i32,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<Self, InputError> {
        let enigo =
            Enigo::new(&Settings::default()).map_err(|e| InputError::Init(e.to_string()))?;
        Ok(Self {
            enigo,
            origin_x,
            origin_y,
            screen_w: screen_w.max(1) as i32,
            screen_h: screen_h.max(1) as i32,
        })
    }

    /// Point the injector at a different monitor without recreating it.
    pub fn set_bounds(&mut self, origin_x: i32, origin_y: i32, screen_w: u32, screen_h: u32) {
        self.origin_x = origin_x;
        self.origin_y = origin_y;
        self.screen_w = screen_w.max(1) as i32;
        self.screen_h = screen_h.max(1) as i32;
    }

    /// Apply one input event.
    pub fn apply(&mut self, event: &InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMove { x, y } => {
                let (px, py) = normalized_to_pixels(*x, *y, self.screen_w, self.screen_h);
                let (gx, gy) = (self.origin_x + px, self.origin_y + py);
                move_pointer(&mut self.enigo, gx, gy)
            }
            InputEvent::MouseButton { button, pressed } => {
                let dir = if *pressed {
                    Direction::Press
                } else {
                    Direction::Release
                };
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
            InputEvent::KeyClick {
                key,
                ctrl,
                alt,
                shift,
                meta,
            } => self.key_click(*key, *ctrl, *alt, *shift, *meta),
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

/// Move the system pointer to a virtual-desktop coordinate (can be on any
/// monitor). On Windows this uses `SetCursorPos`, because enigo's absolute
/// move only addresses the primary monitor; elsewhere it uses enigo.
#[cfg(windows)]
#[allow(unsafe_code)]
fn move_pointer(_enigo: &mut Enigo, gx: i32, gy: i32) -> Result<(), InputError> {
    // SAFETY: `SetCursorPos` only moves the system cursor to the given screen
    // coordinate; it takes two integers, reads no caller memory, and cannot
    // cause UB. A failure returns 0, which we surface as an error.
    let ok = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(gx, gy) };
    if ok == 0 {
        Err(InputError::Inject("SetCursorPos failed".to_string()))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn move_pointer(enigo: &mut Enigo, gx: i32, gy: i32) -> Result<(), InputError> {
    enigo
        .move_mouse(gx, gy, enigo::Coordinate::Abs)
        .map_err(|e| InputError::Inject(e.to_string()))
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

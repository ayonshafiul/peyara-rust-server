//! Desktop input backend: translates mobile-app commands into OS-level
//! mouse / keyboard events via `enigo`.
//!
//! All blocking `enigo` calls run on a dedicated worker thread so the Tokio
//! runtime and the egui UI thread are never blocked. Pure mapping helpers are
//! unit-tested (`cargo test`).

use enigo::{Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use std::sync::mpsc::{self, Sender};

/// Commands sent by the mobile client (mirrors the socket.io events of the
/// original Electron server in `src/main.js`).
#[derive(Debug, Clone)]
pub enum InputCommand {
    /// Relative mouse movement (`coordinates` event, `{x, y}` pixels).
    MouseMove { x: i32, y: i32 },
    /// Mouse click (`clicks` event, `{finger, doubleTap}`).
    Click { button: Button, double: bool },
    /// Scroll (`scroll` event, `{x, y}` ticks).
    Scroll { x: i32, y: i32 },
    /// Press left button down (`windowdragstart`).
    DragStart,
    /// Relative drag movement (`windowdragupdate`, `{x, y}` pixels).
    DragMove { x: i32, y: i32 },
    /// Release left button (`windowdragend`).
    DragEnd,
    /// Single key press (`key` event).
    KeyPress { key: Key },
    /// Type an arbitrary string (`key` fallback / `text` path).
    TypeText { text: String },
    /// Media / arrow key (`media-key` event).
    MediaKey { key: Key },
    /// Key + modifiers (`edit-key` event, `{key, modifier}`).
    EditKey { key: Key, modifiers: Vec<Key> },
}

/// Map a mouse button name from the mobile client to enigo.
pub fn map_button(finger: &str) -> Button {
    match finger.to_lowercase().as_str() {
        "right" => Button::Right,
        "middle" | "center" => Button::Middle,
        _ => Button::Left,
    }
}

/// Map a named key (case-insensitive) to enigo. Returns `None` for unknown names.
pub fn map_named_key(name: &str) -> Option<Key> {
    Some(match name.to_lowercase().as_str() {
        "backspace" => Key::Backspace,
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "space" | " " => Key::Space,
        "delete" | "del" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "up" | "uparrow" => Key::UpArrow,
        "down" | "downarrow" => Key::DownArrow,
        "left" | "leftarrow" => Key::LeftArrow,
        "right" | "rightarrow" => Key::RightArrow,
        "shift" => Key::Shift,
        "control" | "ctrl" => Key::Control,
        "alt" | "option" => Key::Alt,
        "command" | "cmd" | "meta" | "super" | "windows" | "win" => Key::Meta,
        "capslock" => Key::CapsLock,
        // Insert/Pause/PrintScr/Numlock exist everywhere except macOS;
        // ScrollLock is Unix-only (also absent on Windows).
        #[cfg(not(target_os = "macos"))]
        "insert" => Key::Insert,
        #[cfg(not(target_os = "macos"))]
        "pause" => Key::Pause,
        #[cfg(not(target_os = "macos"))]
        "printscreen" | "printscr" => Key::PrintScr,
        #[cfg(all(unix, not(target_os = "macos")))]
        "scrolllock" => Key::ScrollLock,
        #[cfg(not(target_os = "macos"))]
        "numlock" => Key::Numlock,
        f if f.len() == 2 && f.starts_with('f') => match &f[1..] {
            "1" => Key::F1,
            "2" => Key::F2,
            "3" => Key::F3,
            "4" => Key::F4,
            "5" => Key::F5,
            "6" => Key::F6,
            "7" => Key::F7,
            "8" => Key::F8,
            "9" => Key::F9,
            _ => return None,
        },
        f if f.len() == 3 && f.starts_with('f') => match &f[1..] {
            "10" => Key::F10,
            "11" => Key::F11,
            "12" => Key::F12,
            _ => return None,
        },
        _ => return None,
    })
}

/// Map a modifier name from an `edit-key` payload to enigo.
pub fn map_modifier(name: &str) -> Option<Key> {
    Some(match name.to_lowercase().as_str() {
        "shift" => Key::Shift,
        "control" | "ctrl" => Key::Control,
        "alt" | "option" => Key::Alt,
        "command" | "cmd" | "meta" | "super" | "windows" | "win" => Key::Meta,
        _ => return None,
    })
}

/// Map a `media-key` event name to enigo.
///
/// Mirrors the `mediaKeys` allowlist in the original `src/main.js`; unknown
/// names return `None` and are ignored.
pub fn map_media_key(name: &str) -> Option<Key> {
    Some(match name {
        "audio_mute" => Key::VolumeMute,
        "audio_vol_down" => Key::VolumeDown,
        "audio_vol_up" => Key::VolumeUp,
        "audio_play" | "audio_pause" => Key::MediaPlayPause,
        // No stop key on macOS — the name is ignored there.
        #[cfg(not(target_os = "macos"))]
        "audio_stop" => Key::MediaStop,
        "audio_prev" => Key::MediaPrevTrack,
        "audio_next" => Key::MediaNextTrack,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        _ => return None,
    })
}

/// Resolve the `key` socket event payload to an [`InputCommand`].
///
/// Faithful port of the `socket.on("key")` handler in `src/main.js`:
/// single ASCII-lowercase/digit chars are tapped, everything else is typed;
/// `backspace`/`enter` are tapped as special keys.
pub fn resolve_key_event(key: &str) -> InputCommand {
    if key.chars().count() == 1 {
        let c = key.chars().next().unwrap();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            return InputCommand::KeyPress {
                key: Key::Unicode(c),
            };
        }
        return InputCommand::TypeText {
            text: key.to_owned(),
        };
    }
    match key.to_lowercase().as_str() {
        "backspace" => InputCommand::KeyPress {
            key: Key::Backspace,
        },
        "enter" => InputCommand::KeyPress { key: Key::Return },
        named => match map_named_key(named) {
            Some(k) => InputCommand::KeyPress { key: k },
            None => InputCommand::TypeText {
                text: key.to_owned(),
            },
        },
    }
}

/// Resolve a key name inside an `edit-key` payload.
pub fn resolve_edit_key(key: &str) -> Key {
    if key.chars().count() == 1 {
        Key::Unicode(key.chars().next().unwrap())
    } else {
        map_named_key(key).unwrap_or(Key::Unicode('\0'))
    }
}

/// What kind of desktop session we run in. Determines which input backend
/// can actually move the visible cursor.
fn session_kind() -> &'static str {
    match std::env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("wayland") => "wayland",
        Ok("x11") => "x11",
        _ if std::env::var_os("WAYLAND_DISPLAY").is_some() => "wayland (detected)",
        _ if std::env::var_os("DISPLAY").is_some() => "x11 (detected)",
        _ => "unknown (headless?)",
    }
}

/// Spawn the dedicated input worker thread. Returns a channel sender for
/// [`InputCommand`]s plus a log line receiver for UI diagnostics.
///
/// On Wayland (e.g. Ubuntu 24's default GNOME session) X11 injection only
/// reaches XWayland windows, so enigo must go through the Remote Desktop
/// portal (libei). That pops up a one-time system permission prompt which
/// the user has to approve.
pub fn spawn_input_worker() -> (Sender<InputCommand>, mpsc::Receiver<String>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<InputCommand>();
    let (log_tx, log_rx) = mpsc::channel::<String>();

    std::thread::spawn(move || {
        let session = session_kind();
        let _ = log_tx.send(format!("desktop session: {session}"));
        if session.starts_with("wayland") {
            let _ = log_tx.send(
                "Wayland detected: input goes through the Remote Desktop portal. \
                 Approve the system permission prompt, then mouse/keyboard will work."
                    .to_owned(),
            );
        }
        // Match the snappy delays of the original robotjs setup
        // (mouse 1ms, keyboard 2ms); only affects the X11 backend.
        let settings = Settings {
            linux_delay: 1,
            ..Settings::default()
        };
        let mut enigo = match Enigo::new(&settings) {
            Ok(e) => {
                let _ = log_tx.send("input backend ready".to_owned());
                Some(e)
            }
            Err(e) => {
                let _ = log_tx.send(format!("input backend unavailable: {e}"));
                if session.starts_with("wayland") {
                    let _ = log_tx.send(
                        "Wayland fix: make sure xdg-desktop-portal is running and relaunch \
                         the app so you can approve the Remote Desktop prompt."
                            .to_owned(),
                    );
                }
                None
            }
        };
        let Some(enigo) = enigo.as_mut() else {
            return;
        };
        for cmd in cmd_rx {
            if let Err(e) = execute(enigo, cmd) {
                let _ = log_tx.send(format!("input error: {e}"));
            }
        }
    });

    (cmd_tx, log_rx)
}

fn execute(enigo: &mut Enigo, cmd: InputCommand) -> Result<(), enigo::InputError> {
    use enigo::{Axis, Coordinate};
    match cmd {
        InputCommand::MouseMove { x, y } => {
            enigo.move_mouse(x, y, Coordinate::Rel)?;
        }
        InputCommand::Click { button, double } => {
            enigo.button(button, Direction::Click)?;
            if double {
                enigo.button(button, Direction::Click)?;
            }
        }
        InputCommand::Scroll { x, y } => {
            if x != 0 {
                enigo.scroll(x, Axis::Horizontal)?;
            }
            if y != 0 {
                enigo.scroll(y, Axis::Vertical)?;
            }
        }
        InputCommand::DragStart => {
            enigo.button(Button::Left, Direction::Press)?;
        }
        InputCommand::DragMove { x, y } => {
            enigo.move_mouse(x, y, Coordinate::Rel)?;
        }
        InputCommand::DragEnd => {
            enigo.button(Button::Left, Direction::Release)?;
        }
        InputCommand::KeyPress { key } | InputCommand::MediaKey { key } => {
            enigo.key(key, Direction::Click)?;
        }
        InputCommand::TypeText { text } => {
            enigo.text(&text)?;
        }
        InputCommand::EditKey { key, modifiers } => {
            for m in &modifiers {
                enigo.key(*m, Direction::Press)?;
            }
            enigo.key(key, Direction::Click)?;
            for m in modifiers.iter().rev() {
                enigo.key(*m, Direction::Release)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_map() {
        assert_eq!(map_button("left"), Button::Left);
        assert_eq!(map_button("RIGHT"), Button::Right);
        assert_eq!(map_button("middle"), Button::Middle);
        assert_eq!(map_button("unknown"), Button::Left);
    }

    #[test]
    fn named_keys_map() {
        assert_eq!(map_named_key("backspace"), Some(Key::Backspace));
        assert_eq!(map_named_key("ENTER"), Some(Key::Return));
        assert_eq!(map_named_key("up"), Some(Key::UpArrow));
        assert_eq!(map_named_key("f12"), Some(Key::F12));
        assert_eq!(map_named_key("shift"), Some(Key::Shift));
        assert_eq!(map_named_key("ctrl"), Some(Key::Control));
        assert_eq!(map_named_key("cmd"), Some(Key::Meta));
        assert_eq!(map_named_key("nope"), None);
    }

    #[test]
    fn media_keys_allowlist_matches_original() {
        // audio_stop has no key on macOS (enigo omits MediaStop there).
        let mut names = vec![
            "audio_mute",
            "audio_vol_down",
            "audio_vol_up",
            "audio_play",
            "audio_pause",
            "audio_prev",
            "audio_next",
            "up",
            "down",
            "left",
            "right",
        ];
        #[cfg(not(target_os = "macos"))]
        names.push("audio_stop");
        for name in names {
            assert!(map_media_key(name).is_some(), "{name}");
        }
        #[cfg(target_os = "macos")]
        assert!(map_media_key("audio_stop").is_none());
        assert!(map_media_key("audio_rewind").is_none());
        assert_eq!(map_media_key("audio_mute"), Some(Key::VolumeMute));
        assert_eq!(map_media_key("audio_prev"), Some(Key::MediaPrevTrack));
        assert_eq!(map_media_key("audio_next"), Some(Key::MediaNextTrack));
    }

    #[test]
    fn key_event_resolution_matches_original_js() {
        assert!(matches!(
            resolve_key_event("a"),
            InputCommand::KeyPress {
                key: Key::Unicode('a')
            }
        ));
        assert!(matches!(
            resolve_key_event("5"),
            InputCommand::KeyPress {
                key: Key::Unicode('5')
            }
        ));
        assert!(matches!(
            resolve_key_event("A"),
            InputCommand::TypeText { .. }
        ));
        assert!(matches!(
            resolve_key_event("backspace"),
            InputCommand::KeyPress {
                key: Key::Backspace
            }
        ));
        assert!(matches!(
            resolve_key_event("Enter"),
            InputCommand::KeyPress { key: Key::Return }
        ));
    }

    #[test]
    fn worker_channel_roundtrip() {
        let (tx, _log) = spawn_input_worker();
        // May fail headless (no display for enigo); must not panic either way.
        let _ = tx.send(InputCommand::MouseMove { x: 0, y: 0 });
    }
}

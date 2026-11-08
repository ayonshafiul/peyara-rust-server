//! Desktop input backend: translates mobile-app commands into OS-level
//! mouse / keyboard events via `enigo`.
//!
//! All blocking `enigo` calls run on a dedicated worker thread so the Tokio
//! runtime and the egui UI thread are never blocked. Pure mapping helpers are
//! unit-tested (`cargo test`).

use enigo::{Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// How long the worker waits before retrying a dead backend on its own.
/// The UI Retry button bypasses this immediately.
const BACKEND_RETRY_GAP: Duration = Duration::from_secs(5);
/// Identical backend errors are logged at most this often (no UI log floods).
const ERROR_LOG_GAP: Duration = Duration::from_secs(2);
/// After this many consecutive failed auto-rebuilds the worker stops trying
/// on its own and waits for a manual retry (avoids prompt/beep loops when
/// the user denied access or the portal is gone).
const MAX_AUTO_REBUILDS: u32 = 5;

/// Commands sent by the mobile client (mirrors the WebSocket `type` messages
/// documented in `docs/websocket-client-migration.md`).
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
    /// Shortcut-deck action (`action` event).
    ///
    /// `hold` = how long to hold the main key before release (clamped
    /// 0..=5000ms at parse time). `repeats` = how many times to tap
    /// (clamped 1..=10). `code` is reserved for a future scancode/raw-input
    /// mode for games (v1 ignores it, see `docs/shortcut-deck.md`).
    Shortcut {
        key: Key,
        modifiers: Vec<Key>,
        hold: Duration,
        repeats: u32,
        /// Reserved for v2 scancode/raw-input (`code: "KeyW"`). v1 stores
        /// and ignores it.
        #[allow(dead_code)]
        code: Option<String>,
    },
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
/// Port of the `socket.on("key")` handler in `src/main.js`, with one
/// deliberate change: every single character is *typed*, never tapped.
/// Tapping `Key::Unicode(c)` makes enigo resolve the character through the
/// keyboard layout (TIS/`UCKeyTranslate` on macOS), and those HIToolbox APIs
/// enforce a main-thread-only queue assertion — called from our input worker
/// thread they kill the process outright (`dispatch_assert_queue_fail`,
/// EXC_BREAKPOINT). Typing posts a CGEvent carrying the unicode string
/// instead: thread-safe, layout-independent, and correct for uppercase and
/// non-ASCII too. `backspace`/`enter` and other named keys are still tapped —
/// they map to fixed keycodes, no layout lookup involved.
pub fn resolve_key_event(key: &str) -> InputCommand {
    if key.chars().count() == 1 {
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
///
/// On macOS a single character resolves to its ANSI virtual keycode
/// ([`ansi_keycode`]) instead of `Key::Unicode`: enigo maps `Key::Unicode`
/// through the main-thread-only TIS layout API, which traps the process when
/// invoked from the input worker thread. Unmappable characters fall back to
/// `Key::Unicode('\0')`, which enigo turns into a no-op.
pub fn resolve_edit_key(key: &str) -> Key {
    if key.chars().count() == 1 {
        let c = key.chars().next().unwrap();
        #[cfg(target_os = "macos")]
        {
            ansi_keycode(c)
                .map(|code| Key::Other(code as u32))
                .unwrap_or(Key::Unicode('\0'))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Key::Unicode(c)
        }
    } else {
        map_named_key(key).unwrap_or(Key::Unicode('\0'))
    }
}

/// ANSI virtual keycodes (HIToolbox `Events.h`) for printable ASCII, matched
/// case-insensitively. Shortcuts are defined by physical key on macOS, so this
/// works on any keyboard layout. Only used on the [`resolve_edit_key`] path.
#[cfg(target_os = "macos")]
fn ansi_keycode(c: char) -> Option<u16> {
    Some(match c.to_ascii_lowercase() {
        'a' => 0x00,
        's' => 0x01,
        'd' => 0x02,
        'f' => 0x03,
        'h' => 0x04,
        'g' => 0x05,
        'z' => 0x06,
        'x' => 0x07,
        'c' => 0x08,
        'v' => 0x09,
        'b' => 0x0B,
        'q' => 0x0C,
        'w' => 0x0D,
        'e' => 0x0E,
        'r' => 0x0F,
        'y' => 0x10,
        't' => 0x11,
        '1' => 0x12,
        '2' => 0x13,
        '3' => 0x14,
        '4' => 0x15,
        '6' => 0x16,
        '5' => 0x17,
        '=' => 0x18,
        '9' => 0x19,
        '7' => 0x1A,
        '-' => 0x1B,
        '8' => 0x1C,
        '0' => 0x1D,
        ']' => 0x1E,
        'o' => 0x1F,
        'u' => 0x20,
        '[' => 0x21,
        'i' => 0x22,
        'p' => 0x23,
        'l' => 0x25,
        'j' => 0x26,
        '\'' => 0x27,
        'k' => 0x28,
        ';' => 0x29,
        '\\' => 0x2A,
        ',' => 0x2B,
        '/' => 0x2C,
        'n' => 0x2D,
        'm' => 0x2E,
        '.' => 0x2F,
        '`' => 0x32,
        _ => return None,
    })
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

/// Protocol-level failure = the backend session itself is dead (portal or
/// compositor dropped us), as opposed to one bad command. Only then is a
/// rebuild worthwhile.
fn is_backend_dead(e: &enigo::InputError) -> bool {
    matches!(e, enigo::InputError::Simulate(_))
}

/// Pure throttle check: log `msg` if it differs from the last logged error
/// or enough time has passed. Unit-tested.
fn should_log_error(last: &Option<(String, Instant)>, msg: &str, now: Instant) -> bool {
    match last {
        None => true,
        Some((m, t)) => m != msg || now.duration_since(*t) >= ERROR_LOG_GAP,
    }
}

/// Pointer motion is re-emitted to the OS on this fixed tick (250 Hz)
/// instead of the moment a packet arrives.
const PACE_TICK: Duration = Duration::from_millis(4);
/// Fraction of the pending motion emitted per tick. 0.45 drains a burst to
/// ~10% within 16ms, for ~5ms of added average latency.
const PACE_ALPHA: f64 = 0.45;
/// Pending motion at or below this distance (px) is emitted in one go, so
/// a glide always settles exactly instead of decaying forever.
const PACE_SNAP: f64 = 1.0;
/// How long the worker sleeps between polls while no motion is pending.
const IDLE_WAIT: Duration = Duration::from_millis(500);

/// Smooths pointer motion between the network and the OS.
///
/// Wi-Fi delivers pointer packets in bursts (radio power-save, frame
/// aggregation), and applying each burst instantly makes the cursor jump.
/// The pacer collects motion and releases a fixed fraction of it every
/// [`PACE_TICK`], turning bursts into short glides and steady 60/120 Hz
/// phone input into even 250 Hz steps. Spacing the OS moves also matters on
/// macOS: enigo's relative move reads the cursor position back, and reads
/// taken between back-to-back events return a stale position that silently
/// drops motion. Both axes drain by the same fraction, so the direction of
/// travel is preserved; fractional pixels carry over and nothing is lost.
#[derive(Debug, Default)]
struct MotionPacer {
    pending: (f64, f64),
    remainder: (f64, f64),
}

impl MotionPacer {
    fn push(&mut self, dx: i32, dy: i32) {
        self.pending.0 += f64::from(dx);
        self.pending.1 += f64::from(dy);
    }

    fn is_idle(&self) -> bool {
        self.pending == (0.0, 0.0)
    }

    /// Whole-pixel motion to apply on this tick.
    fn step(&mut self) -> (i32, i32) {
        let (px, py) = self.pending;
        if px.hypot(py) <= PACE_SNAP {
            self.flush()
        } else {
            self.emit(px * PACE_ALPHA, py * PACE_ALPHA)
        }
    }

    /// Everything still pending, at once.
    fn flush(&mut self) -> (i32, i32) {
        let (px, py) = self.pending;
        self.emit(px, py)
    }

    fn emit(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        self.pending = (self.pending.0 - dx, self.pending.1 - dy);
        if self.pending.0.abs() < 1e-9 && self.pending.1.abs() < 1e-9 {
            self.pending = (0.0, 0.0);
        }
        let x = dx + self.remainder.0;
        let y = dy + self.remainder.1;
        let (qx, qy) = (x.round(), y.round());
        self.remainder = (x - qx, y - qy);
        (qx as i32, qy as i32)
    }
}

/// Owns the enigo backend and its failure/rebuild bookkeeping.
struct Worker {
    settings: Settings,
    session: &'static str,
    log_tx: Sender<String>,
    backend: Option<Enigo>,
    last_attempt: Instant,
    auto_failures: u32,
    last_err: Option<(String, Instant)>,
}

impl Worker {
    /// Log helper with flood protection.
    fn log_err(&mut self, msg: String) {
        let now = Instant::now();
        if should_log_error(&self.last_err, &msg, now) {
            self.last_err = Some((msg.clone(), now));
            let _ = self.log_tx.send(msg);
        }
    }

    /// Manual retry (UI button / permission granted): rebuild now.
    fn retry(&mut self) {
        if self.backend.is_none() {
            self.backend = init_backend(&self.settings, &self.log_tx, self.session);
            self.last_attempt = Instant::now();
            self.auto_failures = 0;
        }
    }

    fn run(&mut self, cmd: InputCommand) {
        match self.backend.as_mut() {
            Some(e) => match execute(e, cmd) {
                Ok(()) => {
                    self.auto_failures = 0;
                }
                Err(err) => {
                    if is_backend_dead(&err) {
                        self.log_err(format!("input backend lost ({err}); reconnecting…"));
                        // One immediate rebuild for transient blips;
                        // sustained failure falls through to the
                        // gated retries below.
                        self.backend = init_backend(&self.settings, &self.log_tx, self.session);
                        if self.backend.is_none() {
                            self.last_attempt = Instant::now();
                            self.auto_failures += 1;
                        } else {
                            self.auto_failures = 0;
                        }
                        if self.auto_failures >= MAX_AUTO_REBUILDS {
                            self.log_err(
                                "input backend keeps failing — fix the permission/portal, \
                                 then press Retry input backend."
                                    .to_owned(),
                            );
                        }
                    } else {
                        self.log_err(format!("input error: {err}"));
                    }
                }
            },
            None => {
                // Backend down: rebuild at most every BACKEND_RETRY_GAP
                // (and stop auto-retrying after repeated failures),
                // else drop this command to avoid stale-motion bursts.
                let may_retry = self.last_attempt.elapsed() >= BACKEND_RETRY_GAP
                    && self.auto_failures < MAX_AUTO_REBUILDS;
                if may_retry {
                    self.backend = init_backend(&self.settings, &self.log_tx, self.session);
                    self.last_attempt = Instant::now();
                    if self.backend.is_none() {
                        self.auto_failures += 1;
                    }
                }
                match self.backend.as_mut() {
                    Some(e) => {
                        if let Err(err) = execute(e, cmd) {
                            self.log_err(format!("input error: {err}"));
                            if is_backend_dead(&err) {
                                self.backend = None;
                                self.auto_failures += 1;
                            } else {
                                self.auto_failures = 0;
                            }
                        } else {
                            self.auto_failures = 0;
                        }
                    }
                    None => {
                        self.log_err("input dropped: backend unavailable".to_owned());
                    }
                }
            }
        }
    }
}

/// Spawn the dedicated input worker thread. Returns a channel sender for
/// [`InputCommand`]s, a log line receiver for UI diagnostics, and a retry
/// sender that forces an immediate backend rebuild (UI Retry button /
/// permission-granted poll).
///
/// On Wayland (e.g. Ubuntu 24's default GNOME session) X11 injection only
/// reaches XWayland windows, so enigo must go through the Remote Desktop
/// portal (libei). That pops up a one-time system permission prompt which
/// the user has to approve.
///
/// Unlike the first version, the worker never exits when the backend dies:
/// a dead portal session (compositor restart, idle expiry, suspend) is
/// detected via [`is_backend_dead`] and the backend is rebuilt with
/// backoff, so input recovers without an app restart.
///
/// Pointer motion goes through a [`MotionPacer`]; every other command first
/// flushes the pending motion so clicks, drag presses/releases and keys land
/// where the pointer visibly is.
pub fn spawn_input_worker() -> (Sender<InputCommand>, mpsc::Receiver<String>, Sender<()>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<InputCommand>();
    let (log_tx, log_rx) = mpsc::channel::<String>();
    let (retry_tx, retry_rx) = mpsc::channel::<()>();

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
        #[cfg(target_os = "macos")]
        let _ = log_tx.send(
            "macOS detected: Accessibility permission is required — check the \
             Permissions card and grant access."
                .to_owned(),
        );
        // Match the snappy delays of the original robotjs setup
        // (mouse 1ms, keyboard 2ms); only affects the X11 backend.
        let settings = Settings {
            linux_delay: 1,
            ..Settings::default()
        };
        let backend = init_backend(&settings, &log_tx, session);
        let mut worker = Worker {
            settings,
            session,
            log_tx,
            backend,
            last_attempt: Instant::now(),
            auto_failures: 0,
            last_err: None,
        };
        let mut pacer = MotionPacer::default();
        let mut next_tick = Instant::now();

        loop {
            if retry_rx.try_recv().is_ok() {
                while retry_rx.try_recv().is_ok() {}
                worker.retry();
            }
            let timeout = if pacer.is_idle() {
                IDLE_WAIT
            } else {
                next_tick.saturating_duration_since(Instant::now())
            };
            match cmd_rx.recv_timeout(timeout) {
                Ok(InputCommand::MouseMove { x, y } | InputCommand::DragMove { x, y }) => {
                    if pacer.is_idle() {
                        // Start of a motion: first step goes out right away.
                        next_tick = Instant::now();
                    }
                    pacer.push(x, y);
                }
                Ok(cmd) => {
                    let (x, y) = pacer.flush();
                    if x != 0 || y != 0 {
                        worker.run(InputCommand::MouseMove { x, y });
                    }
                    worker.run(cmd);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let now = Instant::now();
            if !pacer.is_idle() && now >= next_tick {
                let (x, y) = pacer.step();
                if x != 0 || y != 0 {
                    // MouseMove and DragMove execute identically: the OS
                    // turns a move with the button held into a drag.
                    worker.run(InputCommand::MouseMove { x, y });
                }
                // Fixed cadence; after a stall, restart rather than burst
                // to catch up.
                next_tick += PACE_TICK;
                if next_tick < now {
                    next_tick = now + PACE_TICK;
                }
            }
        }
    });

    (cmd_tx, log_rx, retry_tx)
}

/// Try to create the enigo backend, logging the outcome. `None` means the
/// worker must keep going without input until a retry succeeds.
fn init_backend(
    settings: &Settings,
    log_tx: &mpsc::Sender<String>,
    session: &str,
) -> Option<Enigo> {
    match Enigo::new(settings) {
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
    }
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
        InputCommand::Shortcut {
            key,
            modifiers,
            hold,
            repeats,
            ..
        } => {
            let taps = repeats.clamp(1, 10);
            let hold = hold.min(Duration::from_secs(5));
            for i in 0..taps {
                for m in &modifiers {
                    enigo.key(*m, Direction::Press)?;
                }
                if hold.is_zero() {
                    enigo.key(key, Direction::Click)?;
                } else {
                    enigo.key(key, Direction::Press)?;
                    std::thread::sleep(hold);
                    enigo.key(key, Direction::Release)?;
                }
                for m in modifiers.iter().rev() {
                    enigo.key(*m, Direction::Release)?;
                }
                if i + 1 < taps {
                    std::thread::sleep(Duration::from_millis(20));
                }
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
    fn key_event_resolution_types_single_chars() {
        // Single characters are typed (never layout-tapped): see the
        // resolve_key_event doc comment about the macOS TIS trap.
        assert!(matches!(
            resolve_key_event("a"),
            InputCommand::TypeText { .. }
        ));
        assert!(matches!(
            resolve_key_event("5"),
            InputCommand::TypeText { .. }
        ));
        assert!(matches!(
            resolve_key_event("A"),
            InputCommand::TypeText { .. }
        ));
        assert!(matches!(
            resolve_key_event("é"),
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
    fn edit_keys_resolve_without_tis_lookup() {
        // Named keys map to fixed keycodes everywhere.
        assert_eq!(resolve_edit_key("f5"), Key::F5);
        assert_eq!(resolve_edit_key("left"), Key::LeftArrow);
        // Unknown multi-char names no-op.
        assert_eq!(resolve_edit_key("nope"), Key::Unicode('\0'));
        // Single characters must not become Key::Unicode on macOS (TIS trap).
        #[cfg(target_os = "macos")]
        {
            assert_eq!(resolve_edit_key("c"), Key::Other(0x08));
            assert_eq!(resolve_edit_key("C"), Key::Other(0x08));
            assert_eq!(resolve_edit_key("a"), Key::Other(0x00));
            assert_eq!(resolve_edit_key("5"), Key::Other(0x17));
            // Non-ANSI characters no-op rather than hitting the layout API.
            assert_eq!(resolve_edit_key("é"), Key::Unicode('\0'));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(resolve_edit_key("c"), Key::Unicode('c'));
        }
    }

    #[test]
    fn backend_death_classification() {
        use enigo::InputError;
        assert!(is_backend_dead(&InputError::Simulate("protocol gone")));
        assert!(!is_backend_dead(&InputError::Mapping("x".to_owned())));
        assert!(!is_backend_dead(&InputError::Unmapping("x".to_owned())));
        assert!(!is_backend_dead(&InputError::NoEmptyKeycodes));
        assert!(!is_backend_dead(&InputError::InvalidInput("bad")));
    }

    #[test]
    fn error_log_throttle() {
        let now = Instant::now();
        assert!(should_log_error(&None, "x", now));
        let last = Some(("x".to_owned(), now));
        // Same message, immediately: suppressed.
        assert!(!should_log_error(&last, "x", now));
        // Different message: logged.
        assert!(should_log_error(&last, "y", now));
        // Same message after the gap: logged.
        assert!(should_log_error(&last, "x", now + ERROR_LOG_GAP));
    }

    /// Drain the pacer tick by tick; returns (total emitted, ticks taken).
    fn drain(p: &mut MotionPacer) -> ((i32, i32), usize) {
        let mut total = (0, 0);
        let mut ticks = 0;
        while !p.is_idle() {
            let (x, y) = p.step();
            total = (total.0 + x, total.1 + y);
            ticks += 1;
            assert!(ticks < 1000, "pacer never settled");
        }
        (total, ticks)
    }

    #[test]
    fn pacer_emits_all_motion_and_settles() {
        let mut p = MotionPacer::default();
        p.push(300, -120);
        let (total, ticks) = drain(&mut p);
        assert_eq!(total, (300, -120));
        // A large burst becomes a short glide, not a jump or a crawl.
        assert!((5..=20).contains(&ticks), "ticks = {ticks}");
    }

    #[test]
    fn pacer_spreads_a_burst() {
        let mut p = MotionPacer::default();
        p.push(100, 0);
        let (first, _) = p.step();
        assert!(first > 0 && first < 100, "first step = {first}");
    }

    #[test]
    fn pacer_small_motion_goes_out_at_once() {
        let mut p = MotionPacer::default();
        p.push(1, 0);
        assert_eq!(p.step(), (1, 0));
        assert!(p.is_idle());
    }

    #[test]
    fn pacer_keeps_direction() {
        let mut p = MotionPacer::default();
        p.push(200, 100);
        while !p.is_idle() {
            let (x, y) = p.step();
            // Both axes drain together: x stays ~2× y (±1px rounding).
            assert!((x - 2 * y).abs() <= 2, "step ({x}, {y})");
        }
    }

    #[test]
    fn pacer_accumulates_streamed_input_exactly() {
        let mut p = MotionPacer::default();
        let mut total = (0, 0);
        for _ in 0..50 {
            p.push(3, -1);
            let (x, y) = p.step();
            total = (total.0 + x, total.1 + y);
        }
        let (rest, _) = drain(&mut p);
        assert_eq!((total.0 + rest.0, total.1 + rest.1), (150, -50));
    }

    #[test]
    fn pacer_flush_empties() {
        let mut p = MotionPacer::default();
        p.push(57, 9);
        p.step();
        let _ = p.flush();
        assert!(p.is_idle());
    }

    #[test]
    fn worker_channel_roundtrip() {
        let (tx, _log, _retry) = spawn_input_worker();
        // May fail headless (no display for enigo); must not panic either way.
        let _ = tx.send(InputCommand::MouseMove { x: 0, y: 0 });
    }
}

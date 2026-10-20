//! Network server: HTTPS + secure WebSocket (`wss`) on port 1313.
//!
//! TLS-only: there is no plaintext listener. The certificate is ephemeral
//! (generated per launch, regenerable from the UI) and the phone pins it via
//! the fingerprint in the QR code — see `crate::tls` and
//! `docs/qr-code-format.md`.
//!
//! Every connection must then pair: its first message must be
//! `{"type":"hello","pair":"<8hex>"}` echoing the pairing code from the QR.
//! Anything else (or three bad codes, or 10s of silence) closes the
//! connection before it can send input or receive clipboard text.
//!
//! Wire protocol (all frames are UTF-8 JSON text over `wss://<host>:1313/ws`):
//!
//! * `GET https://<host>:1313/` returns JSON `"peyara"` (discovery / QR verify).
//! * Phone -> server: first `hello`, then `{"type": "<event>", ...}` where
//!   `<event>` is one of `coordinates`, `clicks`, `scroll`,
//!   `windowdragstart`, `windowdragupdate`, `windowdragend`, `key`,
//!   `media-key`, `edit-key`, `text`, `get-active-app`, `action` (see
//!   `docs/shortcut-deck.md` for the last two).
//! * Server -> phone: `{"type": "text-mobile", "text": "..."}` and
//!   `{"type": "active-app", "app": ..., "slug": ..., ...}`.
//!
//! Screen-sharing signalling (`offer`/`answer`/ICE) from the original is
//! intentionally omitted.

use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Json,
};
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::Sender,
    Arc, Mutex, RwLock,
};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

use crate::active_app;
use crate::input::{
    map_button, map_media_key, map_modifier, resolve_edit_key, resolve_key_event, InputCommand,
};

/// Events forwarded from the server thread to the egui UI thread.
#[derive(Debug)]
pub enum UiEvent {
    /// Text received from the phone (`{"type":"text",...}`).
    IncomingText(String),
    /// Diagnostic line for the on-screen log.
    Log(String),
}

/// State shared between the server tasks and the UI thread.
pub struct Shared {
    /// Currently connected (paired) WebSocket clients.
    pub clients: AtomicUsize,
    /// Channel into the enigo input worker thread.
    pub input_tx: Sender<InputCommand>,
    /// Channel into the egui UI thread.
    pub ui_tx: Sender<UiEvent>,
    /// Fan-out for desktop -> phone text (`text-mobile`).
    pub mobile_tx: broadcast::Sender<String>,
    /// Current 8-digit hex pairing code (QR `pair:` field). Rotated on
    /// launch and every regeneration.
    pub pairing: RwLock<String>,
    /// Cumulative rejected pairing attempts (brute-force visibility).
    pub rejected: AtomicUsize,
    /// Rotation generation. Every session subscribes; `ServerCtl` bumps it
    /// on regenerate so live sessions close themselves promptly. (Aborting
    /// the axum-server task only drops the listener — established sessions
    /// are detached tasks and would otherwise linger.)
    pub generation: watch::Sender<u64>,
}

impl Shared {
    pub fn log(&self, line: impl Into<String>) {
        let _ = self.ui_tx.send(UiEvent::Log(line.into()));
    }

    /// Broadcast text to all connected phones.
    /// Returns the number of receivers that (may) have gotten it.
    pub fn send_to_phone(&self, text: &str) -> usize {
        self.mobile_tx.send(text.to_owned()).unwrap_or(0)
    }
}

/// Per-phone sub-pixel remainders.
///
/// Pointer deltas arrive as floats but `enigo` consumes whole pixels. Rounding
/// every packet would lose up to 0.5px per packet, making slow movement stall
/// then jump. Instead each connection keeps its fractional leftover and adds
/// it to the next packet, so no motion is ever lost. Mouse and scroll keep
/// separate remainders because their units differ.
#[derive(Debug, Default)]
struct Remainders {
    mouse: Mutex<(f64, f64)>,
    scroll: Mutex<(f64, f64)>,
}

/// Add a float delta to its remainder, return the whole-pixel part to apply.
fn quantize(remainder: &Mutex<(f64, f64)>, dx: f64, dy: f64) -> (i32, i32) {
    let mut rem = remainder.lock().unwrap_or_else(|e| e.into_inner());
    let x = dx + rem.0;
    let y = dy + rem.1;
    let qx = x.round() as i32;
    let qy = y.round() as i32;
    *rem = (x - qx as f64, y - qy as f64);
    (qx, qy)
}

/// Build the router. Desktop -> phone text flows through the broadcast
/// channel on [`Shared::mobile_tx`], so no socket handle needs to leave here.
pub fn build(shared: Arc<Shared>) -> axum::Router {
    axum::Router::new()
        .route("/", get(root))
        .route("/ws", get(ws_handler))
        .with_state(shared)
}

async fn ws_handler(ws: WebSocketUpgrade, State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, shared))
}

/// Pairing handshake: the socket must open with
/// `{"type":"hello","pair":"<8hex>"}` matching the current QR code.
/// Returns false on timeout, close, error, or 3 bad codes.
async fn authenticate<S, E>(receiver: &mut S, shared: &Shared, timeout: Duration) -> bool
where
    S: Stream<Item = Result<Message, E>> + Unpin,
{
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut attempts = 0u8;
    loop {
        tokio::select! {
            _ = &mut deadline => {
                shared.log("ws: pairing hello timeout, closing");
                return false;
            }
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if is_valid_hello(&text, shared) {
                            return true;
                        }
                        attempts += 1;
                        let n = shared.rejected.fetch_add(1, Ordering::SeqCst) + 1;
                        shared.log("ws: rejected bad pairing code");
                        if n % 20 == 0 {
                            shared.log(format!(
                                "ws: {n} rejected pairing attempts total — \
                                 unknown device may be probing port 1313"
                            ));
                        }
                        if attempts >= 3 {
                            return false;
                        }
                    }
                    // Close / end / error during handshake: gone.
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return false,
                    // Binary / ping / pong during handshake: ignore.
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Check one candidate `hello` frame against the current pairing code.
fn is_valid_hello(text: &str, shared: &Shared) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    if v.get("type").and_then(Value::as_str) != Some("hello") {
        return false;
    }
    let Some(code) = v.get("pair").and_then(Value::as_str) else {
        return false;
    };
    let expected = shared
        .pairing
        .read()
        .map(|s| s.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    codes_equal(code, &expected)
}

/// Constant-time string equality (lengths are fixed and public, but the
/// code itself must not leak through early exit).
fn codes_equal(a: &str, b: &str) -> bool {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    if x.len() != y.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..x.len() {
        diff |= x[i] ^ y[i];
    }
    diff == 0
}

async fn handle_socket(socket: WebSocket, shared: Arc<Shared>) {
    let (mut sender, mut receiver) = socket.split();

    // Pairing gate: the first message must echo the QR pairing code.
    // Unpaired sockets are closed before they can send input or receive
    // clipboard broadcasts.
    if !authenticate(&mut receiver, &shared, Duration::from_secs(10)).await {
        let _ = sender.send(Message::Close(None)).await;
        return;
    }

    shared.clients.fetch_add(1, Ordering::SeqCst);
    shared.log("phone paired (wss)");
    let remainders = Remainders::default();
    let mut rx = shared.mobile_tx.subscribe();
    let mut gen_rx = shared.generation.subscribe();

    // Replies for request/response messages (`get-active-app`). The main loop
    // pushes JSON strings here; the forward task owns the socket sender.
    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Auto-push active-app once after connection open so the phone can pick
    // the right shortcut deck without an extra round-trip.
    let initial = active_app::payload_or_unknown().to_string();
    let _ = reply_tx.send(initial);

    // Forward broadcast text-mobile + direct replies to this phone. Runs until
    // either the phone disconnects (sender closed), pairing rotates, or the
    // task is aborted below.
    let gen_kick = shared.generation.subscribe();
    let forward = tokio::spawn(async move {
        let mut gen_kick = gen_kick;
        loop {
            tokio::select! {
                text = rx.recv() => {
                    let Ok(text) = text else { break };
                    let payload = serde_json::json!({"type": "text-mobile", "text": text});
                    if sender
                        .send(Message::Text(payload.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                reply = reply_rx.recv() => {
                    let Some(reply) = reply else { break };
                    if sender
                        .send(Message::Text(reply.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                _ = gen_kick.changed() => {
                    // Pairing rotated: end the session politely.
                    let _ = sender.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    });

    let mut kicked = false;
    loop {
        tokio::select! {
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Value>(&text) {
                            Ok(v) => {
                                if let Some(reply) = dispatch(&v, &shared, &remainders) {
                                    let _ = reply_tx.send(reply.to_string());
                                }
                            }
                            Err(_) => shared.log(format!("ws: ignoring non-JSON frame: {text}")),
                        }
                    }
                    // Close / end / error: gone.
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    // Binary / ping / pong: nothing to do.
                    Some(Ok(_)) => {}
                }
            }
            _ = gen_rx.changed() => {
                // Pairing rotated (regenerate button / network change):
                // the forward task delivers the Close frame; just exit.
                // (Dropping our halves would also kill the TCP connection.)
                shared.log("phone session ended by certificate regeneration");
                kicked = true;
                break;
            }
        }
    }

    if kicked {
        // Give the forward task a moment to deliver the Close frame
        // instead of racing it with an abort.
        let _ = tokio::time::timeout(Duration::from_secs(2), forward).await;
    } else {
        forward.abort();
    }
    // Saturating: a regeneration reset may already have zeroed the counter.
    #[allow(deprecated)]
    let _ = shared
        .clients
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
            Some(c.saturating_sub(1))
        });
    shared.log("phone disconnected (ws)");
}

/// Route one decoded client JSON message to the input/UI channels.
///
/// Returns an optional server->phone JSON reply (for `get-active-app`).
/// `action` is fire-and-forget (returns `None`) to avoid spamming the phone.
fn dispatch(v: &Value, shared: &Shared, remainders: &Remainders) -> Option<Value> {
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "coordinates" => {
            let (qx, qy) = quantize(&remainders.mouse, numf(v, "x"), numf(v, "y"));
            if qx != 0 || qy != 0 {
                let _ = shared
                    .input_tx
                    .send(InputCommand::MouseMove { x: qx, y: qy });
            }
        }
        "clicks" => {
            let finger = v.get("finger").and_then(Value::as_str).unwrap_or("left");
            let double = v.get("doubleTap").and_then(Value::as_bool).unwrap_or(false);
            let _ = shared.input_tx.send(InputCommand::Click {
                button: map_button(finger),
                double,
            });
        }
        "scroll" => {
            let (qx, qy) = quantize(&remainders.scroll, numf(v, "x"), numf(v, "y"));
            if qx != 0 || qy != 0 {
                let _ = shared.input_tx.send(InputCommand::Scroll { x: qx, y: qy });
            }
        }
        "windowdragstart" => {
            let _ = shared.input_tx.send(InputCommand::DragStart);
        }
        "windowdragupdate" => {
            let (qx, qy) = quantize(&remainders.mouse, numf(v, "x"), numf(v, "y"));
            if qx != 0 || qy != 0 {
                let _ = shared
                    .input_tx
                    .send(InputCommand::DragMove { x: qx, y: qy });
            }
        }
        "windowdragend" => {
            let _ = shared.input_tx.send(InputCommand::DragEnd);
        }
        "key" => {
            if let Some(key) = str_field(v, &["key", "data", "text"]) {
                let _ = shared.input_tx.send(resolve_key_event(&key));
            }
        }
        "media-key" => {
            if let Some(name) = str_field(v, &["key", "name", "data"]) {
                if let Some(key) = map_media_key(&name) {
                    let _ = shared.input_tx.send(InputCommand::MediaKey { key });
                }
            }
        }
        "edit-key" => {
            let key_name = str_field(v, &["key"])?;
            if key_name.is_empty() {
                return None;
            }
            let modifiers = parse_modifiers(v);
            let _ = shared.input_tx.send(InputCommand::EditKey {
                key: resolve_edit_key(&key_name),
                modifiers,
            });
        }
        "text" => {
            if let Some(text) = str_field(v, &["text", "data"]) {
                let _ = shared.ui_tx.send(UiEvent::IncomingText(text));
            }
        }
        "get-active-app" => {
            return Some(active_app::payload_or_unknown());
        }
        "action" => {
            if let Some(cmd) = parse_action(v, shared) {
                let _ = shared.input_tx.send(cmd);
            }
        }
        "" => {
            shared.log("ws: ignoring message without \"type\"");
        }
        other => {
            shared.log(format!("ws: ignoring unknown type \"{other}\""));
        }
    }
    None
}

/// Parse `modifier` (string) or `modifiers` (array) from edit-key/action payloads.
fn parse_modifiers(v: &Value) -> Vec<enigo::Key> {
    let mut modifiers = Vec::new();
    // Singular `modifier: "ctrl"` (legacy edit-key shape).
    if let Some(Value::String(m)) = v.get("modifier") {
        if let Some(k) = map_modifier(m) {
            modifiers.push(k);
        }
    }
    // Plural `modifiers: ["ctrl","shift"]` (preferred for action).
    if let Some(list) = v.get("modifiers").and_then(Value::as_array) {
        for m in list.iter().filter_map(Value::as_str) {
            if let Some(k) = map_modifier(m) {
                modifiers.push(k);
            }
        }
    }
    // Legacy `modifier: ["ctrl"]` array shape.
    if let Some(list) = v.get("modifier").and_then(Value::as_array) {
        for m in list.iter().filter_map(Value::as_str) {
            if let Some(k) = map_modifier(m) {
                modifiers.push(k);
            }
        }
    }
    modifiers
}

/// Parse an `action` (shortcut-deck) payload into an [`InputCommand::Shortcut`].
///
/// Schema (see `docs/shortcut-deck.md`):
/// `{"type":"action","key":"t","modifiers":["ctrl"],"pressDurationMs":50,
///   "pressCount":1,"actionId":"...","code":"KeyW"}`.
/// `code` is reserved for future raw-input/scancode mode (v1 ignores it).
fn parse_action(v: &Value, shared: &Shared) -> Option<InputCommand> {
    let key_name = str_field(v, &["key"])?;
    if key_name.is_empty() {
        return None;
    }
    let key = resolve_edit_key(&key_name);
    if key == enigo::Key::Unicode('\0') {
        shared.log(format!(
            "ws: ignoring action with unknown key \"{key_name}\""
        ));
        return None;
    }
    let modifiers = parse_modifiers(v);
    let hold_ms = v
        .get("pressDurationMs")
        .and_then(|n| n.as_u64().or_else(|| n.as_i64().map(|i| i.max(0) as u64)))
        .unwrap_or(0)
        .min(5000);
    let repeats = v
        .get("pressCount")
        .and_then(|n| n.as_u64().or_else(|| n.as_i64().map(|i| i.max(0) as u64)))
        .unwrap_or(1)
        .clamp(1, 10) as u32;
    let code = v.get("code").and_then(Value::as_str).map(|s| s.to_owned());
    if code.is_some() {
        shared.log("ws: action `code` is reserved for v2 raw-input; ignoring");
    }
    if let Some(id) = v.get("actionId").and_then(Value::as_str) {
        shared.log(format!("ws: action \"{id}\""));
    }
    Some(InputCommand::Shortcut {
        key,
        modifiers,
        hold: std::time::Duration::from_millis(hold_ms),
        repeats,
        code,
    })
}

/// First present string field among `keys` (accepts aliases for tolerance).
fn str_field(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(Value::as_str) {
            return Some(s.to_owned());
        }
    }
    None
}

fn numf(v: &Value, key: &str) -> f64 {
    v.get(key)
        .and_then(|n| n.as_f64().or_else(|| n.as_i64().map(|i| i as f64)))
        .unwrap_or(0.0)
}

async fn root() -> impl IntoResponse {
    Json(SERVER_RESPONSE)
}

const SERVER_RESPONSE: &str = "peyara";

// NOTE: serving is TLS-only via `crate::tls::serve_tls`; there is
// intentionally no plaintext `serve()` here.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    fn test_shared() -> Arc<Shared> {
        let (input_tx, _) = std::sync::mpsc::channel::<InputCommand>();
        let (ui_tx, _) = std::sync::mpsc::channel::<UiEvent>();
        let (mobile_tx, _) = broadcast::channel::<String>(16);
        let (generation, _) = watch::channel(0u64);
        Arc::new(Shared {
            clients: AtomicUsize::new(0),
            input_tx,
            ui_tx,
            mobile_tx,
            pairing: RwLock::new("a1b2c3d4".to_owned()),
            rejected: AtomicUsize::new(0),
            generation,
        })
    }

    fn test_shared_with_rx() -> (
        Arc<Shared>,
        std::sync::mpsc::Receiver<InputCommand>,
        std::sync::mpsc::Receiver<UiEvent>,
    ) {
        let (input_tx, input_rx) = std::sync::mpsc::channel::<InputCommand>();
        let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (mobile_tx, _) = broadcast::channel::<String>(16);
        let (generation, _) = watch::channel(0u64);
        (
            Arc::new(Shared {
                clients: AtomicUsize::new(0),
                input_tx,
                ui_tx,
                mobile_tx,
                pairing: RwLock::new("a1b2c3d4".to_owned()),
                rejected: AtomicUsize::new(0),
                generation,
            }),
            input_rx,
            ui_rx,
        )
    }

    fn http_roundtrip(port: u16, request: &[u8]) -> String {
        // The axum task needs a moment to start accepting; retry briefly.
        // Blocking IO runs off the async runtime to avoid stalling it.
        let request = request.to_vec();
        tokio::task::block_in_place(|| {
            let mut last_err = String::new();
            for _ in 0..50 {
                match TcpStream::connect(("127.0.0.1", port)) {
                    Ok(mut stream) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        stream.write_all(&request).unwrap();
                        let mut resp = String::new();
                        stream.read_to_string(&mut resp).unwrap();
                        return resp;
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            panic!("could not connect to test server: {last_err}");
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn discovery_roundtrip() {
        let app = build(test_shared());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });

        // GET / must return JSON "peyara" (mobile discovery handshake).
        let resp = http_roundtrip(port, b"GET / HTTP/1.0\r\nHost: x\r\n\r\n");
        assert!(resp.contains("\"peyara\""), "{resp}");

        // removed endpoint stays gone
        let resp = http_roundtrip(
            port,
            b"POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert!(resp.contains("404"), "{resp}");
    }

    #[test]
    fn quantize_keeps_no_motion_lossless() {
        // Ten 0.4px steps must eventually move 4px, not 0.
        let rem = Mutex::new((0.0, 0.0));
        let mut total = (0, 0);
        for _ in 0..10 {
            let (qx, qy) = quantize(&rem, 0.4, 0.0);
            total.0 += qx;
            total.1 += qy;
        }
        assert_eq!(total, (4, 0));
    }

    #[test]
    fn quantize_rounds_normal_deltas() {
        let rem = Mutex::new((0.0, 0.0));
        assert_eq!(quantize(&rem, 3.2, -1.7), (3, -2));
        assert_eq!(quantize(&rem, 0.0, 0.0), (0, 0));
    }

    #[test]
    fn numf_reads_ints_and_floats() {
        let v: Value = serde_json::json!({"x": 2.5, "y": 3});
        assert_eq!(numf(&v, "x"), 2.5);
        assert_eq!(numf(&v, "y"), 3.0);
        assert_eq!(numf(&v, "missing"), 0.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn websocket_route_replaces_socket_io() {
        let app = build(test_shared());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });

        // Plain HTTP on /ws (no Upgrade header) must not 404: axum replies
        // 426 Upgrade Required, proving the WS route exists.
        let resp = http_roundtrip(
            port,
            b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        );
        assert!(
            resp.contains("426") || resp.contains("400"),
            "expected upgrade-required, got: {resp}"
        );

        // Old socket.io endpoint is gone.
        let resp = http_roundtrip(
            port,
            b"GET /socket.io/?EIO=4&transport=polling HTTP/1.0\r\nHost: x\r\n\r\n",
        );
        assert!(resp.contains("404"), "{resp}");
    }

    #[test]
    fn dispatch_routes_all_message_types() {
        let (shared, input_rx, ui_rx) = test_shared_with_rx();
        let rem = Remainders::default();

        let _ = dispatch(
            &serde_json::json!({"type": "coordinates", "x": 3.2, "y": 0.0}),
            &shared,
            &rem,
        );
        assert!(matches!(
            input_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            InputCommand::MouseMove { x: 3, y: 0 }
        ));

        let _ = dispatch(
            &serde_json::json!({"type": "clicks", "finger": "right", "doubleTap": true}),
            &shared,
            &rem,
        );
        assert!(matches!(
            input_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            InputCommand::Click { double: true, .. }
        ));

        let _ = dispatch(
            &serde_json::json!({"type": "scroll", "x": 0, "y": 2.0}),
            &shared,
            &rem,
        );
        assert!(matches!(
            input_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            InputCommand::Scroll { .. }
        ));

        for msg in [
            serde_json::json!({"type": "windowdragstart"}),
            serde_json::json!({"type": "windowdragupdate", "x": 1.0, "y": 1.0}),
            serde_json::json!({"type": "windowdragend"}),
            serde_json::json!({"type": "key", "key": "a"}),
            serde_json::json!({"type": "media-key", "key": "audio_mute"}),
            serde_json::json!({"type": "edit-key", "key": "c", "modifier": "ctrl"}),
        ] {
            let _ = dispatch(&msg, &shared, &rem);
        }
        // 3 drag/key/media/edit-key inputs above produce 6 commands total
        // (dragstart, dragmove, dragend, key, mediakey, editkey); 3 consumed
        // so far, so 3 more input commands remain before the text check.
        for _ in 0..6 {
            let _ = input_rx.recv_timeout(Duration::from_secs(1));
        }

        let _ = dispatch(
            &serde_json::json!({"type": "text", "text": "hello"}),
            &shared,
            &rem,
        );
        assert!(matches!(
            ui_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            UiEvent::IncomingText(t) if t == "hello"
        ));

        // Unknown types and missing types are ignored (no new commands).
        let _ = dispatch(&serde_json::json!({"type": "nope"}), &shared, &rem);
        let _ = dispatch(&serde_json::json!({"x": 1}), &shared, &rem);
        assert!(input_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn dispatch_get_active_app_replies() {
        let (shared, _, _) = test_shared_with_rx();
        let rem = Remainders::default();
        let reply = dispatch(
            &serde_json::json!({"type": "get-active-app"}),
            &shared,
            &rem,
        );
        let v = reply.expect("get-active-app must reply");
        assert_eq!(v.get("type").and_then(Value::as_str), Some("active-app"));
        assert!(v.get("app").and_then(Value::as_str).is_some());
        assert!(v.get("slug").and_then(Value::as_str).is_some());
        assert!(v.get("os").and_then(Value::as_str).is_some());
    }

    #[test]
    fn dispatch_action_parses_modifiers_timing() {
        let (shared, input_rx, _) = test_shared_with_rx();
        let rem = Remainders::default();
        let reply = dispatch(
            &serde_json::json!({
                "type": "action",
                "actionId": "chrome-new-tab",
                "key": "t",
                "modifiers": ["ctrl"],
                "pressDurationMs": 120,
                "pressCount": 2,
            }),
            &shared,
            &rem,
        );
        // Fire-and-forget: no reply frame.
        assert!(reply.is_none());
        match input_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            InputCommand::Shortcut {
                hold,
                repeats,
                code,
                ..
            } => {
                assert_eq!(hold, std::time::Duration::from_millis(120));
                assert_eq!(repeats, 2);
                assert_eq!(code, None);
            }
            other => panic!("expected Shortcut, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_action_clamps_and_tolerates_shapes() {
        let (shared, input_rx, _) = test_shared_with_rx();
        let rem = Remainders::default();
        // Legacy singular `modifier` + oversized timing values clamp.
        let _ = dispatch(
            &serde_json::json!({
                "type": "action",
                "key": "s",
                "modifier": "ctrl",
                "pressDurationMs": 99999,
                "pressCount": 99,
                "code": "KeyS",
            }),
            &shared,
            &rem,
        );
        match input_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            InputCommand::Shortcut {
                hold,
                repeats,
                code,
                modifiers,
                ..
            } => {
                assert_eq!(hold, std::time::Duration::from_millis(5000));
                assert_eq!(repeats, 10);
                assert_eq!(code.as_deref(), Some("KeyS"));
                assert_eq!(modifiers.len(), 1);
            }
            other => panic!("expected Shortcut, got {other:?}"),
        }
        // Unknown key -> ignored, no command.
        let _ = dispatch(
            &serde_json::json!({"type": "action", "key": "definitely-not-a-key-xyz"}),
            &shared,
            &rem,
        );
        assert!(input_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_to_phone_fanout() {
        let (shared, _, _) = test_shared_with_rx();
        let mut rx = shared.mobile_tx.subscribe();
        assert_eq!(shared.send_to_phone("hi"), 1);
        assert_eq!(rx.recv().await.unwrap(), "hi");
    }

    #[test]
    fn codes_equal_is_exact() {
        assert!(codes_equal("a1b2c3d4", "a1b2c3d4"));
        assert!(!codes_equal("a1b2c3d4", "a1b2c3d5"));
        assert!(!codes_equal("a1b2c3d4", "a1b2c3d"));
        assert!(!codes_equal("a1b2c3d4", "a1b2c3d44"));
        assert!(!codes_equal("", "a1b2c3d4"));
    }

    #[test]
    fn hello_validation() {
        let (shared, _, _) = test_shared_with_rx(); // pairing = a1b2c3d4
        assert!(is_valid_hello(
            r#"{"type":"hello","pair":"a1b2c3d4"}"#,
            &shared
        ));
        assert!(!is_valid_hello(
            r#"{"type":"hello","pair":"00000000"}"#,
            &shared
        ));
        assert!(!is_valid_hello(r#"{"type":"hello"}"#, &shared));
        assert!(!is_valid_hello(r#"{"type":"coordinates","x":1}"#, &shared));
        assert!(!is_valid_hello("not json", &shared));
        // Rotation invalidates the old code.
        *shared.pairing.write().unwrap() = "ffffffff".to_owned();
        assert!(!is_valid_hello(
            r#"{"type":"hello","pair":"a1b2c3d4"}"#,
            &shared
        ));
        assert!(is_valid_hello(
            r#"{"type":"hello","pair":"ffffffff"}"#,
            &shared
        ));
    }

    fn hello_stream(msgs: Vec<&str>) -> impl Stream<Item = Result<Message, axum::Error>> + Unpin {
        futures_util::stream::iter(
            msgs.into_iter()
                .map(|m| Ok(Message::Text(m.to_owned().into())))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auth_accepts_correct_hello_first_try() {
        let (shared, _, _) = test_shared_with_rx();
        let mut rx = hello_stream(vec![r#"{"type":"hello","pair":"a1b2c3d4"}"#]);
        assert!(authenticate(&mut rx, &shared, Duration::from_secs(5)).await);
        assert_eq!(shared.rejected.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auth_tolerates_one_strike_then_hello() {
        let (shared, _, _) = test_shared_with_rx();
        let mut rx = hello_stream(vec![
            r#"{"type":"coordinates","x":1,"y":1}"#,
            r#"{"type":"hello","pair":"a1b2c3d4"}"#,
        ]);
        assert!(authenticate(&mut rx, &shared, Duration::from_secs(5)).await);
        assert_eq!(shared.rejected.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auth_closes_after_three_bad_codes() {
        let (shared, _, _) = test_shared_with_rx();
        let mut rx = hello_stream(vec![
            r#"{"type":"hello","pair":"00000000"}"#,
            r#"{"type":"hello","pair":"11111111"}"#,
            r#"{"type":"hello","pair":"22222222"}"#,
            r#"{"type":"hello","pair":"a1b2c3d4"}"#, // never reached
        ]);
        assert!(!authenticate(&mut rx, &shared, Duration::from_secs(5)).await);
        assert_eq!(shared.rejected.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auth_times_out_on_silence() {
        let (shared, _, _) = test_shared_with_rx();
        let mut rx = futures_util::stream::pending::<Result<Message, axum::Error>>();
        assert!(
            !authenticate(&mut rx, &shared, Duration::from_millis(50)).await,
            "silent socket must be closed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auth_rejects_immediate_close() {
        let (shared, _, _) = test_shared_with_rx();
        let mut rx =
            futures_util::stream::iter(vec![Ok::<Message, axum::Error>(Message::Close(None))]);
        assert!(!authenticate(&mut rx, &shared, Duration::from_secs(5)).await);
    }

    // --- Raw-socket end-to-end: pair a live session, then kick it --------

    /// Minimal WS client frame: masked text (client frames MUST be masked).
    fn ws_masked_text(payload: &str) -> Vec<u8> {
        let bytes = payload.as_bytes();
        assert!(bytes.len() < 126, "test payload too large");
        let mask = [0x11u8, 0x22, 0x33, 0x44];
        let mut frame = vec![0x81, 0x80 | bytes.len() as u8];
        frame.extend_from_slice(&mask);
        frame.extend(bytes.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        frame
    }

    /// Read one server→client frame (unmasked). Returns (opcode, payload).
    async fn ws_read_frame(s: &mut tokio::net::TcpStream) -> Option<(u8, Vec<u8>)> {
        use tokio::io::AsyncReadExt;
        let mut hdr = [0u8; 2];
        tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut hdr))
            .await
            .ok()?
            .ok()?;
        let opcode = hdr[0] & 0x0F;
        let mut len = (hdr[1] & 0x7F) as u64;
        if len == 126 {
            let mut ext = [0u8; 2];
            s.read_exact(&mut ext).await.ok()?;
            len = u16::from_be_bytes(ext) as u64;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            s.read_exact(&mut ext).await.ok()?;
            len = u64::from_be_bytes(ext);
        }
        let mut payload = vec![0u8; len as usize];
        if len > 0 {
            s.read_exact(&mut payload).await.ok()?;
        }
        Some((opcode, payload))
    }

    /// Raw TCP + RFC 6455 handshake against `/ws`. Returns the connected stream.
    async fn ws_raw_connect(port: u16) -> tokio::net::TcpStream {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        s.write_all(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = s.read(&mut tmp).await.unwrap();
            assert!(n > 0, "handshake: server closed early");
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf);
        assert!(head.contains("101"), "handshake failed: {head}");
        s
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generation_bump_kicks_live_session() {
        use tokio::io::AsyncWriteExt;
        let (shared, input_rx, _) = test_shared_with_rx(); // pairing = a1b2c3d4
        let app = build(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });

        // Pair over a raw socket.
        let mut s = ws_raw_connect(port).await;
        s.write_all(&ws_masked_text(r#"{"type":"hello","pair":"a1b2c3d4"}"#))
            .await
            .unwrap();
        // Prove the session is live: input arrives server-side.
        s.write_all(&ws_masked_text(r#"{"type":"coordinates","x":5,"y":0}"#))
            .await
            .unwrap();
        assert!(matches!(
            input_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            InputCommand::MouseMove { x: 5, y: 0 }
        ));
        // Wait until the server counted us, so the kick assertion is tight.
        for _ in 0..50 {
            if shared.clients.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(shared.clients.load(Ordering::SeqCst), 1);

        // Drain the auto-pushed active-app frame (opcode 0x1) sent right
        // after hello, so the kick assertion below sees the Close frame.
        let (opcode, payload) = ws_read_frame(&mut s).await.expect("no active-app frame");
        assert_eq!(opcode, 0x1, "expected initial active-app text frame");
        let v: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(v.get("type").and_then(Value::as_str), Some("active-app"));

        // Rotate (what ServerCtl::regenerate does): sessions must be kicked.
        let _ = shared.generation.send(1);

        // The server must deliver a Close frame (0x8), not linger.
        let (opcode, _) = ws_read_frame(&mut s).await.expect("no frame after kick");
        assert_eq!(opcode, 0x8, "expected Close frame after generation bump");

        // And the client counter must drain back to zero (no underflow).
        for _ in 0..50 {
            if shared.clients.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(shared.clients.load(Ordering::SeqCst), 0);
    }
}

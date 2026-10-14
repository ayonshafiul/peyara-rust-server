//! Network server: HTTP + native WebSocket on port 1313.
//!
//! Wire protocol (all frames are UTF-8 JSON text over `GET /ws`):
//!
//! * `GET /` returns JSON `"peyara"` (server discovery / QR verification).
//! * Phone -> server: `{"type": "<event>", ...}` where `<event>` is one of
//!   `coordinates`, `clicks`, `scroll`, `windowdragstart`,
//!   `windowdragupdate`, `windowdragend`, `key`, `media-key`, `edit-key`,
//!   `text` (see `docs/websocket-client-migration.md` for shapes).
//! * Server -> phone: `{"type": "text-mobile", "text": "..."}`.
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
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::Sender,
    Arc, Mutex,
};
use tokio::sync::broadcast;

use crate::{
    input::{
        map_button, map_media_key, map_modifier, resolve_edit_key, resolve_key_event, InputCommand,
    },
    netinfo::PORT,
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
    /// Currently connected WebSocket clients.
    pub clients: AtomicUsize,
    /// Channel into the enigo input worker thread.
    pub input_tx: Sender<InputCommand>,
    /// Channel into the egui UI thread.
    pub ui_tx: Sender<UiEvent>,
    /// Fan-out for desktop -> phone text (`text-mobile`).
    pub mobile_tx: broadcast::Sender<String>,
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

async fn handle_socket(socket: WebSocket, shared: Arc<Shared>) {
    shared.clients.fetch_add(1, Ordering::SeqCst);
    shared.log("phone connected (ws)");
    let remainders = Remainders::default();
    let mut rx = shared.mobile_tx.subscribe();

    let (mut sender, mut receiver) = socket.split();

    // Forward broadcast text-mobile messages to this phone. Runs until either
    // the phone disconnects (sender closed) or the task is aborted below.
    let forward = tokio::spawn(async move {
        while let Ok(text) = rx.recv().await {
            let payload = serde_json::json!({"type": "text-mobile", "text": text});
            if sender
                .send(Message::Text(payload.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Text(text)) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => dispatch(&v, &shared, &remainders),
                Err(_) => shared.log(format!("ws: ignoring non-JSON frame: {text}")),
            },
            Ok(Message::Close(_)) => break,
            // Binary / ping / pong: nothing to do.
            Ok(_) => {}
            Err(_) => break,
        }
    }

    forward.abort();
    shared.clients.fetch_sub(1, Ordering::SeqCst);
    shared.log("phone disconnected (ws)");
}

/// Route one decoded client JSON message to the input/UI channels.
fn dispatch(v: &Value, shared: &Shared, remainders: &Remainders) {
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
            let Some(key_name) = str_field(v, &["key"]) else {
                return;
            };
            if key_name.is_empty() {
                return;
            }
            let mut modifiers = Vec::new();
            match v.get("modifier") {
                Some(Value::String(m)) => {
                    if let Some(k) = map_modifier(m) {
                        modifiers.push(k);
                    }
                }
                Some(Value::Array(list)) => {
                    for m in list.iter().filter_map(Value::as_str) {
                        if let Some(k) = map_modifier(m) {
                            modifiers.push(k);
                        }
                    }
                }
                _ => {}
            }
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
        "" => {
            shared.log("ws: ignoring message without \"type\"");
        }
        other => {
            shared.log(format!("ws: ignoring unknown type \"{other}\""));
        }
    }
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

/// Serve forever on `0.0.0.0:1313`.
pub async fn serve(app: axum::Router) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PORT)).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

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
        Arc::new(Shared {
            clients: AtomicUsize::new(0),
            input_tx,
            ui_tx,
            mobile_tx,
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
        (
            Arc::new(Shared {
                clients: AtomicUsize::new(0),
                input_tx,
                ui_tx,
                mobile_tx,
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

        dispatch(
            &serde_json::json!({"type": "coordinates", "x": 3.2, "y": 0.0}),
            &shared,
            &rem,
        );
        assert!(matches!(
            input_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            InputCommand::MouseMove { x: 3, y: 0 }
        ));

        dispatch(
            &serde_json::json!({"type": "clicks", "finger": "right", "doubleTap": true}),
            &shared,
            &rem,
        );
        assert!(matches!(
            input_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            InputCommand::Click { double: true, .. }
        ));

        dispatch(
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
            dispatch(&msg, &shared, &rem);
        }
        // 3 drag/key/media/edit-key inputs above produce 6 commands total
        // (dragstart, dragmove, dragend, key, mediakey, editkey); 3 consumed
        // so far, so 3 more input commands remain before the text check.
        for _ in 0..6 {
            let _ = input_rx.recv_timeout(Duration::from_secs(1));
        }

        dispatch(
            &serde_json::json!({"type": "text", "text": "hello"}),
            &shared,
            &rem,
        );
        assert!(matches!(
            ui_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            UiEvent::IncomingText(t) if t == "hello"
        ));

        // Unknown types and missing types are ignored (no new commands).
        dispatch(&serde_json::json!({"type": "nope"}), &shared, &rem);
        dispatch(&serde_json::json!({"x": 1}), &shared, &rem);
        assert!(input_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_to_phone_fanout() {
        let (shared, _, _) = test_shared_with_rx();
        let mut rx = shared.mobile_tx.subscribe();
        assert_eq!(shared.send_to_phone("hi"), 1);
        assert_eq!(rx.recv().await.unwrap(), "hi");
    }
}

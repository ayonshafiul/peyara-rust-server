//! Network server: HTTP + socket.io on port 1313.
//!
//! Replaces the Express + `socket.io` JS server from the original
//! `src/main.js`, keeping the same wire protocol so the existing Peyara
//! mobile app keeps working:
//!
//! * `GET /` returns JSON `"peyara"` (server discovery / QR verification).
//! * socket.io events: `coordinates`, `clicks`, `scroll`, `windowdragstart`,
//!   `windowdragupdate`, `windowdragend`, `key`, `media-key`, `edit-key`,
//!   `text` (phone -> desktop) and `text-mobile` (desktop -> phone).
//!
//! Screen-sharing signalling (`offer`/`answer`/ICE) from the original is
//! intentionally omitted.

use axum::{response::IntoResponse, routing::get, Json};
use serde_json::Value;
use socketioxide::{
    extract::{Data, SocketRef, State},
    SocketIo,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::Sender,
    Arc, Mutex,
};

use crate::{
    input::{
        map_button, map_media_key, map_modifier, resolve_edit_key, resolve_key_event, InputCommand,
    },
    netinfo::PORT,
};

/// Events forwarded from the server thread to the egui UI thread.
#[derive(Debug)]
pub enum UiEvent {
    /// Text received from the phone (`text` event).
    IncomingText(String),
    /// Diagnostic line for the on-screen log.
    Log(String),
}

/// State shared between the server thread and the UI thread.
pub struct Shared {
    /// Currently connected socket.io clients.
    pub clients: AtomicUsize,
    /// Channel into the enigo input worker thread.
    pub input_tx: Sender<InputCommand>,
    /// Channel into the egui UI thread.
    pub ui_tx: Sender<UiEvent>,
}

impl Shared {
    pub fn log(&self, line: impl Into<String>) {
        let _ = self.ui_tx.send(UiEvent::Log(line.into()));
    }
}

/// Per-phone sub-pixel remainders.
///
/// Pointer deltas arrive as floats but `enigo` consumes whole pixels. Rounding
/// every packet would lose up to 0.5px per packet, making slow movement stall
/// then jump. Instead each socket keeps its fractional leftover and adds it to
/// the next packet, so no motion is ever lost. Mouse and scroll keep separate
/// remainders because their units differ.
#[derive(Debug, Clone, Default)]
struct Remainders {
    mouse: Arc<Mutex<(f64, f64)>>,
    scroll: Arc<Mutex<(f64, f64)>>,
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

/// Build the router + socket.io layer. The [`SocketIo`] handle is returned so
/// the UI thread can emit `text-mobile` events.
pub fn build(shared: Arc<Shared>) -> (axum::Router, SocketIo) {
    let (layer, io) = SocketIo::builder().with_state(shared.clone()).build_layer();

    io.ns(
        "/",
        async |s: SocketRef, State(shared): State<Arc<Shared>>| {
            shared.clients.fetch_add(1, Ordering::SeqCst);
            s.extensions.insert(Remainders::default());
            shared.log(format!("phone connected ({})", s.id));
            register_handlers(&s);
            s.on_disconnect(async |s: SocketRef, State(shared): State<Arc<Shared>>| {
                shared.clients.fetch_sub(1, Ordering::SeqCst);
                shared.log(format!("phone disconnected ({})", s.id));
            });
        },
    );

    let app = axum::Router::new()
        .route("/", get(root))
        .with_state(shared)
        .layer(layer);
    (app, io)
}

fn register_handlers(s: &SocketRef) {
    s.on(
        "coordinates",
        async |s: SocketRef, Data::<Value>(v): Data<Value>, State(shared): State<Arc<Shared>>| {
            let (qx, qy) = quantize_mouse(&s, numf(&v, "x"), numf(&v, "y"));
            if qx != 0 || qy != 0 {
                let _ = shared
                    .input_tx
                    .send(InputCommand::MouseMove { x: qx, y: qy });
            }
        },
    );
    s.on(
        "clicks",
        async |_: SocketRef, Data::<Value>(v): Data<Value>, State(shared): State<Arc<Shared>>| {
            let finger = v.get("finger").and_then(Value::as_str).unwrap_or("left");
            let double = v.get("doubleTap").and_then(Value::as_bool).unwrap_or(false);
            let _ = shared.input_tx.send(InputCommand::Click {
                button: map_button(finger),
                double,
            });
        },
    );
    s.on(
        "scroll",
        async |s: SocketRef, Data::<Value>(v): Data<Value>, State(shared): State<Arc<Shared>>| {
            let (qx, qy) = quantize_scroll(&s, numf(&v, "x"), numf(&v, "y"));
            if qx != 0 || qy != 0 {
                let _ = shared.input_tx.send(InputCommand::Scroll { x: qx, y: qy });
            }
        },
    );
    s.on(
        "windowdragstart",
        async |_: SocketRef, State(shared): State<Arc<Shared>>| {
            let _ = shared.input_tx.send(InputCommand::DragStart);
        },
    );
    s.on(
        "windowdragupdate",
        async |s: SocketRef, Data::<Value>(v): Data<Value>, State(shared): State<Arc<Shared>>| {
            let (qx, qy) = quantize_mouse(&s, numf(&v, "x"), numf(&v, "y"));
            if qx != 0 || qy != 0 {
                let _ = shared
                    .input_tx
                    .send(InputCommand::DragMove { x: qx, y: qy });
            }
        },
    );
    s.on(
        "windowdragend",
        async |_: SocketRef, State(shared): State<Arc<Shared>>| {
            let _ = shared.input_tx.send(InputCommand::DragEnd);
        },
    );
    s.on(
        "key",
        async |_: SocketRef,
               Data::<String>(key): Data<String>,
               State(shared): State<Arc<Shared>>| {
            let _ = shared.input_tx.send(resolve_key_event(&key));
        },
    );
    s.on(
        "media-key",
        async |_: SocketRef,
               Data::<String>(name): Data<String>,
               State(shared): State<Arc<Shared>>| {
            if let Some(key) = map_media_key(&name) {
                let _ = shared.input_tx.send(InputCommand::MediaKey { key });
            }
        },
    );
    s.on(
        "edit-key",
        async |_: SocketRef, Data::<Value>(v): Data<Value>, State(shared): State<Arc<Shared>>| {
            let key_name = v.get("key").and_then(Value::as_str).unwrap_or("");
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
                key: resolve_edit_key(key_name),
                modifiers,
            });
        },
    );
    s.on(
        "text",
        async |_: SocketRef,
               Data::<String>(text): Data<String>,
               State(shared): State<Arc<Shared>>| {
            let _ = shared.ui_tx.send(UiEvent::IncomingText(text));
        },
    );
}

fn numf(v: &Value, key: &str) -> f64 {
    v.get(key)
        .and_then(|n| n.as_f64().or_else(|| n.as_i64().map(|i| i as f64)))
        .unwrap_or(0.0)
}

/// Quantize a mouse/drag delta through the socket's remainder (falls back to
/// plain rounding when the socket carries no remainder, e.g. in tests).
fn quantize_mouse(s: &SocketRef, dx: f64, dy: f64) -> (i32, i32) {
    match s.extensions.get::<Remainders>() {
        Some(r) => quantize(&r.mouse, dx, dy),
        None => (dx.round() as i32, dy.round() as i32),
    }
}

/// Quantize a scroll delta through the socket's scroll remainder.
fn quantize_scroll(s: &SocketRef, dx: f64, dy: f64) -> (i32, i32) {
    match s.extensions.get::<Remainders>() {
        Some(r) => quantize(&r.scroll, dx, dy),
        None => (dx.round() as i32, dy.round() as i32),
    }
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
        Arc::new(Shared {
            clients: AtomicUsize::new(0),
            input_tx,
            ui_tx,
        })
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
        let (app, _io) = build(test_shared());
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
    async fn socket_io_handshake_responds() {
        let (app, _io) = build(test_shared());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        // Engine.IO v4 polling handshake must issue a session id.
        let resp = http_roundtrip(
            port,
            b"GET /socket.io/?EIO=4&transport=polling HTTP/1.0\r\nHost: x\r\n\r\n",
        );
        assert!(resp.contains("\"sid\""), "{resp}");
    }
}

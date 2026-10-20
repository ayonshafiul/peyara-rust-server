//! Peyara Remote Mouse Server — Rust + egui edition.
//!
//! Desktop server that lets a Peyara mobile app control the mouse, type via
//! the keyboard and exchange clipboard text over the LAN (port 1313).
//! The phone pairs by scanning the on-screen QR code.
//!
//! TLS-only: every launch mints a fresh self-signed certificate in memory
//! (regenerable from the UI); the phone pins it via the QR fingerprint.

mod active_app;
mod app;
mod input;
mod netinfo;
mod permissions;
mod server;
mod tls;

use server::{Shared, UiEvent};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc, Mutex, RwLock,
};

/// Controls the TLS server task so the UI can rotate the certificate live.
///
/// Regeneration aborts the current listener and rebinds the same port with a
/// fresh certificate. Existing phone connections drop and must reconnect by
/// scanning the new QR code.
pub struct ServerCtl {
    rt: tokio::runtime::Handle,
    shared: Arc<Shared>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    current: RwLock<Arc<tls::CertBundle>>,
    generation: AtomicUsize,
}

impl ServerCtl {
    fn spawn(&self, bundle: Arc<tls::CertBundle>) {
        let app = server::build(self.shared.clone());
        if let Ok(mut slot) = self.task.lock() {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
            let b = bundle.clone();
            *slot = Some(self.rt.spawn(async move {
                if let Err(e) = tls::serve_tls(app, (*b).clone()).await {
                    eprintln!("server error: {e}");
                }
            }));
        }
    }

    pub fn current(&self) -> Arc<tls::CertBundle> {
        self.current
            .read()
            .map(|b| b.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Mint a fresh certificate + pairing code, restart the TLS listener and
    /// forcibly close every live session. Returns the new bundle and code.
    /// Connected phones drop and must re-pair by scanning the new QR code.
    /// The new identity is persisted so it survives restarts.
    pub fn regenerate(&self, sans: &[String]) -> Result<(Arc<tls::CertBundle>, String), String> {
        let bundle = Arc::new(tls::generate_cert(sans)?);
        let code = netinfo::generate_pairing()?;
        if let Err(e) = tls::save_identity(&bundle, &code, sans) {
            self.shared
                .log(format!("warn: could not persist identity: {e}"));
        }
        self.spawn(bundle.clone());
        if let Ok(mut cur) = self.current.write() {
            *cur = bundle.clone();
        }
        if let Ok(mut pairing) = self.shared.pairing.write() {
            *pairing = code.clone();
        }
        // Kick every live session: its pairing code is dead. The sessions
        // close themselves (dedicated tasks survive the listener abort).
        let kicked = self.shared.clients.swap(0, Ordering::SeqCst);
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) as u64 + 1;
        let _ = self.shared.generation.send(gen);
        self.shared.log(format!(
            "certificate regenerated — disconnected {kicked} phone{}",
            if kicked == 1 { "" } else { "s" }
        ));
        Ok((bundle, code))
    }

    /// Current pairing code.
    pub fn pairing(&self) -> String {
        self.shared
            .pairing
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

fn main() {
    // 1. Input worker (owns the enigo backend on its own thread).
    let (input_tx, worker_log_rx, backend_retry_tx) = input::spawn_input_worker();
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();

    // 2. LAN addresses first: they become certificate SANs (no server needed).
    let host = netinfo::host_name();
    let addrs = netinfo::lan_addrs();
    let sans: Vec<String> = addrs.iter().map(|a| a.ip.clone()).collect();

    // 3. Server identity: reuse the persisted certificate + pairing code so
    //    phones stay paired across restarts. Rotation happens only on demand
    //    (Regenerate), on network-driven regeneration, or when the stored
    //    identity is missing/stale/doesn't cover current addresses.
    let (initial, pairing) = match tls::load_identity(&sans) {
        Some(id) => (Arc::new(id.bundle), id.pairing),
        None => {
            let bundle = match tls::generate_cert(&sans) {
                Ok(b) => Arc::new(b),
                Err(e) => {
                    eprintln!("tls error: {e}");
                    std::process::exit(1);
                }
            };
            let code = match netinfo::generate_pairing() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("pairing error: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = tls::save_identity(&bundle, &code, &sans) {
                eprintln!("warn: could not persist identity ({e}); a fresh one will be minted next launch");
            }
            (bundle, code)
        }
    };

    // 4. Shared state.
    let (mobile_tx, _) = tokio::sync::broadcast::channel::<String>(16);
    let (generation, _) = tokio::sync::watch::channel(0u64);
    let shared = Arc::new(Shared {
        clients: AtomicUsize::new(0),
        input_tx,
        ui_tx,
        mobile_tx,
        pairing: RwLock::new(pairing.clone()),
        rejected: AtomicUsize::new(0),
        generation,
    });

    // 5. Tokio runtime + TLS server task (https + wss on :1313, no plaintext).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let ctl = Arc::new(ServerCtl {
        rt: rt.handle().clone(),
        shared: shared.clone(),
        task: Mutex::new(None),
        current: RwLock::new(initial.clone()),
        generation: AtomicUsize::new(0),
    });
    ctl.spawn(initial.clone());
    // `rt` stays alive on the stack until the window closes.

    // 6. Verify our LAN URLs against the live TLS server, then build the QR
    //    payload (fingerprint + pairing code) the phone scans. Every usable
    //    address is advertised — verified first — so the phone can try each.
    let checks = netinfo::verify_all(&addrs, &initial.cert_der);
    let mut urls = netinfo::urls_from_checks(&checks);
    if urls.is_empty() {
        urls = netinfo::candidate_urls(&addrs);
    }
    shared.log(format!("net: {}", netinfo::describe_addrs(&addrs)));
    for c in &checks {
        if c.verified {
            shared.log(format!("net: {} ok ({})", c.url, c.addr.name));
        } else if c.physical {
            shared.log(format!(
                "net: {} not verifiable from this PC — phone will still try it; \
                 if it fails, allow incoming TCP 1313 (e.g. sudo ufw allow 1313/tcp)",
                c.url
            ));
        } else {
            shared.log(format!(
                "net: {} skipped ({}, virtual interface)",
                c.url, c.addr.name
            ));
        }
    }
    let qr_payload = netinfo::qr_payload(
        netinfo::app_version(),
        &host,
        &initial.fingerprint,
        &pairing,
        &urls,
    );

    // 7. egui desktop window (Electron's icon reused for the frame).
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("Peyara Server v{}", netinfo::app_version()))
            .with_inner_size([880.0, 640.0])
            .with_min_inner_size([720.0, 520.0])
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Peyara Server",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::PeyaraApp::new(
                cc,
                app::AppInit {
                    shared: shared.clone(),
                    ctl: ctl.clone(),
                    qr_payload: qr_payload.clone(),
                    host: host.clone(),
                    addrs: addrs.clone(),
                    urls: urls.clone(),
                    fingerprint: initial.fingerprint.clone(),
                    pairing: pairing.clone(),
                    ui_rx,
                    worker_log_rx,
                    backend_retry_tx: backend_retry_tx.clone(),
                },
            )))
        }),
    )
    .expect("eframe");
}

/// Window icon decoded from the bundled Electron asset (`assets/icon.png`).
fn load_icon() -> std::sync::Arc<egui::IconData> {
    let rgba = include_bytes!("../assets/icon.png");
    if let Ok(img) = image::load_from_memory(rgba) {
        let img = img.to_rgba8();
        let (width, height) = (img.width(), img.height());
        return std::sync::Arc::new(egui::IconData {
            width,
            height,
            rgba: img.into_raw(),
        });
    }
    std::sync::Arc::new(egui::IconData {
        width: 1,
        height: 1,
        rgba: vec![0, 0, 0, 0],
    })
}

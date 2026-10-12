//! Peyara Remote Mouse Server — Rust + egui edition.
//!
//! Desktop server that lets a Peyara mobile app control the mouse, type via
//! the keyboard and exchange clipboard text over the LAN (port 1313).
//! The phone pairs by scanning the on-screen QR code.

mod app;
mod input;
mod netinfo;
mod server;

use server::{Shared, UiEvent};
use std::{
    sync::{atomic::AtomicUsize, mpsc, Arc},
    time::Duration,
};

fn main() {
    // 1. Input worker (owns the enigo backend on its own thread).
    let (input_tx, worker_log_rx) = input::spawn_input_worker();
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();

    // 2. Shared state.
    let shared = Arc::new(Shared {
        clients: AtomicUsize::new(0),
        input_tx,
        ui_tx,
    });

    // 3. Tokio runtime + server task (axum + socket.io on :1313).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (app_router, io) = server::build(shared.clone());
    rt.spawn(async move {
        if let Err(e) = server::serve(app_router).await {
            eprintln!("server error: {e}");
            std::process::exit(1);
        }
    });
    let rt_handle = rt.handle().clone();
    // `rt` stays alive on the stack until the window closes.

    // 4. Discover LAN addresses and verify them against the live server,
    //    then build the QR payload the phone scans. The server needs a
    //    moment to bind, so retry verification briefly.
    let host = netinfo::host_name();
    let addrs = netinfo::lan_ipv4_addrs();
    let candidates = netinfo::candidate_urls(&addrs);
    let mut urls = Vec::new();
    for _ in 0..10 {
        urls = candidates
            .iter()
            .filter(|u| netinfo::verify_server_url(u, Duration::from_millis(300)))
            .cloned()
            .collect();
        if !urls.is_empty() || candidates.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    // Fall back to unverified candidates so a QR code always shows.
    if urls.is_empty() {
        urls = candidates;
    }
    let qr_payload = netinfo::qr_payload(netinfo::app_version(), &host, &urls);

    // 5. egui desktop window (Electron's icon reused for the frame).
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
                    io: io.clone(),
                    rt: rt_handle.clone(),
                    qr_payload: qr_payload.clone(),
                    host: host.clone(),
                    urls: urls.clone(),
                    ui_rx,
                    worker_log_rx,
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

//! egui desktop UI: QR code, connection status, text/clipboard sharing and
//! incoming-file directory. Replaces the Electron `BrowserWindow` +
//! `src/index.html` / `src/js/renderer.js` UI (minus screen sharing).

use egui::{
    vec2, Align, Color32, ColorImage, CornerRadius, Layout, Margin, RichText, Sense, Stroke,
    TextureHandle, TextureOptions,
};
use std::sync::{
    atomic::Ordering,
    mpsc::{Receiver, Sender, TryRecvError},
    Arc,
};
use std::time::{Duration, Instant};

use crate::{
    netinfo::{self, LanAddr},
    permissions::{self, PermissionState},
    server::{Shared, UiEvent},
    ServerCtl,
};

const PLAY_STORE_URL: &str =
    "https://play.google.com/store/apps/details?id=io.github.ayonshafiul.peyara";
const SITE_URL: &str = "https://peyara-remote-mouse.vercel.app/";

// Palette: dark slate + Peyara yellow accent.
const ACCENT: Color32 = Color32::from_rgb(255, 209, 48);
const ACCENT_DIM: Color32 = Color32::from_rgb(255, 209, 48);
const INK: Color32 = Color32::from_rgb(16, 17, 20);
const BG: Color32 = Color32::from_rgb(17, 19, 24);
const PANEL: Color32 = Color32::from_rgb(22, 25, 32);
const CARD: Color32 = Color32::from_rgb(28, 32, 41);
const BORDER: Color32 = Color32::from_rgb(48, 54, 68);
const MUTED: Color32 = Color32::from_rgb(148, 156, 172);
const GREEN: Color32 = Color32::from_rgb(74, 222, 128);
const AMBER: Color32 = Color32::from_rgb(251, 191, 36);

/// Render the QR payload to an egui image using the pure-Rust `qrcode` crate.
pub fn qr_image(payload: &str) -> ColorImage {
    match qrcode::QrCode::new(payload.as_bytes()) {
        Ok(code) => {
            let img = code
                .render::<image::Luma<u8>>()
                .max_dimensions(248, 248)
                .build();
            let w = img.width() as usize;
            let h = img.height() as usize;
            ColorImage::from_gray([w, h], img.as_raw())
        }
        Err(_) => ColorImage::from_gray([8, 8], &[255u8; 64]),
    }
}

/// Result of a background network refresh (verify runs off the UI thread).
struct NetUpdate {
    urls: Vec<String>,
    fingerprint: String,
    pairing: String,
}

pub struct PeyaraApp {
    shared: Arc<Shared>,
    ctl: Arc<ServerCtl>,
    qr: TextureHandle,
    qr_payload: String,
    host: String,
    addrs: Vec<LanAddr>,
    urls: Vec<String>,
    fingerprint: String,
    pairing: String,
    outgoing: String,
    incoming: String,
    status_line: String,
    log: Vec<String>,
    ui_rx: Receiver<UiEvent>,
    worker_log_rx: Receiver<String>,
    net_tx: Sender<NetUpdate>,
    net_rx: Receiver<NetUpdate>,
    net_pending: bool,
    last_net_poll: Instant,
    regen_confirm: bool,
    // macOS Accessibility tracking (stubbed Granted elsewhere).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    backend_retry_tx: Sender<()>,
    // macOS Accessibility tracking (stubbed Granted elsewhere).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    perm_state: PermissionState,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    last_perm_poll: Instant,
}

/// Constructor bundle for [`PeyaraApp`] (keeps the arg list small).
pub struct AppInit {
    pub shared: Arc<Shared>,
    pub ctl: Arc<ServerCtl>,
    pub qr_payload: String,
    pub host: String,
    pub addrs: Vec<LanAddr>,
    pub urls: Vec<String>,
    pub fingerprint: String,
    pub pairing: String,
    pub ui_rx: Receiver<UiEvent>,
    pub worker_log_rx: Receiver<String>,
    pub backend_retry_tx: Sender<()>,
}

fn theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.dark_mode = true;
    visuals.window_fill = BG;
    visuals.panel_fill = PANEL;
    visuals.faint_bg_color = CARD;
    visuals.extreme_bg_color = INK;
    visuals.code_bg_color = INK;
    visuals.selection.bg_fill = Color32::from_rgb(84, 74, 28);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.hyperlink_color = ACCENT;
    ctx.set_visuals(visuals);

    ctx.style_mut(|style| {
        style.spacing.button_padding = vec2(12.0, 8.0);
        style.spacing.item_spacing = vec2(8.0, 8.0);
        style.visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.active.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.open.corner_radius = CornerRadius::same(8);
    });
}

fn card_frame() -> egui::Frame {
    egui::Frame {
        fill: CARD,
        corner_radius: CornerRadius::same(14),
        stroke: Stroke::new(1.0_f32, BORDER),
        inner_margin: Margin::same(16),
        ..Default::default()
    }
}

impl PeyaraApp {
    pub fn new(cc: &eframe::CreationContext<'_>, init: AppInit) -> Self {
        theme(&cc.egui_ctx);
        let AppInit {
            shared,
            ctl,
            qr_payload,
            host,
            addrs,
            urls,
            fingerprint,
            pairing,
            ui_rx,
            worker_log_rx,
            backend_retry_tx,
        } = init;
        let qr = cc.egui_ctx.load_texture(
            "peyara-qr",
            qr_image(&qr_payload),
            TextureOptions::default(),
        );
        let (net_tx, net_rx) = std::sync::mpsc::channel::<NetUpdate>();
        Self {
            shared,
            ctl,
            qr,
            qr_payload,
            host,
            addrs,
            urls,
            fingerprint,
            pairing,
            outgoing: String::new(),
            incoming: String::new(),
            status_line: "Ready — scan the QR code with your phone.".to_owned(),
            log: vec!["https + wss listening on port 1313".to_owned()],
            ui_rx,
            worker_log_rx,
            net_tx,
            net_rx,
            net_pending: false,
            last_net_poll: Instant::now(),
            regen_confirm: false,
            backend_retry_tx,
            perm_state: permissions::input_permission_state(),
            last_perm_poll: Instant::now(),
        }
    }

    fn rebuild_qr(&mut self, ctx: &egui::Context) {
        self.qr_payload = netinfo::qr_payload(
            netinfo::app_version(),
            &self.host,
            &self.fingerprint,
            &self.pairing,
            &self.urls,
        );
        self.qr = ctx.load_texture(
            "peyara-qr",
            qr_image(&self.qr_payload),
            TextureOptions::default(),
        );
    }

    /// Re-enumerate addresses and re-verify them on a worker thread, then
    /// rebuild the QR. Regenerates the certificate first when forced or when
    /// the address set changed (new IPs need new SANs baked into the cert).
    fn start_net_refresh(&mut self, force_regen: bool) {
        if self.net_pending {
            self.set_status("Refresh already running…");
            return;
        }
        let fresh = netinfo::lan_addrs();
        let (fp, code) = if force_regen || fresh != self.addrs {
            let sans: Vec<String> = fresh.iter().map(|a| a.ip.clone()).collect();
            match self.ctl.regenerate(&sans) {
                Ok((b, c)) => {
                    self.push_log("certificate + pairing code regenerated; phones must re-scan");
                    (b.fingerprint.clone(), c)
                }
                Err(e) => {
                    self.set_status("Certificate regeneration failed.");
                    self.push_log(format!("cert regen failed: {e}"));
                    return;
                }
            }
        } else {
            let bundle = self.ctl.current();
            (bundle.fingerprint.clone(), self.ctl.pairing())
        };
        self.addrs = fresh.clone();
        self.fingerprint = fp.clone();
        self.pairing = code.clone();
        self.net_pending = true;
        self.set_status("Refreshing network…");
        let tx = self.net_tx.clone();
        let shared = self.shared.clone();
        let cert_der = self.ctl.current().cert_der.clone();
        std::thread::spawn(move || {
            let checks = netinfo::verify_all(&fresh, &cert_der);
            for c in &checks {
                if c.verified {
                    shared.log(format!("net: {} ok ({})", c.url, c.addr.name));
                } else if c.physical {
                    shared.log(format!(
                        "net: {} unverified — phone will still try it",
                        c.url
                    ));
                }
            }
            let mut urls = netinfo::urls_from_checks(&checks);
            if urls.is_empty() {
                urls = netinfo::candidate_urls(&fresh);
            }
            let _ = tx.send(NetUpdate {
                urls,
                fingerprint: fp,
                pairing: code,
            });
        });
    }

    /// Apply a finished background refresh (runs on the UI thread).
    fn apply_net_update(&mut self, ctx: &egui::Context, u: NetUpdate) {
        self.net_pending = false;
        self.urls = u.urls;
        self.fingerprint = u.fingerprint;
        self.pairing = u.pairing;
        self.rebuild_qr(ctx);
        self.set_status("Network refreshed — scan the QR code.");
        self.push_log("network refreshed");
    }

    fn iface_for_url(&self, url: &str) -> Option<&str> {
        let host = netinfo::url_host(url);
        self.addrs
            .iter()
            .find(|a| a.ip == host)
            .map(|a| a.name.as_str())
    }

    /// macOS Accessibility card: status + explicit grant buttons.
    #[cfg(target_os = "macos")]
    fn show_permission_card(&mut self, ui: &mut egui::Ui) {
        let granted = self.perm_state == PermissionState::Granted;
        card_frame().show(ui, |ui| {
            section_title(ui, "🔐", "Input permission");
            ui.horizontal(|ui| {
                status_dot(ui, if granted { GREEN } else { AMBER }, 9.0);
                ui.label(
                    RichText::new(if granted {
                        "Accessibility granted — mouse & keyboard work."
                    } else {
                        "Accessibility missing — the phone connects but input won't move."
                    })
                    .size(13.0)
                    .color(MUTED),
                );
            });
            if !granted {
                ui.label(
                    RichText::new(
                        "macOS only lets apps control the mouse/keyboard after an explicit \
                         grant. Press Request Access, approve the system dialog, then enable \
                         Peyara under System Settings → Privacy & Security → Accessibility.",
                    )
                    .size(12.0)
                    .color(MUTED),
                );
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_sized(
                        [150.0, 32.0],
                        egui::Button::new("Request Access…").corner_radius(CornerRadius::same(8)),
                    )
                    .clicked()
                {
                    match permissions::request_input_permission() {
                        PermissionState::Granted => {
                            self.perm_state = PermissionState::Granted;
                            self.set_status("Accessibility granted — input enabled.");
                            self.push_log("accessibility granted; input enabled");
                            let _ = self.backend_retry_tx.send(());
                        }
                        PermissionState::Missing => {
                            self.perm_state = PermissionState::Missing;
                            self.set_status("Still missing — enable Peyara in System Settings.");
                            self.push_log(
                                "accessibility still missing after prompt; open System Settings",
                            );
                        }
                    }
                }
                if ui
                    .add_sized(
                        [210.0, 32.0],
                        egui::Button::new("Open Accessibility Settings")
                            .corner_radius(CornerRadius::same(8)),
                    )
                    .clicked()
                {
                    permissions::open_accessibility_settings();
                }
                if ui.small_button("Check again").clicked() {
                    self.perm_state = permissions::input_permission_state();
                    let _ = self.backend_retry_tx.send(());
                    self.set_status("Permission re-checked.");
                }
            });
        });
        ui.add_space(10.0);
    }

    fn poll_channels(&mut self, ctx: &egui::Context) {
        loop {
            match self.ui_rx.try_recv() {
                Ok(UiEvent::IncomingText(text)) => {
                    self.incoming = text;
                    self.set_status("Text received from phone.");
                    self.push_log("text received from phone");
                    // Bring the window forward so the text is seen immediately.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Ok(UiEvent::Log(line)) => self.push_log(line),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        while let Ok(line) = self.worker_log_rx.try_recv() {
            self.push_log(line);
        }
    }

    fn set_status(&mut self, line: impl Into<String>) {
        self.status_line = line.into();
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > 40 {
            let excess = self.log.len() - 40;
            self.log.drain(..excess);
        }
    }

    fn send_to_phone(&mut self) {
        if self.outgoing.trim().is_empty() {
            self.set_status("Type something first.");
            return;
        }
        let receivers = self.shared.send_to_phone(&self.outgoing);
        if receivers > 0 {
            self.set_status("Sent to phone.");
            self.push_log("text sent to phone");
        } else {
            self.set_status("Send failed — is a phone connected?");
            self.push_log("send failed: no ws clients");
        }
        self.outgoing.clear();
    }

    fn copy_to_clipboard(&mut self, text: &str, what: &str) {
        match arboard::Clipboard::new() {
            Ok(mut c) => match c.set_text(text.to_owned()) {
                Ok(()) => {
                    self.set_status(format!("{what} copied to clipboard."));
                    self.push_log(format!("copied {what} to clipboard"));
                }
                Err(e) => {
                    self.set_status("Clipboard failed.");
                    self.push_log(format!("clipboard error: {e}"));
                }
            },
            Err(e) => {
                self.set_status("Clipboard unavailable.");
                self.push_log(format!("clipboard error: {e}"));
            }
        }
    }
}

fn status_dot(ui: &mut egui::Ui, color: Color32, diameter: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(diameter, diameter), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), diameter / 2.0, color);
}

fn section_title(ui: &mut egui::Ui, icon: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(17.0));
        ui.label(RichText::new(title).size(16.0).strong());
    });
    ui.add_space(4.0);
}

impl eframe::App for PeyaraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_channels(ctx);
        // Apply finished background network refreshes (take the latest).
        let mut latest: Option<NetUpdate> = None;
        while let Ok(u) = self.net_rx.try_recv() {
            latest = Some(u);
        }
        if let Some(u) = latest {
            self.apply_net_update(ctx, u);
        }
        // Re-enumerate occasionally: Wi-Fi/VPN/DHCP changes alter the address
        // set, which needs a new certificate (SANs are baked in) and QR.
        if !self.net_pending && self.last_net_poll.elapsed() > Duration::from_secs(15) {
            self.last_net_poll = Instant::now();
            if netinfo::lan_addrs() != self.addrs {
                self.push_log("network change detected — refreshing");
                self.start_net_refresh(false);
            }
        }
        // Re-check the OS input permission occasionally; a fresh grant
        // (user toggled it in System Settings) re-arms the input backend.
        // macOS only — other platforms report Granted unconditionally.
        #[cfg(target_os = "macos")]
        if self.last_perm_poll.elapsed() > Duration::from_secs(3) {
            self.last_perm_poll = Instant::now();
            let now_state = permissions::input_permission_state();
            if now_state != self.perm_state {
                self.perm_state = now_state;
                match now_state {
                    PermissionState::Granted => {
                        self.set_status("Accessibility granted — input enabled.");
                        self.push_log("accessibility granted; input enabled");
                        let _ = self.backend_retry_tx.send(());
                    }
                    PermissionState::Missing => {
                        self.set_status("Accessibility permission missing.");
                        self.push_log("accessibility permission missing");
                    }
                }
            }
        }
        let clients = self.shared.clients.load(Ordering::SeqCst);
        let (dot_color, status_text) = if clients > 0 {
            (
                GREEN,
                format!(
                    "{clients} phone{} connected",
                    if clients == 1 { "" } else { "s" }
                ),
            )
        } else {
            (AMBER, "waiting for phone…".to_owned())
        };

        // Text boxes scale with the window instead of fixed row counts.
        let text_h = (ctx.available_rect().height() * 0.16).clamp(80.0, 240.0);

        // ── Top bar ──────────────────────────────────────────────
        egui::TopBottomPanel::top("topbar")
            .frame(egui::Frame {
                fill: PANEL,
                stroke: Stroke::new(1.0_f32, BORDER),
                inner_margin: Margin::symmetric(16, 10),
                ..Default::default()
            })
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("◉").size(20.0).color(ACCENT));
                    ui.label(RichText::new("Peyara").size(19.0).strong());
                    egui::Frame {
                        fill: Color32::from_rgb(52, 46, 18),
                        corner_radius: CornerRadius::same(6),
                        stroke: Stroke::new(1.0_f32, Color32::from_rgb(120, 100, 30)),
                        inner_margin: Margin::symmetric(8, 3),
                        ..Default::default()
                    }
                    .show(ui, |ui| {
                        ui.monospace(
                            RichText::new(format!("v{}", netinfo::app_version())).color(ACCENT),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(&status_text).color(MUTED).size(13.0));
                        status_dot(ui, dot_color, 9.0);
                        ui.label(RichText::new(&self.host).color(MUTED).size(13.0));
                        ui.label(RichText::new(":1313").monospace().color(MUTED).size(13.0));
                    });
                });
            });

        // ── Left sidebar: connect ────────────────────────────────
        egui::SidePanel::left("sidebar")
            .exact_width(296.0)
            .resizable(false)
            .frame(egui::Frame {
                fill: PANEL,
                stroke: Stroke::new(1.0_f32, BORDER),
                inner_margin: Margin::same(16),
                ..Default::default()
            })
            .show(ctx, |ui| {
                ui.label(RichText::new("CONNECT").size(12.0).color(MUTED).strong());
                ui.add_space(6.0);
                card_frame().show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        // White quiet-zone so phones scan reliably.
                        egui::Frame {
                            fill: Color32::WHITE,
                            corner_radius: CornerRadius::same(10),
                            inner_margin: Margin::same(12),
                            ..Default::default()
                        }
                        .show(ui, |ui| {
                            ui.image((self.qr.id(), vec2(200.0, 200.0)));
                        });
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("Scan with the Peyara app")
                                .size(13.5)
                                .strong(),
                        );
                        ui.label(
                            RichText::new("Your phone must be on the same Wi-Fi.")
                                .size(12.0)
                                .color(MUTED),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new("🔒 Secure (https + wss, pinned cert)")
                                .size(11.5)
                                .color(MUTED),
                        );
                        ui.add_space(4.0);
                        if ui
                            .add_sized(
                                [ui.available_width(), 30.0],
                                egui::Button::new("Regenerate certificate")
                                    .corner_radius(CornerRadius::same(8)),
                            )
                            .clicked()
                        {
                            self.regen_confirm = true;
                        }
                    });
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("PAIRING CODE")
                            .size(11.0)
                            .color(MUTED)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(&self.pairing)
                            .monospace()
                            .size(22.0)
                            .strong()
                            .color(ACCENT),
                    );
                    ui.label(
                        RichText::new("Enter this on the phone if the camera can't scan.")
                            .size(11.0)
                            .color(MUTED),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("CERT SHA-256 FINGERPRINT")
                            .size(11.0)
                            .color(MUTED)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(&self.fingerprint)
                            .monospace()
                            .size(9.5)
                            .color(MUTED),
                    );
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("SERVER ADDRESSES")
                            .size(12.0)
                            .color(MUTED)
                            .strong(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Refresh network").clicked() {
                            self.start_net_refresh(false);
                        }
                    });
                });
                ui.add_space(2.0);
                if self.urls.is_empty() {
                    ui.label(RichText::new("No network found.").color(MUTED));
                }
                for url in self.urls.clone() {
                    let iface = self.iface_for_url(&url).unwrap_or("?").to_owned();
                    card_frame()
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.monospace(RichText::new(&url).size(11.5));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.small_button("Copy").clicked() {
                                        self.copy_to_clipboard(&url, "Address");
                                    }
                                });
                            });
                            ui.label(
                                RichText::new(format!("interface: {iface}"))
                                    .size(10.5)
                                    .color(MUTED),
                            );
                        });
                    ui.add_space(6.0);
                }
                ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                    ui.hyperlink_to("peyara-remote-mouse.vercel.app", SITE_URL);
                    ui.hyperlink_to("Get the Android app", PLAY_STORE_URL);
                    ui.add_space(4.0);
                });
            });

        // ── Main content ─────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame {
                fill: BG,
                inner_margin: Margin::same(16),
                ..Default::default()
            })
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Status line.
                    ui.horizontal(|ui| {
                        status_dot(ui, ACCENT_DIM, 8.0);
                        ui.label(RichText::new(&self.status_line).size(13.0).color(MUTED));
                    });
                    ui.add_space(6.0);

                    // macOS Accessibility grant card.
                    #[cfg(target_os = "macos")]
                    self.show_permission_card(ui);

                    // Text sharing.
                    card_frame().show(ui, |ui| {
                        section_title(ui, "✉", "Send text to phone");
                        ui.add_sized(
                            [ui.available_width(), text_h],
                            egui::TextEdit::multiline(&mut self.outgoing)
                                .desired_width(f32::INFINITY)
                                .hint_text("Type here to send text to phone…"),
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let send = egui::Button::new(
                                RichText::new("Send to phone").color(INK).strong(),
                            )
                            .fill(ACCENT)
                            .corner_radius(CornerRadius::same(8))
                            .min_size(vec2(150.0, 34.0));
                            if ui.add(send).clicked() {
                                self.send_to_phone();
                            }
                            if ui
                                .add_sized(
                                    [170.0, 34.0],
                                    egui::Button::new("Copy to clipboard")
                                        .corner_radius(CornerRadius::same(8)),
                                )
                                .clicked()
                            {
                                let text = self.outgoing.clone();
                                self.copy_to_clipboard(&text, "Text");
                            }
                        });
                    });
                    ui.add_space(10.0);

                    // Incoming text from the phone.
                    card_frame().show(ui, |ui| {
                        section_title(ui, "↓", "From phone");
                        ui.add_sized(
                            [ui.available_width(), text_h],
                            egui::TextEdit::multiline(&mut self.incoming)
                                .desired_width(f32::INFINITY)
                                .hint_text("Incoming phone text appears here…"),
                        );
                        ui.add_space(4.0);
                        if ui
                            .add_sized(
                                [ui.available_width(), 30.0],
                                egui::Button::new("Copy to clipboard")
                                    .corner_radius(CornerRadius::same(8)),
                            )
                            .clicked()
                        {
                            let text = self.incoming.clone();
                            self.copy_to_clipboard(&text, "Incoming text");
                        }
                    });
                    ui.add_space(10.0);

                    // Log.
                    card_frame().show(ui, |ui| {
                        ui.collapsing("Event log", |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(140.0)
                                .show(ui, |ui| {
                                    for line in &self.log {
                                        ui.monospace(RichText::new(line).size(12.0).color(MUTED));
                                    }
                                });
                        });
                    });
                });
            });

        // Regenerate confirmation dialog.
        if self.regen_confirm {
            let n = self.shared.clients.load(Ordering::SeqCst);
            let msg = if n == 0 {
                "No phones are connected.\nA new certificate and pairing code will be issued \
                 and the QR code will change."
                    .to_owned()
            } else {
                format!(
                    "This will disconnect {n} phone{}.\nThey must scan the QR code again \
                     to reconnect. Continue?",
                    if n == 1 { "" } else { "s" }
                )
            };
            egui::Window::new("Regenerate certificate?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(msg);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Regenerate").clicked() {
                            self.regen_confirm = false;
                            self.start_net_refresh(true);
                        }
                        if ui.button("Cancel").clicked() {
                            self.regen_confirm = false;
                        }
                    });
                });
        }

        // Keep client count fresh.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

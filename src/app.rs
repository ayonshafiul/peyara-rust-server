//! egui desktop UI: QR code, connection status, text/clipboard sharing and
//! incoming-file directory. Replaces the Electron `BrowserWindow` +
//! `src/index.html` / `src/js/renderer.js` UI (minus screen sharing).

use egui::{
    vec2, Align, Color32, ColorImage, CornerRadius, Layout, Margin, RichText, Sense, Stroke,
    TextureHandle, TextureOptions,
};
use socketioxide::SocketIo;
use std::sync::{
    atomic::Ordering,
    mpsc::{Receiver, TryRecvError},
    Arc,
};

use crate::{
    netinfo::{self},
    server::{Shared, UiEvent},
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

pub struct PeyaraApp {
    shared: Arc<Shared>,
    io: SocketIo,
    rt: tokio::runtime::Handle,
    qr: TextureHandle,
    host: String,
    urls: Vec<String>,
    outgoing: String,
    incoming: String,
    status_line: String,
    log: Vec<String>,
    ui_rx: Receiver<UiEvent>,
    worker_log_rx: Receiver<String>,
}

/// Constructor bundle for [`PeyaraApp`] (keeps the arg list small).
pub struct AppInit {
    pub shared: Arc<Shared>,
    pub io: SocketIo,
    pub rt: tokio::runtime::Handle,
    pub qr_payload: String,
    pub host: String,
    pub urls: Vec<String>,
    pub ui_rx: Receiver<UiEvent>,
    pub worker_log_rx: Receiver<String>,
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
            io,
            rt,
            qr_payload,
            host,
            urls,
            ui_rx,
            worker_log_rx,
        } = init;
        let qr = cc.egui_ctx.load_texture(
            "peyara-qr",
            qr_image(&qr_payload),
            TextureOptions::default(),
        );
        Self {
            shared,
            io,
            rt,
            qr,
            host,
            urls,
            outgoing: String::new(),
            incoming: String::new(),
            status_line: "Ready — scan the QR code with your phone.".to_owned(),
            log: vec!["server listening on port 1313".to_owned()],
            ui_rx,
            worker_log_rx,
        }
    }

    fn poll_channels(&mut self) {
        loop {
            match self.ui_rx.try_recv() {
                Ok(UiEvent::IncomingText(text)) => {
                    self.incoming = text;
                    self.set_status("Text received from phone.");
                    self.push_log("text received from phone");
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
        match self
            .rt
            .block_on(self.io.emit("text-mobile", &self.outgoing))
        {
            Ok(()) => {
                self.set_status("Sent to phone.");
                self.push_log("text sent to phone");
            }
            Err(e) => {
                self.set_status("Send failed — is a phone connected?");
                self.push_log(format!("send failed: {e}"));
            }
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
        self.poll_channels();
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
                    });
                });
                ui.add_space(10.0);
                ui.label(
                    RichText::new("SERVER ADDRESSES")
                        .size(12.0)
                        .color(MUTED)
                        .strong(),
                );
                ui.add_space(2.0);
                if self.urls.is_empty() {
                    ui.label(RichText::new("No network found.").color(MUTED));
                }
                for url in self.urls.clone() {
                    card_frame()
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.monospace(RichText::new(&url).size(12.5));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.small_button("Copy").clicked() {
                                        self.copy_to_clipboard(&url, "Address");
                                    }
                                });
                            });
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

        // Keep client count fresh.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

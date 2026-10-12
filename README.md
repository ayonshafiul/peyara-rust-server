# Peyara Server (Rust + egui)

Desktop server for [Peyara Remote Mouse](https://peyara-remote-mouse.vercel.app/):
control your PC's mouse, keyboard and clipboard from the Peyara mobile app
over the LAN. This is a pure-Rust rewrite of the original Electron server,
keeping the same phone protocol (port **1313**) so the existing mobile app
works unchanged. Screen sharing was intentionally dropped.

## Run

```sh
cargo run --release
```

Scan the on-screen QR code with the phone app. The window also shows
connection status and a text/clipboard exchange box.

## Releases

Every `main` push builds native packages via GitHub Actions and publishes
them to a public release tagged `v<version>` (taken from `Cargo.toml`, so
bump the version there for a new release line; re-pushes refresh the same
tag with the latest binaries): `.deb` + `.rpm` (Linux), NSIS `.exe`
installer (Windows), `.dmg` (macOS arm64). Linux packages install the binary
to `/usr/bin`, plus a desktop entry and the app icon.

## How it works

| Piece | Crate |
|---|---|
| GUI | `eframe` / `egui` |
| QR code | `qrcode` (pure Rust, rendered to an egui texture) |
| HTTP + socket.io server | `axum` + `socketioxide` (v4 protocol compat) |
| Mouse / keyboard | `enigo` on a dedicated worker thread |
| Clipboard | `arboard` |

Phone → PC events: `coordinates`, `clicks`, `scroll`,
`windowdragstart`/`windowdragupdate`/`windowdragend`, `key`, `media-key`,
`edit-key`, `text`. PC → phone: `text-mobile`. `GET /` answers `"peyara"`
for discovery/QR verification.

## Wayland (Ubuntu 24 default session)

X11 injection only reaches XWayland windows, so on GNOME/Wayland input
goes through the **Remote Desktop portal** (`libei`, enabled via enigo's
`libei_tokio` feature):

1. Launch the app — a system prompt asks to allow remote interaction.
2. Approve it (the choice is remembered per app).
3. The mouse/keyboard from the phone now drives the whole desktop.

Requirements (preinstalled on stock Ubuntu 24): `xdg-desktop-portal` and
`xdg-desktop-portal-gnome` running in your session. If you deny the prompt
or it never appears, relaunch the app and watch the in-app Log panel, which
reports the detected session type and backend status. As a fallback, pick
"Ubuntu on Xorg" at the login screen, where plain X11 injection works.

## Layout

- `src/main.rs` — startup: input thread, Tokio server task, QR payload, window
- `src/server.rs` — HTTP + socket.io routes (same wire protocol as `src/main.js`)
- `src/input.rs` — phone commands → `enigo` key/button mapping
- `src/netinfo.rs` — LAN addresses, QR payload (`version,<peyara>,host,urls…`)
- `src/app.rs` — egui window

# Peyara Server

[![Build](https://github.com/ayonshafiul/peyara-rust-server/actions/workflows/build.yml/badge.svg)](https://github.com/ayonshafiul/peyara-rust-server/actions)
[![Latest release](https://img.shields.io/github/v/release/ayonshafiul/peyara-rust-server)](https://github.com/ayonshafiul/peyara-rust-server/releases)
[![Downloads](https://img.shields.io/github/downloads/ayonshafiul/peyara-rust-server/total)](https://github.com/ayonshafiul/peyara-rust-server/releases)
[![License](https://img.shields.io/github/license/ayonshafiul/peyara-rust-server)](LICENSE)

Turn your phone into a wireless touchpad, keyboard, and clipboard for your
PC. Peyara Server runs on your desktop and pairs with the [Peyara mobile
app](https://peyara-remote-mouse.vercel.app/) over your local network —
just scan the QR code on screen.

This is a pure-Rust rewrite of the [original Electron
server](https://github.com/ayonshafiul/peyara-mouse-server). It speaks the
same phone protocol (port **1313**), so the existing mobile app works
unchanged. Screen sharing was intentionally dropped to keep it light.

## Features

- 🖱️ **Touchpad** — move, click (left/right/middle, double-tap), drag, scroll
- ⌨️ **Keyboard** — type text, special keys, media keys, key combos
- 📋 **Clipboard sharing** — send text to the phone, receive text from it
- 🔳 **QR pairing** — no typing IP addresses; scan and connect
- 🪶 **Lightweight** — one small native binary, no Electron, no Node.js

## Download

Grab the latest release for your OS from the [**Releases**
page](https://github.com/ayonshafiul/peyara-rust-server/releases):

| OS | File | Install |
|---|---|---|
| Linux (Debian/Ubuntu/…) | `Peyara-Server-<ver>-linux-amd64.deb` | `sudo dpkg -i <file>` |
| Linux (Fedora/…) | `Peyara-Server-<ver>-linux-x86_64.rpm` | `sudo rpm -i <file>` |
| Windows | `Peyara-Server-<ver>-windows-x86_64-setup.exe` | run the installer wizard |
| macOS (Apple Silicon) | `Peyara-Server-<ver>-macos-arm64.dmg` | open it, drag the app to Applications |

> Windows SmartScreen and macOS Gatekeeper may warn on first launch because
> community builds aren't commercially signed — this is expected.

## Quick start

1. Start Peyara Server on your PC.
2. Connect your phone to the **same Wi-Fi**.
3. Scan the on-screen QR code with the Peyara mobile app.
4. On GNOME/Wayland, approve the one-time "remote interaction" prompt (see
   [Wayland](#wayland-ubuntu-24-default-session)).

## Build from source

Requirements: a recent stable [Rust](https://www.rust-lang.org/tools/install)
toolchain.

```sh
cargo run --release
```

Linux needs the GUI/input system libraries:

```sh
sudo apt-get install -y \
  libx11-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev \
  libxcb-xfixes0-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libegl1-mesa-dev libgl1-mesa-dev
```

Windows and macOS need nothing extra. Useful checks before a PR:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Releases

Pushing a version tag (`git tag v1.1.0 && git push origin v1.1.0`) builds
native packages via GitHub Actions and publishes them as a public release
under that tag. The tag must match the `version` in `Cargo.toml`
(CI fails the run otherwise). Linux packages install the binary to
`/usr/bin`, plus a desktop entry and the app icon.

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

## Project layout

- `src/main.rs` — startup: input thread, Tokio server task, QR payload, window
- `src/server.rs` — HTTP + socket.io routes (same wire protocol as `src/main.js`)
- `src/input.rs` — phone commands → `enigo` key/button mapping
- `src/netinfo.rs` — LAN addresses, QR payload (`version,<peyara>,host,urls…`)
- `src/app.rs` — egui window

## Contributing

Issues and pull requests are welcome! For code changes, please keep
`cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt
--check` green. If you change the phone protocol, note that the mobile app
is a separate codebase — call it out in the PR.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) 2026 Shafiul Muslebeen.

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
| HTTPS + secure WebSocket server | `axum` + `axum-server` (rustls, `GET /` + `GET /ws`, TLS-only) |
| Ephemeral certificate | `rcgen` (fresh self-signed cert per launch, in memory only) |
| Fingerprint | SHA-256 of cert DER (`sha2` + `hex`), carried in the QR |
| Mouse / keyboard | `enigo` on a dedicated worker thread |
| Clipboard | `arboard` |

🔒 **TLS-only + paired.** The first launch mints a self-signed certificate and
pairing code, persisted in the OS config dir (`~/.config/peyara-egui/` on
Linux) so phones stay paired across restarts; the phone pins the cert via the
`sha256:` fingerprint in the QR code, then pairs with the 8-digit `pair:`
code from the same QR (shown on screen for manual entry). Unpaired connections
are closed before they can send input or receive clipboard text — a random
LAN device can no longer connect. Phone → PC messages (JSON over `wss://<host>:1313/ws`, after
`{"type":"hello","pair":…}`): `coordinates`,
`clicks`, `scroll`, `windowdragstart`/`windowdragupdate`/`windowdragend`,
`key`, `media-key`, `edit-key`, `text`. PC → phone: `text-mobile`.
`GET https://<host>:1313/` answers `"peyara"` for discovery/QR verification.
Pressing **Regenerate certificate** (under the QR code) mints a new cert
live — connected phones drop and must scan again. See
[`docs/qr-code-format.md`](docs/qr-code-format.md) for the QR spec and
[`docs/websocket-client-migration.md`](docs/websocket-client-migration.md)
for the message shapes.

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

## macOS (Accessibility permission)

Controlling the mouse/keyboard requires the app to be listed under System
Settings → Privacy & Security → Accessibility. Peyara shows an **Input
permission** card with the current status and two explicit actions:

1. **Request Access…** — shows macOS's own system prompt; approving it still
   requires toggling Peyara on in System Settings (the prompt deep-links
   there).
2. **Open Accessibility Settings** — jumps straight to the right pane.

The app re-checks every few seconds: granting access enables input
immediately, no restart needed. If the backend dies at runtime (e.g. the
permission is revoked), Peyara rebuilds it automatically a few times, then
asks you to fix the permission and press **Check again** (which also retries
the backend) instead of spamming prompts.

## Project layout

- `src/main.rs` — startup: input thread, ephemeral cert, TLS server task, QR payload, window
- `src/tls.rs` — cert generation, fingerprint, TLS-only serving + verification
- `src/server.rs` — HTTPS + secure WebSocket routes (`GET /`, `GET /ws`)
- `src/input.rs` — phone commands → `enigo` key/button mapping
- `src/netinfo.rs` — LAN addresses, QR payload (`version,<peyara>,host,sha256:fp,urls…`)
- `src/app.rs` — egui window (QR + regenerate-certificate button)

## Contributing

Issues and pull requests are welcome! For code changes, please keep
`cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt
--check` green. If you change the phone protocol, note that the mobile app
is a separate codebase — call it out in the PR.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) 2026 Shafiul Muslebeen.

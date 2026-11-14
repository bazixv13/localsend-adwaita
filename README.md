# LocalSend for GNOME

A native GNOME client for [LocalSend](https://localsend.org), built with Rust, GTK 4 and
libadwaita. It speaks the LocalSend protocol directly, so it can exchange files with the
official LocalSend apps on Android, iOS, Windows, macOS and Linux without an account, a
cloud or an internet connection.

![Application icon](data/icons/scalable/apps/io.github.localsend_adwaita.LocalSend.svg)

## Features

- Automatic device discovery over UDP multicast (`224.0.0.167:53317`), with a one-time
  HTTP sweep fallback if multicast finds nothing.
- Send files and folders by picking them, dropping them on the window, or using the
  `Send Folder…` action.
- Send a text message from the compose dialog (with a Paste button) without turning it
  into a file.
- Receive files with an accept/decline dialog, live progress and an **Open Folder**
  action when the transfer completes.
- Receive LocalSend text messages in a selectable, copyable dialog without saving
  them as files.
- Optional PIN protection on the receiving side; the sender is asked for the PIN only when
  the remote device requires it.
- Encrypted transfers (HTTPS with a self-signed certificate) by default, verified against
  the peer's LocalSend certificate fingerprint. Plain HTTP is still supported for peers
  that announce it.
- Native Adwaita patterns: overlay split view, status pages, header bar, toasts,
  preferences dialog, shortcuts window and an About dialog.
- No telemetry and no network access outside the local network.

## Requirements

- Rust 1.86 or newer
- GTK 4.10 or newer
- libadwaita 1.6 or newer
- `glib-compile-resources` (from GLib)
- A C compiler and pkg-config

On Fedora:

```sh
sudo dnf install gtk4-devel libadwaita-devel glib2-devel gcc pkg-config
```

On Debian/Ubuntu:

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev libglib2.0-dev-bin build-essential pkg-config
```

## Build and run

```sh
cargo build --release
./target/release/localsend-adwaita
```

Or, for a quick debug run:

```sh
cargo run
```

The application stores its settings and TLS identity in
`~/.config/localsend-adwaita/`, and received files in `~/Downloads/LocalSend` by default
(both are configurable; the download folder can be changed in Settings).

## Flatpak

The manifest lives in `flatpak/` and uses the GNOME 50 runtime with the Rust SDK
extension. Crate sources are vendored through `flatpak/cargo-sources.json`, so the build
itself does not need network access.

```sh
flatpak install flathub org.gnome.Platform//50 org.gnome.Sdk//50
flatpak-builder --user --install --force-clean \
    --install-deps-from=flathub \
    flatpak/build flatpak/io.github.localsend_adwaita.LocalSend.json
flatpak run io.github.localsend_adwaita.LocalSend
```

`cargo-sources.json` was generated from `Cargo.lock`. After changing dependencies,
regenerate it before rebuilding the Flatpak.

The sandbox is granted network access (required for discovery and transfers) and
`xdg-download` for storing received files. Keep the save location inside the Downloads
folder when running as a Flatpak, since no other host folders are exposed to the app.

## LocalSend protocol compatibility

Implemented protocol version: **2.2** (compatible with LocalSend 2.x peers).

| Area | Support |
| --- | --- |
| Discovery | UDP multicast announcement and response, `POST /api/localsend/v2/register` |
| Device info | `GET /api/localsend/v2/info` |
| Upload API | `prepare-upload`, streaming `upload`, `cancel` |
| HTTPS | Self-signed certificate, SHA-256 fingerprint verification, client certificate |
| PIN | Query parameter on `prepare-upload` |
| Folders | Sent as files whose names carry the relative path |
| Reverse transfer | Not advertised (`download: false`) |

The application is intentionally a single receiving session at a time, matching the
protocol's `409 Blocked by another session` behaviour. Incoming file names are sanitized:
absolute paths, `..` components and control characters are rejected.

## Project layout

```
Cargo.toml                  Rust package
build.rs                    GResource compilation for the app icon
src/application.rs          AdwApplication setup, resources and CSS
src/window.rs               Complete Adwaita UI and event handling
src/core.rs                 Shared state, devices, sessions and UI events
src/protocol.rs             LocalSend DTOs and path sanitization
src/discovery.rs            Multicast discovery, register and HTTP scan
src/server.rs               LocalSend receiver (axum + rustls)
src/sender.rs               Outgoing transfers and progress reporting
src/tls.rs                  Certificate identity and fingerprint-pinned TLS
src/net.rs                  HTTP client helpers
src/config.rs               JSON settings in XDG config
src/util.rs                 Formatting and icon helpers
data/                       Icon, CSS, desktop file and AppStream metadata
flatpak/                    Flatpak manifest and vendored Cargo sources
```

## Limitations

- The reverse transfer (download) API is not implemented; `download` is announced as
  `false`, which is valid for protocol 2.2.
- Protocol v1 peers are not supported.
- A peer must be reachable on the same local network; there is no relay or internet mode.
- Changing the port or encryption mode takes effect after restarting the app.

## License

GPL-3.0-or-later. See [COPYING](COPYING).

This is an unofficial GNOME client. LocalSend is developed by the LocalSend project; see
<https://localsend.org> for the upstream apps and protocol documentation.

# LocalSend for GNOME

An unofficial native GNOME client for [LocalSend](https://localsend.org). It speaks the
LocalSend protocol directly, so it exchanges files, folders and text messages with the
official LocalSend apps on Android, iOS, Windows, macOS and Linux — no account, no cloud,
no internet connection.

Rust · GTK 4 · libadwaita · protocol 2.2

![Application icon](data/icons/scalable/apps/io.github.localsend_adwaita.LocalSend.svg)

## Install

Every stable release ships a `.flatpak` bundle compiled by CI, so you never have to build
anything yourself.

### Flatpak bundle (recommended)

```sh
flatpak install --user --from \
  https://github.com/bazixv13/localsend-adwaita/releases/latest/download/localsend-adw.flatpak
flatpak run io.github.localsend_adwaita.LocalSend
```

`--from` accepts a URL directly in current Flatpak versions. If yours is older, download
the bundle first:

```sh
curl -L -o localsend.flatpak \
  https://github.com/bazixv13/localsend-adwaita/releases/latest/download/localsend-adw.flatpak
flatpak install --user ./localsend.flatpak
```

The bundle needs the GNOME 50 runtime; Flatpak offers to install it from Flathub if it is
missing.

### With automatic updates (Flatpak remote)

Install from the bazixv13 Flatpak remote instead and the app updates like any other
Flatpak app:

```sh
flatpak remote-add --if-not-exists --from bazixv13-apps \
  https://bazixv13.github.io/flatpak-repo/flatpakrepo
flatpak install bazixv13-apps io.github.localsend_adwaita.LocalSend
```

Afterwards `flatpak update` (or GNOME Software) picks up new versions automatically.

### Without Flatpak (release binary)

The same release also carries a plain tarball. This one paste installs it into `~/.local`:

```sh
mkdir -p /tmp/ls ~/.local/bin ~/.local/share/applications ~/.local/share/metainfo ~/.local/share/icons/hicolor/scalable/apps \
  && curl -L https://github.com/bazixv13/localsend-adwaita/releases/latest/download/localsend-adwaita-x86_64.tar.gz | tar -xz -C /tmp/ls \
  && install -m755 /tmp/ls/localsend-adwaita ~/.local/bin/ \
  && install -m644 /tmp/ls/io.github.localsend_adwaita.LocalSend.desktop ~/.local/share/applications/ \
  && install -m644 /tmp/ls/io.github.localsend_adwaita.LocalSend.metainfo.xml ~/.local/share/metainfo/ \
  && install -m644 /tmp/ls/io.github.localsend_adwaita.LocalSend.svg ~/.local/share/icons/hicolor/scalable/apps/ \
  && rm -rf /tmp/ls \
  && update-desktop-database ~/.local/share/applications 2>/dev/null; localsend-adwaita
```

`~/.local/bin` must be on your `PATH`. Uninstall by deleting those four files.

### From source (one paste)

Needs Rust 1.86+, GTK 4.10+, libadwaita 1.6+, `glib-compile-resources`, a C compiler and
pkg-config.

```sh
# Fedora / Bazzite / Silverblue toolbox
sudo dnf install gtk4-devel libadwaita-devel glib2-devel gcc gcc-c++ pkgconf-pkg-config cargo

# Debian / Ubuntu (Ubuntu 25.04+ ships libadwaita 1.6)
sudo apt install libgtk-4-dev libadwaita-1-dev libglib2.0-dev-bin build-essential pkg-config cargo
```

Then clone, build and install your own copy:

```sh
git clone https://github.com/bazixv13/localsend-adwaita && cd localsend-adwaita \
  && cargo install --path . --locked && localsend-adwaita
```

To build the Flatpak locally instead, regenerate `flatpak/cargo-sources.json` after
dependency changes (`python3 scripts/regen_cargo_sources.py`) and run:

```sh
flatpak install flathub org.gnome.Platform//50 org.gnome.Sdk//50 \
  && flatpak-builder --user --install --force-clean --install-deps-from=flathub \
       flatpak/build flatpak/io.github.localsend_adwaita.LocalSend.json
```

Keep the save location inside Downloads when running as a Flatpak, since only
`xdg-download` and network access are granted to the sandbox.

## Features

- Automatic device discovery over UDP multicast (`224.0.0.167:53317`), with a one-time
  HTTP sweep if multicast finds nothing.
- Send files and folders by picking, dragging onto the window, or queueing for later;
  send text as a message instead of a file.
- Receive with accept/decline, live progress, Quick Save mode, tray icon and native
  notifications (progress updates reuse a single low-priority notification).
- Optional PIN protection; the PIN is requested from the sender only when needed.
- HTTPS with a self-signed certificate, verified against the peer's LocalSend
  fingerprint; plain HTTP is still supported for peers that announce it.
- Adwaita UI throughout: overlay split view, status pages, preferences, shortcuts and an
  About dialog. No telemetry and no traffic beyond the local network.
- Transfer history with per-file actions (open, copy path, delete from disk), capped at
  100 transfers with cached-copy eviction.

## Releases

Everything in the Releases tab is built by CI — no manual uploading.

| Channel | Trigger | Published as |
| --- | --- | --- |
| Stable | tag `v*` (for example `v0.3.37`) | Release marked **Latest**: `localsend-adw.flatpak` + `x86_64` tarballs |
| Nightly | every push to `nightly` | Release tag `nightly`, marked **Pre-release**, same assets |
| `main` | every push | build artifacts under Actions (not published) |

Nightly always stays a pre-release, so the stable release remains the default download and
the Flatpak remote keeps serving stable builds.

## Protocol compatibility

Implemented protocol version: **2.2** (LocalSend 2.x peers).

| Area | Support |
| --- | --- |
| Discovery | UDP multicast announcement/response, `POST /api/localsend/v2/register` |
| Device info | `GET /api/localsend/v2/info` |
| Upload API | `prepare-upload`, streamed `upload`, `cancel`, skip files the receiver already has |
| HTTPS | Self-signed certificate, SHA-256 fingerprint pinning, optional client certificate |
| PIN | `pin` query parameter on `prepare-upload` |
| Folders | Sent as files whose names carry the relative path |
| Reverse transfer | Not advertised (`download: false`) |

One receiving session at a time, matching the protocol's `409 Blocked by another session`.
Incoming names are sanitised: absolute paths, `..` components and control characters are
rejected.

## Development

```sh
cargo fmt
cargo check --locked
cargo test
cargo run                       # debug build
scripts/check-runtime.sh --fast # protocol + notification checks against a running app
```

`src/window.rs` holds the UI, `src/core.rs` the shared state and event bus,
`src/protocol.rs` the DTOs and path sanitising, `src/server.rs` the receiver,
`src/sender.rs` the outgoing transfers, `src/discovery.rs` multicast discovery and
`src/tls.rs` the certificate identity.

## License and attribution

Copyright © the localsend-adwaita contributors. Licensed under the
**GNU General Public License v3.0 or later** — see [COPYING](COPYING).

This project is an independent Rust/GTK reimplementation of the LocalSend protocol. It
contains no source code from the upstream [LocalSend](https://github.com/localsend/localsend)
project, which is licensed under Apache-2.0 and documents the protocol at
<https://github.com/localsend/protocol>. The LocalSend name and logo belong to the
LocalSend project; this client is not affiliated with or endorsed by it. Third-party Rust
crates are used under their respective licenses (see `Cargo.lock`). See [NOTICE](NOTICE)
for details.

#!/usr/bin/env bash
# Installs the GNOME Files ("Send with LocalSend") integration for the current
# user, together with the desktop entry the menu item launches.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
extensions="$data_dir/nautilus-python/extensions"

if ! python3 - <<'PY' >/dev/null 2>&1
import glob

dirs = (
    "/usr/lib64/girepository-1.0",
    "/usr/lib/girepository-1.0",
    "/usr/lib/x86_64-linux-gnu/girepository-1.0",
)
found = [p for directory in dirs for p in glob.glob(directory + "/Nautilus-*.typelib")]
raise SystemExit(0 if found else 1)
PY
then
    echo "The Nautilus Python bindings were not found." >&2
    echo "Install them first: sudo apt install nautilus-python" >&2
    echo "                or: sudo dnf install nautilus-python" >&2
    exit 1
fi

install -Dm644 "$root/data/io.github.localsend_adwaita.LocalSend.desktop" \
    "$data_dir/applications/io.github.localsend_adwaita.LocalSend.desktop"
install -Dm644 "$root/data/icons/scalable/apps/io.github.localsend_adwaita.LocalSend.svg" \
    "$data_dir/icons/hicolor/scalable/apps/io.github.localsend_adwaita.LocalSend.svg"
install -Dm644 "$root/data/nautilus/localsend_adwaita.py" \
    "$extensions/localsend_adwaita.py"

# Nautilus loads extensions at startup only.
nautilus -q >/dev/null 2>&1 || true

echo "Installed the LocalSend menu item for GNOME Files."
echo "Restart Files if the entry does not appear."

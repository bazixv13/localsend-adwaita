"""Adds "Send with LocalSend" to the GNOME Files context menu.

Install it with scripts/install-nautilus-extension.sh, which needs the Nautilus
Python bindings (nautilus-python) and the app's desktop entry.
"""

import glob
import json
import os
import shutil

import gi

APP_ID = "io.github.localsend_adwaita.LocalSend"
ITEM_NAME = "LocalSendAdwaita::send"
LABEL = "Send with LocalSend"

# The app writes its settings as JSON; the extension runs outside the sandbox,
# so it can read both a native install's config and a Flatpak one.
_CONFIG_PATHS = (
    os.path.expanduser("~/.config/localsend-adwaita/config.json"),
    os.path.expanduser("~/.var/app/" + APP_ID + "/config/localsend-adwaita/config.json"),
)

# The introspection typelib is versioned per Nautilus release (4.0, 4.1, ...),
# so pick whichever one the system actually ships instead of guessing.
_TYPELIB_DIRS = (
    "/usr/lib64/girepository-1.0",
    "/usr/lib/girepository-1.0",
    "/usr/lib/x86_64-linux-gnu/girepository-1.0",
    "/app/lib/girepository-1.0",
)


def _integration_enabled():
    """Honours the "Send with LocalSend" switch in the app's settings."""
    candidates = list(_CONFIG_PATHS)
    config_home = os.environ.get("XDG_CONFIG_HOME")
    if config_home:
        candidates.insert(0, os.path.join(config_home, "localsend-adwaita", "config.json"))
    # A native and a Flatpak install keep separate configs; use whichever was
    # written last so the switch follows the app the user actually runs.
    newest = None
    for path in candidates:
        try:
            modified = os.path.getmtime(path)
        except OSError:
            continue
        if newest is None or modified > newest[0]:
            newest = (modified, path)
    if newest is None:
        return True
    try:
        with open(newest[1], encoding="utf-8") as handle:
            return bool(json.load(handle).get("files_integration", True))
    except (OSError, ValueError):
        return True


def _typelib_versions():
    versions = set()
    for directory in _TYPELIB_DIRS:
        for path in glob.glob(os.path.join(directory, "Nautilus-*.typelib")):
            name = os.path.basename(path)
            versions.add(name[len("Nautilus-") : -len(".typelib")])
    return sorted(versions, reverse=True)


for _version in _typelib_versions():
    try:
        gi.require_version("Nautilus", _version)
    except ValueError:
        continue
    break

from gi.repository import Gio, GObject, Nautilus  # noqa: E402

try:  # GLib 2.80 moved the desktop entry API out of Gio.
    from gi.repository import GioUnix

    _DesktopAppInfo = GioUnix.DesktopAppInfo
except (ImportError, ValueError):
    _DesktopAppInfo = Gio.DesktopAppInfo


class LocalSendMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    def _send(self, _item, files):
        paths = []
        for file in files:
            path = file.get_location().get_path()
            if path:
                paths.append(path)
        if not paths:
            return

        # Launching the desktop entry works for both a native install and a
        # Flatpak one, and lets GIO hand the files over as arguments.
        app = _DesktopAppInfo.new(APP_ID + ".desktop")
        if app is not None:
            app.launch([Gio.File.new_for_path(path) for path in paths], None)
            return

        binary = shutil.which("localsend-adwaita")
        if binary is not None:
            Gio.Subprocess.new([binary, *paths], Gio.SubprocessFlags.NONE)
            return

        Gio.Subprocess.new(
            ["flatpak", "run", APP_ID, *paths], Gio.SubprocessFlags.NONE
        )

    def get_file_items(self, *args):
        # Nautilus 43+ passes the selection only; older versions pass a window
        # first. The selection is always the last argument.
        files = args[-1]
        if not files or not _integration_enabled():
            return []
        item = Nautilus.MenuItem(
            name=ITEM_NAME,
            label=LABEL,
            tip="Send the selection to a nearby device with LocalSend",
        )
        item.connect("activate", self._send, files)
        return [item]

    def get_background_items(self, *args):
        return []

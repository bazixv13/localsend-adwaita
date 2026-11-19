"""Adds "Send with LocalSend" to the GNOME Files context menu.

Install it with scripts/install-nautilus-extension.sh, which needs the Nautilus
Python bindings (nautilus-python) and the app's desktop entry.
"""

from gi.repository import Gio, GObject, Nautilus

APP_ID = "io.github.localsend_adwaita.LocalSend"
ITEM_NAME = "LocalSendAdwaita::send"
LABEL = "Send with LocalSend"


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
        app = Gio.DesktopAppInfo.new(APP_ID + ".desktop")
        if app is None:
            return
        app.launch([Gio.File.new_for_path(path) for path in paths], None)

    def get_file_items(self, *args):
        # Nautilus 43+ passes the selection only; older versions pass a window
        # first. The selection is always the last argument.
        files = args[-1]
        if not files:
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

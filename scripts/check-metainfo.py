#!/usr/bin/env python3
"""Checks that the crate version and the newest AppStream release agree.

The version is written in `Cargo.toml`, in the Flatpak manifest's checkout and
in the metainfo release list; this catches the two that can silently drift.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO = ROOT / "Cargo.toml"
METAINFO = ROOT / "data" / "io.github.localsend_adwaita.LocalSend.metainfo.xml"


def main() -> int:
    cargo = CARGO.read_text(encoding="utf-8")
    version = re.search(r'^version = "([^"]+)"', cargo, re.MULTILINE)
    if version is None:
        print("Cargo.toml has no package version", file=sys.stderr)
        return 1
    crate_version = version.group(1)

    metainfo = METAINFO.read_text(encoding="utf-8")
    release = re.search(r'<release version="([^"]+)" date="(\d{4}-\d{2}-\d{2})"', metainfo)
    if release is None:
        print("the metainfo has no release entry", file=sys.stderr)
        return 1
    release_version, release_date = release.groups()

    if crate_version != release_version:
        print(
            f"version mismatch: Cargo.toml has {crate_version}, "
            f"the newest metainfo release is {release_version}",
            file=sys.stderr,
        )
        return 1
    print(f"version {crate_version} matches the {release_date} release entry")
    return 0


if __name__ == "__main__":
    sys.exit(main())

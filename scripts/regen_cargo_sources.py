#!/usr/bin/env python3
"""Regenerate flatpak/cargo-sources.json from Cargo.lock.

The Flatpak build vendors every crates.io package up front and builds with
`cargo --offline --locked`, so the source list has to match the lockfile
exactly: one archive per package plus the checksum manifest the manifest's
build commands write `.cargo-checksum.json` files from.
"""
import json
import tomllib
from pathlib import Path

lock_path = Path("Cargo.lock")
sources_path = Path("flatpak/cargo-sources.json")
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"

# Tells cargo where the vendored sources are instead of the network.
CARGO_CONFIG = (
    "[source.vendored-sources]\n"
    'directory = "cargo/vendor"\n'
    "\n"
    "[source.crates-io]\n"
    'replace-with = "vendored-sources"\n'
)

lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))

packages = []
for package in lock.get("package", []):
    if package.get("source") != CRATES_IO:
        continue
    name, version, checksum = package["name"], package["version"], package.get("checksum")
    if not checksum:
        raise SystemExit(f"{name} {version} comes from crates.io without a checksum")
    packages.append((name, version, checksum))

packages.sort(key=lambda p: (p[0], p[1]))

sources = [
    {
        "type": "inline",
        "contents": CARGO_CONFIG,
        "dest": "cargo",
        "dest-filename": "config",
    }
]
for name, version, checksum in packages:
    sources.append(
        {
            "type": "archive",
            "archive-type": "tar-gzip",
            "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
            "sha256": checksum,
            "dest": f"cargo/vendor/{name}-{version}",
            "strip-components": 1,
        }
    )
sources.append(
    {
        "type": "inline",
        "contents": "".join(f"{name}-{version} {checksum}\n" for name, version, checksum in packages),
        "dest": "cargo",
        "dest-filename": "vendor-checksums.txt",
    }
)

text = json.dumps(sources, indent=4) + "\n"
previous = sources_path.read_text(encoding="utf-8") if sources_path.exists() else ""
if text != previous:
    sources_path.write_text(text, encoding="utf-8")
    print(f"Updated {sources_path}: {len(packages)} crates")
else:
    print(f"{sources_path} is up to date: {len(packages)} crates")

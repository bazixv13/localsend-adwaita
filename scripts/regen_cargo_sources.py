from pathlib import Path
import json

cargo_lock_path = Path("Cargo.lock")
sources_path = Path("flatpak/cargo-sources.json")

cargo_lock_text = cargo_lock_path.read_text(encoding="utf-8")

packages = []
current = None
current_deps = []
in_deps = False
for line in cargo_lock_text.splitlines():
    if line.startswith("[[package]]"):
        if current is not None:
            current["dependencies"] = current_deps
            packages.append(current)
        current = {}
        current_deps = []
        in_deps = False
    elif current is not None:
        if "=" in line:
            key, _, value = line.partition("=")
            key, value = key.strip(), value.strip()
            if key == "dependencies":
                in_deps = True
                current[key] = None
            else:
                in_deps = False
                current[key] = value
        elif in_deps and line.strip():
            dep = line.strip().split()[0]
            current_deps.append(dep)
if current is not None:
    current["dependencies"] = current_deps
    packages.append(current)

packages_by_name = {p["name"]: p for p in packages if "name" in p}

def is_crates_io(pkg):
    src = pkg.get("source", "") or ""
    return "registry+https://github.com/rust-lang/crates.io-index" in src

def entry_for(name):
    pkg = packages_by_name.get(name)
    if not pkg or not is_crates_io(pkg):
        return None
    ver = pkg.get("version")
    checksum = pkg.get("checksum")
    if not ver or not checksum:
        return None
    return {
        "name": name,
        "version": ver,
        "dl_path": f"https://static.crates.io/crates/{name}/{name}-{ver}.crate",
        "hash": checksum,
        "dest": f"cargo/vendor/{name}-{ver}",
    }

needed_names = ["ksni", "zbus", "async-io", "async-lock"]
extra = set()
for name in needed_names:
    pkg = packages_by_name.get(name)
    if not pkg:
        continue
    for dep in pkg.get("dependencies", []):
        extra.add(dep)

entries = []
seen = set()
for name in sorted(needed_names + list(extra)):
    e = entry_for(name)
    if not e:
        continue
    key = (e["name"], e["version"])
    if key in seen:
        continue
    seen.add(key)
    entries.append(e)

if sources_path.exists():
    root = json.loads(sources_path.read_text(encoding="utf-8"))
else:
    root = {"sources": []}

if not isinstance(root, dict):
    root = {"sources": []}

existing_sources = root.get("sources") or []
if not isinstance(existing_sources, list):
    existing_sources = [existing_sources]

# Keep existing vendored sources (and the cargo config inline source if present).
existing_map = {}
for item in existing_sources:
    key = (item.get("name"), item.get("version"))
    if key:
        existing_map[key] = item

merged_sources = []
for item in existing_sources:
    key = (item.get("name"), item.get("version"))
    if key and key in existing_map:
        existing_map.pop(key, None)
        merged_sources.append(item)
    else:
        merged_sources.append(item)

for e in entries:
    key = (e["name"], e["version"])
    if key not in existing_map:
        merged_sources.append(e)

root["sources"] = merged_sources
sources_path.write_text(json.dumps(root, indent=2) + "\n", encoding="utf-8")

print(f"Updated {sources_path}: {len(merged_sources)} sources")
for e in merged_sources:
    print(" -", e.get("name"), e.get("version"))

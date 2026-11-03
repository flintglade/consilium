#!/usr/bin/env python3
"""Regenerate packaging/flatpak/cargo-sources.json from Cargo.lock, with no network access.

Output matches Flathub's flatpak-builder-tools Cargo generator for registry crates. Run this and
packaging/generate-third-party-notices.sh after any Cargo.lock change; CI checks both.
"""
import json
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONFIG = '[source.vendored-sources]\ndirectory = "cargo/vendor"\n\n[source.crates-io]\nreplace-with = "vendored-sources"\n'

sources = []
for package in tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]:
    if not package.get("source", "").startswith("registry+"):
        continue
    name, version, checksum = package["name"], package["version"], package["checksum"]
    dest = f"cargo/vendor/{name}-{version}"
    sources.append({
        "type": "archive",
        "archive-type": "tar-gzip",
        "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
        "sha256": checksum,
        "dest": dest,
    })
    sources.append({
        "type": "inline",
        "contents": json.dumps({"package": checksum, "files": {}}),
        "dest": dest,
        "dest-filename": ".cargo-checksum.json",
    })
sources.append({"type": "inline", "contents": CONFIG, "dest": "cargo", "dest-filename": "config"})
(ROOT / "packaging/flatpak/cargo-sources.json").write_text(json.dumps(sources, indent=4))

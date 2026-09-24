#!/usr/bin/env python3
"""Build a release archive from an explicit allowlist; never package the checkout wholesale."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import tarfile
import tempfile
import tomllib
import zipfile

root = Path(__file__).resolve().parents[1]
p = argparse.ArgumentParser()
p.add_argument("--target", required=True, choices=["linux-x64", "linux-arm64", "macos-arm64", "windows-x64"])
a = p.parse_args()
meta = tomllib.loads((root / "Cargo.toml").read_text())["package"]
name, version = meta["name"], meta["version"]
exe = name + (".exe" if a.target.startswith("windows") else "")
source = root / "target" / "release" / exe
if not source.is_file():
    raise SystemExit(f"Build the target executable first: {source}")
config = "sirius-api-config.example.yaml" if name == "sirius-api-proxy" else "sirius-asset-config.example.yaml"
dist = root / "dist"
dist.mkdir(exist_ok=True)
stem = f"{name}-{version}-{a.target}"
with tempfile.TemporaryDirectory() as tmp:
    stage = Path(tmp) / stem
    stage.mkdir()
    shutil.copy2(source, stage / exe)
    for filename in ["README.md", "CHANGELOG.md", config]:
        shutil.copy2(root / filename, stage / filename)
    for license_file in root.glob("LICENSE*"):
        shutil.copy2(license_file, stage / license_file.name)
    shutil.copytree(root / "docs", stage / "docs")
    if name == "sirius-api-proxy":
        shutil.copytree(root / "protocol", stage / "protocol")
    else:
        shutil.copy2(root / "export-config.example.yaml", stage / "export-config.example.yaml")
    manifest = {"name": name, "version": version, "target": a.target, "files": {}}
    for item in sorted(stage.rglob("*")):
        if item.is_symlink():
            raise SystemExit("Release files must not be symlinks")
        if item.is_file():
            manifest["files"][item.relative_to(stage).as_posix()] = hashlib.sha256(item.read_bytes()).hexdigest()
    (stage / "release-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    if a.target.startswith("windows"):
        archive = dist / (stem + ".zip")
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
            for item in sorted(stage.rglob("*")):
                if item.is_file():
                    z.write(item, item.relative_to(stage.parent))
    else:
        archive = dist / (stem + ".tar.gz")
        with tarfile.open(archive, "w:gz") as t:
            t.add(stage, arcname=stem)
    (dist / (archive.name + ".sha256")).write_text(hashlib.sha256(archive.read_bytes()).hexdigest() + "  " + archive.name + "\n")
print(archive)

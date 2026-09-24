#!/usr/bin/env python3
"""Extract and verify an archive, then run offline startup/authentication checks."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import zipfile

p = argparse.ArgumentParser()
p.add_argument("archive", type=Path)
a = p.parse_args()
with tempfile.TemporaryDirectory() as tmp:
    dest = Path(tmp)
    if a.archive.suffix == ".zip":
        with zipfile.ZipFile(a.archive) as z:
            z.extractall(dest)
    else:
        with tarfile.open(a.archive) as t:
            t.extractall(dest, filter="data")
    folders = list(dest.iterdir())
    assert len(folders) == 1 and folders[0].is_dir()
    root = folders[0]
    m = json.loads((root / "release-manifest.json").read_text())
    actual = {f.relative_to(root).as_posix() for f in root.rglob("*") if f.is_file()}
    assert actual == set(m["files"]) | {"release-manifest.json"}
    for f, digest in m["files"].items():
        assert hashlib.sha256((root / f).read_bytes()).hexdigest() == digest, f
    assert not any(f.startswith(("downloads/", "exports/", "tests/", ".git/")) for f in actual)
    exe = root / (m["name"] + (".exe" if os.name == "nt" else ""))
    env = os.environ.copy()
    for k in list(env):
        if k.startswith("SIRIUS_"):
            del env[k]
    env.update(SIRIUS_API_TOKEN="smoke-public-token", SIRIUS_INTERNAL_TOKEN="smoke-internal-token",
               SIRIUS_CDN_USERNAME="smoke-user", SIRIUS_CDN_CREDENTIAL="smoke-password")
    version = subprocess.run([str(exe), "--version"], cwd=root, env=env, check=True, capture_output=True, timeout=15)
    assert version.stdout.decode().strip() == m["name"] + " " + m["version"]
    if m["name"] == "sirius-asset-updater":
        (root / "sirius-asset-config.yaml").write_text((root / "sirius-asset-config.example.yaml").read_text())
        subprocess.run([str(exe), "--help"], cwd=root, env=env, check=True, capture_output=True, timeout=15)
        check = subprocess.run([str(exe), "check"], cwd=root, env=env, check=True, capture_output=True, timeout=15)
        assert json.loads(check.stdout)["ready"]
    else:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        config = (root / "sirius-api-config.example.yaml").read_text().replace("127.0.0.1:9999", f"127.0.0.1:{port}")
        (root / "sirius-api-config.yaml").write_text(config)
        proc = subprocess.Popen([str(exe)], cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        def request(path, token=None):
            req = urllib.request.Request(f"http://127.0.0.1:{port}" + path)
            if token:
                req.add_header("Authorization", "Bearer " + token)
            try:
                with urllib.request.urlopen(req, timeout=2) as response:
                    return response.status, json.load(response)
            except urllib.error.HTTPError as error:
                return error.code, None
        try:
            for _ in range(100):
                if proc.poll() is not None:
                    raise RuntimeError(proc.stderr.read().decode())
                try:
                    code, health = request("/health")
                    break
                except (OSError, urllib.error.URLError):
                    time.sleep(0.1)
            else:
                raise RuntimeError("Packaged server did not start")
            assert code == 200 and health["version"] == m["version"]
            assert request("/internal/v1/protocol")[0] == 401
            assert request("/internal/v1/protocol", env["SIRIUS_API_TOKEN"])[0] == 401
            code, protocol = request("/internal/v1/protocol", env["SIRIUS_INTERNAL_TOKEN"])
            assert code == 200 and protocol["codec"] == "native"
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
    print(f"Archive hashes, runtime files and offline startup passed: {m['name']} {m['version']} ({m['target']})")

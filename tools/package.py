#!/usr/bin/env python3
"""Create the source-only .hnmod bundle consumed by Horizon's native mod launcher."""
import hashlib
import json
from pathlib import Path
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
GAME = "26.3"

def package():
    compat = json.loads((ROOT / "compatibility.json").read_text())
    manifests = sorted((ROOT / "mixins").glob("*/horizon-mixin.toml"))
    if len(manifests) != 1:
        raise SystemExit("Expected exactly one mixins/<id>/horizon-mixin.toml")
    bundle = manifests[0].parent
    mixin = tomllib.loads(manifests[0].read_text())
    mod = mixin["mod"]
    if bundle.name != mod["id"]:
        raise SystemExit("Bundle directory must match the mod id")
    if mixin.get("compatibility", {}).get("horizon_revision") != compat["horizon_revision"]:
        raise SystemExit("Manifest and compatibility.json Horizon revisions differ")
    manifest = {
        "schema": 1, "id": mod["id"], "version": mod["version"], "game": GAME,
        "horizon": compat["horizon_revision"], "mixins": compat["horizon_mixins_revision"],
    }
    files = {"horizon-native.json": (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()}
    for path in sorted(bundle.iterdir()):
        if path.is_file() and path.suffix in (".rs", ".toml"):
            files[path.relative_to(ROOT).as_posix()] = path.read_bytes()
    out = ROOT / "target" / f'{mod["id"]}-{mod["version"]}.hnmod'
    out.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in sorted(files.items()):
            info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            archive.writestr(info, data)
    digest = hashlib.sha256(out.read_bytes()).hexdigest()
    print(f"Created {out} ({out.stat().st_size} bytes, SHA-256 {digest})")

if __name__ == "__main__":
    package()

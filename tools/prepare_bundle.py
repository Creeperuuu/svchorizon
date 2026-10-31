#!/usr/bin/env python3
"""Stage the native bundle's vendored engine sources for horizon-mixins."""
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "engine"
BUNDLE = ROOT / "mixins" / "svchorizon"
DEST = BUNDLE / "engine"

def main():
    if not (BUNDLE / "horizon-mixin.toml").is_file():
        raise SystemExit("Missing mixins/svchorizon/horizon-mixin.toml")
    if not (SOURCE / "src" / "lib.rs").is_file():
        raise SystemExit("Missing vendored engine/src/lib.rs")
    DEST.mkdir(parents=True, exist_ok=True)
    target = DEST / "src"
    if target.exists():
        shutil.rmtree(target)
    shutil.copytree(SOURCE / "src", target)
    shutil.copy2(SOURCE / "COPYING", DEST / "COPYING")
    print(f"Prepared native engine sources in {DEST}")

if __name__ == "__main__":
    main()

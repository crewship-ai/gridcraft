#!/usr/bin/env python3
"""Verify the vendored canonical SDK before any native browser build."""
import hashlib
import json
from pathlib import Path
root = Path(__file__).resolve().parents[2] / "apps/gridcraft-web/mooring"
pin = json.loads((root / "provenance.json").read_text())
for name, expected in pin["sources"].items():
    if Path(name).name != name:
        raise SystemExit("invalid SDK source path")
    if hashlib.sha256((root / "src" / name).read_bytes()).hexdigest() != expected:
        raise SystemExit(f"SDK source digest mismatch: {name}")
if hashlib.sha256((root / "native-sdk.js").read_bytes()).hexdigest() != pin["compiled_sha256"]:
    raise SystemExit("SDK compiled digest mismatch")
print(f"Shared editor SDK verified: {pin['commit']}")

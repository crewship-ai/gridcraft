#!/usr/bin/env python3
"""Write or check the bundle manifest (`manifest.json`) of a built web site.

Usage:
  bundle-manifest.py write <site_dir> <source_commit> <toolchain_json> <out_manifest>
  bundle-manifest.py check <bundle_dir> [<expected_digest>]

The manifest pins every file a host serves for one bundle, and the bundle is addressed by the
sha256 of the manifest's exact bytes (`/b/<digest>/index.html`). Its format is strict so that
every reader sees the same file set:

- top-level keys exactly `files`, `source_commit`, `toolchain` (no others, no case variants,
  no duplicates at any level);
- `files` maps a clean relative path (no leading `/`, no `.`/`..`/empty elements, none of
  `\\ ? # %` or NUL, never `manifest.json`) to the lowercase hex sha256 of that file;
- `index.html` is listed; 1 to 512 files, at most 64 MiB in total.

`write` emits sorted keys, two-space indentation and a final newline, so the bytes (and the
digest) depend only on the site files, the commit and the toolchain. It prints the digest.
`check` verifies a bundle directory laid out as served (`manifest.json` next to the files):
the format above, every file's digest, no unlisted files, and optionally the expected digest.
"""
import hashlib
import json
import pathlib
import posixpath
import re
import sys

KEYS = ("files", "source_commit", "toolchain")
HEX64 = re.compile(r"[0-9a-f]{64}")
MAX_FILES = 512
MAX_BYTES = 64 << 20


def fail(msg: str) -> "NoReturn":  # noqa: F821
    sys.exit(f"bundle-manifest: {msg}")


def clean_name(name: str) -> bool:
    if not name or name == "manifest.json" or name.startswith("/") or name.endswith("/"):
        return False
    if any(c in name for c in "\\\x00?#%"):
        return False
    parts = name.split("/")
    if any(p in ("", ".", "..") for p in parts):
        return False
    return posixpath.normpath(name) == name


def sha256(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def no_duplicates(pairs):
    seen = {}
    for k, v in pairs:
        if k in seen:
            fail(f"duplicate key {k!r}")
        seen[k] = v
    return seen


def site_files(root: pathlib.Path) -> dict:
    files = {}
    for p in sorted(root.rglob("*")):
        if p.is_symlink():
            fail(f"{p} is a symlink")
        if p.is_file():
            name = p.relative_to(root).as_posix()
            if name == "manifest.json":
                continue
            if not clean_name(name):
                fail(f"{name!r} is not a clean relative path")
            files[name] = sha256(p)
    return files


def check_files(files: dict, root: pathlib.Path) -> None:
    if not isinstance(files, dict) or not 1 <= len(files) <= MAX_FILES:
        fail(f"files must list between 1 and {MAX_FILES} files")
    if "index.html" not in files:
        fail("index.html is not listed")
    total = 0
    for name, digest in files.items():
        if not clean_name(name) or not isinstance(digest, str) or not HEX64.fullmatch(digest):
            fail(f"entry {name!r} is not a clean relative path with a lowercase sha256")
        total += (root / name).stat().st_size
    if total > MAX_BYTES:
        fail("bundle exceeds 64 MiB")


def write(site: pathlib.Path, commit: str, toolchain_json: str, out: pathlib.Path) -> int:
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        fail("source_commit must be a full lowercase git commit id")
    toolchain = json.loads(toolchain_json, object_pairs_hook=no_duplicates)
    if not isinstance(toolchain, dict) or not all(isinstance(v, str) for v in toolchain.values()):
        fail("toolchain must be an object of strings")
    files = site_files(site)
    check_files(files, site)
    manifest = {"files": files, "source_commit": commit, "toolchain": toolchain}
    raw = (json.dumps(manifest, indent=2, sort_keys=True, ensure_ascii=True) + "\n").encode()
    out.write_bytes(raw)
    print(hashlib.sha256(raw).hexdigest())
    return 0


def check(bundle: pathlib.Path, expected: str | None) -> int:
    raw = (bundle / "manifest.json").read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    if expected is not None and digest != expected:
        fail(f"manifest digest {digest} != expected {expected}")
    manifest = json.loads(raw, object_pairs_hook=no_duplicates)
    if not isinstance(manifest, dict) or set(manifest) - set(KEYS) or "files" not in manifest:
        fail(f"top-level keys must be a subset of {KEYS} and include files, got {sorted(manifest)}")
    files = manifest["files"]
    check_files(files, bundle)
    for name, want in files.items():
        if sha256(bundle / name) != want:
            fail(f"{name} does not match its manifest digest")
    unlisted = sorted(set(site_files(bundle)) - set(files))
    if unlisted:
        fail(f"files not in the manifest: {unlisted}")
    print(digest)
    return 0


def main(argv) -> int:
    if len(argv) == 6 and argv[1] == "write":
        return write(pathlib.Path(argv[2]), argv[3], argv[4], pathlib.Path(argv[5]))
    if len(argv) in (3, 4) and argv[1] == "check":
        return check(pathlib.Path(argv[2]), argv[3] if len(argv) == 4 else None)
    fail(__doc__.split("\n\n")[1])


if __name__ == "__main__":
    sys.exit(main(sys.argv))

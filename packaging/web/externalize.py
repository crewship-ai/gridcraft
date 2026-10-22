#!/usr/bin/env python3
"""Make trunk's index.html work under a strict CSP and a sandboxed (opaque-origin) frame.

Usage: externalize.py <site_dir> <csp_out>

- Moves trunk's inline module bootstrap (`<script type="module">…</script>`) into
  `gridcraft-boot-<hash>.js` and the inline `<style>` into `gridcraft-<hash>.css`, referenced with
  SRI (sha384) like trunk's own links. The page then needs no 'unsafe-inline' and no hashes.
- Refuses any remaining inline script, style element, style attribute or event handler.
- Writes the matching Content-Security-Policy (one line) to <csp_out>.

Deterministic: output depends only on the input files.
"""
import base64
import hashlib
import pathlib
import re
import sys


def sri(data: bytes) -> str:
    return "sha384-" + base64.b64encode(hashlib.sha384(data).digest()).decode()


def main() -> int:
    site = pathlib.Path(sys.argv[1])
    csp_out = pathlib.Path(sys.argv[2])
    index = site / "index.html"
    html = index.read_text(encoding="utf-8")

    def extract(pattern: str, prefix: str, ext: str, tag) -> None:
        nonlocal html
        found = list(re.finditer(pattern, html, flags=re.S))
        if len(found) != 1:
            sys.exit(f"externalize: expected exactly one match for {pattern!r}, found {len(found)}")
        m = found[0]
        body = m.group(1).strip("\n").encode() + b"\n"
        name = f"{prefix}-{hashlib.sha256(body).hexdigest()[:16]}.{ext}"
        (site / name).write_bytes(body)
        html = html[: m.start()] + tag(name, sri(body)) + html[m.end() :]

    extract(
        r"<script type=\"module\">(.*?)</script>",
        "gridcraft-boot",
        "js",
        lambda n, i: f'<script type="module" src="./{n}" integrity="{i}" crossorigin="anonymous"></script>',
    )
    extract(
        r"<style>(.*?)</style>",
        "gridcraft",
        "css",
        lambda n, i: f'<link rel="stylesheet" href="./{n}" integrity="{i}" crossorigin="anonymous">',
    )

    leftovers = [
        (r"<script(?![^>]*\ssrc=)[^>]*>", "inline <script>"),
        (r"<style[\s>]", "<style> element"),
        (r"\sstyle=", "style attribute"),
        (r"\son[a-z]+=", "inline event handler"),
    ]
    for pattern, what in leftovers:
        if re.search(pattern, html):
            sys.exit(f"externalize: {what} left in index.html")
    index.write_text(html, encoding="utf-8")

    # 'self' is the editor's own origin. Under `sandbox allow-scripts` (no allow-same-origin) the
    # document is opaque but CSP still matches 'self' against its URL's origin; the host adds
    # frame-ancestors for its own origin. The bundle's files must be served with
    # `Access-Control-Allow-Origin: *` in that case (module scripts and the wasm fetch are CORS
    # requests from origin "null").
    csp = "; ".join(
        [
            "default-src 'none'",
            "script-src 'self' 'wasm-unsafe-eval'",
            "style-src 'self'",
            "connect-src 'self'",
            "img-src 'self' data: blob:",
            "font-src 'self'",
            "base-uri 'none'",
            "form-action 'none'",
            "sandbox allow-scripts",
        ]
    )
    csp_out.write_text(csp + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())

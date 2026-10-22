#!/usr/bin/env bash
# Fetch the pinned web build tools (trunk, wasm-bindgen, wasm-opt) into TOOLS_DIR and verify them
# against packaging/web/toolchain.lock (tarball sha256 *and* extracted binary sha256).
# This and `cargo fetch --locked` are the only network steps of the reproducible web build.
#
# Usage: packaging/web/fetch-tools.sh <tools_dir>      (binaries land in <tools_dir>/bin)
# Re-running is cheap: verified tarballs in <tools_dir>/dl are reused.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=toolchain.lock
. "$HERE/toolchain.lock"

TOOLS_DIR="${1:?usage: fetch-tools.sh <tools_dir>}"
mkdir -p "$TOOLS_DIR/dl" "$TOOLS_DIR/bin"
TOOLS_DIR="$(cd "$TOOLS_DIR" && pwd)"

[ "$(uname -s)-$(uname -m)" = "Linux-x86_64" ] || { echo "error: pinned tools exist for Linux x86_64 only" >&2; exit 1; }

sha_of() { sha256sum "$1" | cut -d' ' -f1; }

fetch() { # url expected_sha256 -> path of verified tarball
  local url="$1" want="$2" out
  out="$TOOLS_DIR/dl/$(basename "$url")"
  if [ ! -f "$out" ] || [ "$(sha_of "$out")" != "$want" ]; then
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$out.part" "$url"
    mv -f "$out.part" "$out"
  fi
  local got; got="$(sha_of "$out")"
  [ "$got" = "$want" ] || { echo "error: sha256 mismatch for $url: got $got, want $want" >&2; exit 1; }
  printf '%s\n' "$out"
}

install_bin() { # tarball member_path dest_name expected_bin_sha256
  local tarball="$1" member="$2" name="$3" want="$4" tmp
  tmp="$(mktemp -d "$TOOLS_DIR/x.XXXXXX")"
  tar -xzf "$tarball" -C "$tmp" "$member"
  install -m 0755 "$tmp/$member" "$TOOLS_DIR/bin/$name"
  rm -rf "$tmp"
  local got; got="$(sha_of "$TOOLS_DIR/bin/$name")"
  [ "$got" = "$want" ] || { echo "error: sha256 mismatch for $name binary: got $got, want $want" >&2; exit 1; }
}

t="$(fetch "$TRUNK_URL" "$TRUNK_TARBALL_SHA256")"
install_bin "$t" trunk trunk "$TRUNK_BIN_SHA256"

t="$(fetch "$WASM_BINDGEN_URL" "$WASM_BINDGEN_TARBALL_SHA256")"
install_bin "$t" "wasm-bindgen-$WASM_BINDGEN_VERSION-x86_64-unknown-linux-musl/wasm-bindgen" wasm-bindgen "$WASM_BINDGEN_BIN_SHA256"

t="$(fetch "$WASM_OPT_URL" "$WASM_OPT_TARBALL_SHA256")"
install_bin "$t" "binaryen-$WASM_OPT_VERSION/bin/wasm-opt" wasm-opt "$WASM_OPT_BIN_SHA256"

echo "pinned web tools verified in $TOOLS_DIR/bin"

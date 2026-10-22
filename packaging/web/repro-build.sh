#!/usr/bin/env bash
# Reproducible build of the browser bundle plus a digest manifest.
#
# Usage: packaging/web/repro-build.sh <out_dir>
#   env GRIDCRAFT_WEB_TOOLS  dir prepared by packaging/web/fetch-tools.sh (required)
#   env CARGO_TARGET_DIR     defaults to <repo>/target; use a fresh one for a clean build
#   env SOURCE_DATE_EPOCH    defaults to the HEAD commit time
#
# Output (all deterministic, nothing depends on the checkout path, user, clock or locale):
#   <out_dir>/site/                       trunk output (index.html, *.js, *_bg.wasm, icon)
#   <out_dir>/gridcraft-web-<rev>.tar     the site as a normalized tar (sorted, mtime=SDE, uid 0)
#   <out_dir>/build-info.json             commit, SOURCE_DATE_EPOCH, toolchain versions
#   <out_dir>/MANIFEST.sha256             sha256 of every file above (sha256sum format)
#
# Network: only `cargo fetch --locked`. trunk runs --offline --locked; cargo runs offline.
# Two builds of the same commit in different directories must produce an identical
# MANIFEST.sha256 (.github/workflows/web-repro.yml checks exactly that).
set -euo pipefail
export LC_ALL=C TZ=UTC
umask 022

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# shellcheck source=toolchain.lock
. "$HERE/toolchain.lock"

OUT="${1:?usage: repro-build.sh <out_dir>}"
TOOLS="${GRIDCRAFT_WEB_TOOLS:?set GRIDCRAFT_WEB_TOOLS (run packaging/web/fetch-tools.sh first)}"
TOOLS="$(cd "$TOOLS" && pwd)"
if [ -e "$OUT" ] && [ -n "$(ls -A "$OUT" 2>/dev/null)" ]; then
  echo "error: $OUT is not empty (a reproducible build starts from an empty output dir)" >&2; exit 1
fi
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

export PATH="$TOOLS/bin:$PATH"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
mkdir -p "$CARGO_TARGET_DIR"
CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"

cd "$ROOT"
REV="$(git rev-parse HEAD)"
DIRTY=false
[ -z "$(git status --porcelain --untracked-files=no)" ] || DIRTY=true
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct HEAD)}"

# --- verify the pinned toolchain (rustup resolves rust-toolchain.toml from $ROOT) ---------------
check() { [ "$2" = "$3" ] || { echo "error: $1 is '$2', pinned '$3'" >&2; exit 1; }; }
check rustc "$(rustc -V)" "$RUSTC_VERSION"
check rustc-commit "$(rustc -vV | sed -n 's/^commit-hash: //p')" "$RUSTC_COMMIT_HASH"
check trunk "$(trunk --version)" "trunk $TRUNK_VERSION"
check wasm-bindgen "$(wasm-bindgen --version)" "wasm-bindgen $WASM_BINDGEN_VERSION"
check trunk-bin "$(sha256sum "$TOOLS/bin/trunk" | cut -d' ' -f1)" "$TRUNK_BIN_SHA256"
check wasm-bindgen-bin "$(sha256sum "$TOOLS/bin/wasm-bindgen" | cut -d' ' -f1)" "$WASM_BINDGEN_BIN_SHA256"
check wasm-opt-bin "$(sha256sum "$TOOLS/bin/wasm-opt" | cut -d' ' -f1)" "$WASM_OPT_BIN_SHA256"
LOCK_WB="$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | sed -n 's/^version = "\(.*\)"$/\1/p')"
check "Cargo.lock wasm-bindgen" "$LOCK_WB" "$WASM_BINDGEN_VERSION"

# --- the only network step ----------------------------------------------------------------------
# All platforms, not just wasm32: trunk runs `cargo metadata`, which resolves every target's deps.
cargo fetch --locked

# --- offline, path-independent build ------------------------------------------------------------
# Remaps: later prefixes win, so the most specific (target dir, often inside the repo) comes last.
export RUSTFLAGS="--remap-path-prefix=$ROOT=/gridcraft --remap-path-prefix=$CARGO_HOME=/cargo --remap-path-prefix=$CARGO_TARGET_DIR=/target"
export CARGO_NET_OFFLINE=true CARGO_INCREMENTAL=0 CARGO_TERM_COLOR=never
export TRUNK_TOOLS_WASM_BINDGEN="$WASM_BINDGEN_VERSION" TRUNK_TOOLS_WASM_OPT="$WASM_OPT_VERSION"
(cd "$ROOT/apps/gridcraft-web" &&
  trunk build --offline --release --locked --dist "$OUT/site" --public-url ./ --filehash true)

# --- normalize and digest -----------------------------------------------------------------------
rm -f "$OUT/site/.trunk-lock"   # trunk's (empty) build lock, not part of the site
find "$OUT/site" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
TAR="gridcraft-web-${REV:0:12}.tar"
(cd "$OUT" && find site -print0 | sort -z |
  tar --null --no-recursion -T - --format=gnu --mtime="@$SOURCE_DATE_EPOCH" \
      --owner=0 --group=0 --numeric-owner --mode='u+rwX,go+rX,go-w' -cf "$TAR")

cat > "$OUT/build-info.json" <<JSON
{
  "commit": "$REV",
  "dirty": $DIRTY,
  "source_date_epoch": $SOURCE_DATE_EPOCH,
  "rustc": "$RUSTC_VERSION",
  "target": "wasm32-unknown-unknown",
  "trunk": "$TRUNK_VERSION",
  "wasm_bindgen": "$WASM_BINDGEN_VERSION",
  "wasm_opt": "$WASM_OPT_VERSION",
  "cargo_profile": "release",
  "tar": "$TAR"
}
JSON
touch -d "@$SOURCE_DATE_EPOCH" "$OUT/build-info.json" "$OUT/$TAR"

(cd "$OUT" && find site "$TAR" build-info.json -type f -print0 | sort -z | xargs -0 sha256sum > MANIFEST.sha256)
cat "$OUT/MANIFEST.sha256"

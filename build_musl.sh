#!/usr/bin/env bash
# Build mping as a stripped, statically linked musl binary into dist/.
# Output: dist/mping-v<version>-musl
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"

TARGET="$(uname -m)-unknown-linux-musl"
VERSION="$(sed -n '/^\[package\]/,/^\[/ s/^version *= *"\(.*\)"/\1/p' Cargo.toml)"
if [[ -z "$VERSION" ]]; then
    echo "error: could not read package version from Cargo.toml" >&2
    exit 1
fi

OUT_DIR="dist"
OUT="${OUT_DIR}/mping-v${VERSION}-musl"

echo "Building mping v${VERSION} for ${TARGET}..."
CARGO_PROFILE_RELEASE_STRIP=symbols \
RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static" \
    cargo build --release --locked --target "$TARGET" --bin mping

mkdir -p "$OUT_DIR"
cp "target/${TARGET}/release/mping" "$OUT"

echo "Built: ${OUT}"
file "$OUT" 2>/dev/null || true
ls -lh "$OUT"

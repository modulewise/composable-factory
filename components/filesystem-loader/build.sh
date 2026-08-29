#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

OUT="lib/filesystem-loader.wasm"

echo "==> Building filesystem-loader core module..."
cargo build --release --target wasm32-unknown-unknown

echo "==> Componentizing -> ${OUT}..."
wasm-tools component new \
  ../../target/wasm32-unknown-unknown/release/filesystem_loader.wasm \
  -o "$OUT"

echo ""
echo "==> Loader WIT:"
wasm-tools component wit "$OUT" | head -8

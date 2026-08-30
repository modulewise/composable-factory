#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

FACTORY="lib/calculator-factory.wasm"
OUT="${1:-lib/calculator.wasm}"

echo "==> Building calculator-factory core module..."
cargo build --release --target wasm32-unknown-unknown

CORE="../../target/wasm32-unknown-unknown/release/calculator_factory.wasm"

echo "==> Componentizing -> ${FACTORY}..."
wasm-tools component new "$CORE" -o "$FACTORY"

echo "==> Invoking calculator-factory.factory.build -> ${OUT} ..."
composable invoke "$FACTORY" -- calculator-factory.factory.build > "$OUT"

echo ""
echo "==> Produced calculator WIT:"
wasm-tools component wit "$OUT" | head -8
